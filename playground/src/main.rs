//! The playground: play Victris by hand against the bot's own brain, with no clock, and
//! rewind to any position to try something else.
//!
//! A small local web server. Every position comes from the exact engine port
//! (`victris-engine`), and the bot's side is decided by the same tracker and policy code
//! as the live bot (`marshal`), seeing only what the ladder would send it. Positions are
//! kept in a tree: rewinding is jumping to a node, and a different choice grows a new
//! branch while the old line stays there to go back to.
//!
//! It talks to nothing outside this machine and never touches the bots, the gateway or
//! their tokens.

use marshal::policy::{decide, Playbook};
use marshal::session::{turn_json, Playbooks};
use marshal::tracker::Tracker;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use victris_engine::events::{visible_events, Event};
use victris_engine::geom::{DIR_NAMES, P, SUB};
use victris_engine::observe::{blast_tiles, can_see, View};
use victris_engine::replay::parse_replay;
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::{Engine, State};

const WEB: &str = include_str!("../web/index.html");

/// Who chose a turn.
const BY_RECORDED: u8 = 0;
const BY_YOU: u8 = 1;
const BY_BOT: u8 = 2;

/// What the tree search held at its root when it chose.
#[derive(Clone)]
struct RootInfo {
    /// per side: (turn, visits, average result in hp for that side)
    acts: [Vec<(RawTurn, u32, f64)>; 2],
    /// average result of all iterations, for fighter 1
    value: f64,
    /// iterations carried over from the previous tick's tree
    reused: u32,
}

/// One decision of the bot's brain at a position, for one seat.
#[derive(Clone)]
struct Thought {
    turn: RawTurn,
    /// which part of the policy chose: "tree", "book", ...
    note: String,
    score: f64,
    sims: u32,
    ms: u64,
    playbook: String,
    /// how well the tracker had predicted this observation
    sync: String,
    root: Option<RootInfo>,
    enemy_visible: bool,
    /// where the tracker believes the opponent stands
    believed_enemy: P,
    /// tiles the opponent could be on, when it is out of sight
    possible: Vec<P>,
}

struct Node {
    parent: Option<usize>,
    /// the state before tick `state.tick` is played
    state: State,
    /// the turns that led here from the parent, and who chose them
    turns: [RawTurn; 2],
    by: [u8; 2],
    /// the events of the tick that led here
    events: Vec<Event>,
    children: Vec<usize>,
    /// the child the current line goes through
    fwd: Option<usize>,
    thoughts: [Option<Thought>; 2],
    /// on the loaded replay's own line
    recorded: bool,
    /// did the state hash equal the one the ladder recorded (replays only)
    hash_ok: Option<bool>,
    /// the value network's chance of fighter 1 winning from here
    pwin: f32,
}

struct Track {
    tracker: Tracker,
    /// the node whose observation it took in last
    at: usize,
    sync: String,
}

struct Session {
    id: u64,
    engine: Engine,
    rules_json: Value,
    map_json: Value,
    seed: i64,
    names: [String; 2],
    /// the seat played by hand (fighter index), if any
    human: Option<usize>,
    nodes: Vec<Node>,
    cur: usize,
    /// "game" or the replay's id
    source: String,
    recorded_result: Option<Value>,
    /// replay hashes checked and how many differed
    verified: Option<(usize, usize)>,
    trackers: [Option<Track>; 2],
    save_name: String,
    saved: Option<String>,
}

fn label(e: &Engine, t: &RawTurn) -> String {
    let mut parts: Vec<String> = Vec::new();
    match t.mv {
        DirIn::Dir(d) => parts.push(format!("step {}", DIR_NAMES[d as usize])),
        DirIn::Bad => parts.push("step ?".into()),
        DirIn::None => {}
    }
    match t.dash {
        DirIn::Dir(d) => parts.push(format!("dash {}", DIR_NAMES[d as usize])),
        DirIn::Bad => parts.push("dash ?".into()),
        DirIn::None => {}
    }
    match t.attack {
        AttackIn::Named { attack, target } => {
            let name = &e.rules.attacks[attack as usize].name;
            parts.push(match target {
                TargetIn::Tile(p) => format!("{name} at {},{}", p[0], p[1]),
                _ => name.clone(),
            });
        }
        AttackIn::Unknown => parts.push("attack ?".into()),
        AttackIn::None => {}
    }
    if parts.is_empty() {
        "wait".into()
    } else {
        parts.join(" + ")
    }
}

/// A projectile's position in tile units (tile centres at whole numbers).
fn exact(p: P) -> Value {
    json!([p[0] as f64 / SUB as f64, p[1] as f64 / SUB as f64])
}

/// Where the projectiles in `ids` fly if nobody moves: a point per tick from `st` on.
fn trace(e: &Engine, mut st: State, ids: &[u32]) -> Value {
    let mut paths: Vec<(u32, u8, Vec<Value>, Value)> = st
        .projectiles()
        .iter()
        .filter(|p| ids.contains(&p.id))
        .map(|p| (p.id, p.owner, vec![exact(p.exact_pos)], Value::Null))
        .collect();
    let mut events = Vec::new();
    for _ in 0..80 {
        if st.result.is_some() || !st.projectiles().iter().any(|p| ids.contains(&p.id)) {
            break;
        }
        e.step_events(&mut st, &[RawTurn::IDLE; 2], &mut events);
        for path in paths.iter_mut() {
            if let Some(p) = st.projectiles().iter().find(|p| p.id == path.0) {
                path.2.push(exact(p.exact_pos));
            }
        }
        for ev in &events {
            match ev {
                Event::ProjectileEnd { projectile, pos, reason } => {
                    if let Some(path) = paths.iter_mut().find(|x| x.0 == *projectile) {
                        path.2.push(json!([pos[0], pos[1]]));
                        path.3 = json!(reason);
                    }
                }
                Event::ProjectileBounce { projectile, pos, .. } => {
                    if let Some(path) = paths.iter_mut().find(|x| x.0 == *projectile) {
                        // the corner of the flight, between this tick's two points
                        let n = path.2.len();
                        path.2.insert(n.saturating_sub(1), json!([pos[0], pos[1]]));
                    }
                }
                _ => {}
            }
        }
    }
    Value::Array(paths.into_iter().map(|(id, owner, pts, end)| json!({ "id": id, "owner": owner + 1, "pts": pts, "end": end })).collect())
}

fn utc_stamp(secs: u64) -> String {
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Session {
    fn new(id: u64, engine: Engine, rules_json: Value, map_json: Value, seed: i64, names: [String; 2], source: String) -> Session {
        let state = engine.new_match(seed);
        let pwin = victris_engine::valuenet::win_prob(&engine, &state, 0, &victris_engine::valuenet_weights::VALUE_NET);
        let root = Node { parent: None, state, turns: [RawTurn::IDLE; 2], by: [BY_RECORDED; 2], events: Vec::new(), children: Vec::new(), fwd: None, thoughts: [None, None], recorded: false, hash_ok: None, pwin };
        let save_name = format!("{}-{}-{}", utc_stamp(now_secs()), engine.map.name, seed);
        Session { id, engine, rules_json, map_json, seed, names, human: None, nodes: vec![root], cur: 0, source, recorded_result: None, verified: None, trackers: [None, None], save_name, saved: None }
    }

    fn add_child(&mut self, parent: usize, turns: [RawTurn; 2], by: [u8; 2], recorded: bool) -> usize {
        let mut state = self.nodes[parent].state;
        let mut events = Vec::new();
        self.engine.step_events(&mut state, &turns, &mut events);
        let pwin = match state.result {
            Some(r) => match r.winner {
                Some(1) => 1.0,
                Some(_) => 0.0,
                None => 0.5,
            },
            None => victris_engine::valuenet::win_prob(&self.engine, &state, 0, &victris_engine::valuenet_weights::VALUE_NET),
        };
        self.nodes.push(Node { parent: Some(parent), state, turns, by, events, children: Vec::new(), fwd: None, thoughts: [None, None], recorded, hash_ok: None, pwin });
        let id = self.nodes.len() - 1;
        self.nodes[parent].children.push(id);
        id
    }

    /// The nodes from the start to `node`, in order.
    fn path_to(&self, node: usize) -> Vec<usize> {
        let mut path = vec![node];
        let mut n = node;
        while let Some(p) = self.nodes[n].parent {
            path.push(p);
            n = p;
        }
        path.reverse();
        path
    }

    /// The current line: the path to the current node, then on through the remembered children.
    fn line(&self) -> Vec<usize> {
        let mut line = self.path_to(self.cur);
        let mut n = self.cur;
        while let Some(c) = self.nodes[n].fwd {
            line.push(c);
            n = c;
        }
        line
    }

    fn goto(&mut self, node: usize) -> Result<(), String> {
        if node >= self.nodes.len() {
            return Err("no such position".into());
        }
        // The line keeps going through the node we jump to.
        let path = self.path_to(node);
        for pair in path.windows(2) {
            self.nodes[pair[0]].fwd = Some(pair[1]);
        }
        self.cur = node;
        Ok(())
    }

    /// Brings seat `seat`'s tracker to `node`: it has then taken in every observation that
    /// seat would have been sent on the way there, exactly as in a live match.
    fn sync_tracker(&mut self, seat: usize, node: usize) {
        let path = self.path_to(node);
        let mut tr = self.trackers[seat].take();
        let from = match &tr {
            Some(t) if t.at == node => path.len(),
            Some(t) if Some(t.at) == self.nodes[node].parent => path.len() - 1,
            _ => {
                tr = None;
                0
            }
        };
        let mut tr = tr.unwrap_or_else(|| {
            let mut tracker = Tracker::for_engine(self.engine.clone(), seat, self.seed);
            // There is no clock here: the match budget must never cut a think short.
            tracker.engine.rules.limits.match_budget_ms = i32::MAX;
            Track { tracker, at: usize::MAX, sync: String::new() }
        });
        for &n in &path[from..] {
            let nd = &self.nodes[n];
            let view = View::from_state(&self.engine, &nd.state, seat);
            let told: Vec<Value> = visible_events(&self.engine, &nd.state, seat, &nd.events).iter().map(|e| e.to_json(&self.engine)).collect();
            let prev = nd.parent.map(|_| nd.turns[seat]);
            let sync = tr.tracker.observe(&view, &told, prev);
            tr.sync = sync.name().to_string();
            tr.at = n;
        }
        self.trackers[seat] = Some(tr);
    }

    /// Asks the bot's brain for seat `seat`'s turn at `node`.
    fn think(&mut self, node: usize, seat: usize, pb: &Playbook) -> Thought {
        self.sync_tracker(seat, node);
        let view = View::from_state(&self.engine, &self.nodes[node].state, seat);
        let tr = self.trackers[seat].as_mut().expect("tracker synced");
        // A think longer than the ladder's tick allows needs the tick limit lifted too.
        let need = (pb.tree_ms + 450).min(i32::MAX as u64) as i32;
        if tr.tracker.engine.rules.limits.tick_ms < need {
            tr.tracker.engine.rules.limits.tick_ms = need;
        }
        #[cfg(root_hook)]
        marshal::policy::LAST_ROOT.with(|l| *l.borrow_mut() = None);
        let started = Instant::now();
        let d = decide(&tr.tracker, &view, pb);
        let ms = started.elapsed().as_millis() as u64;
        #[cfg(root_hook)]
        let root = marshal::policy::LAST_ROOT.with(|l| l.borrow_mut().take()).map(|r| RootInfo { acts: r.acts, value: r.value, reused: r.reused });
        #[cfg(not(root_hook))]
        let root: Option<RootInfo> = None;
        let w = self.engine.map.w;
        let t = &tr.tracker;
        let believed_enemy = t.belief.map(|s| s.fighters[1 - seat].pos).unwrap_or([0, 0]);
        let possible: Vec<P> = if t.enemy_visible { Vec::new() } else { t.possible.iter().enumerate().filter(|(_, b)| **b).map(|(i, _)| [i as i32 % w, i as i32 / w]).collect() };
        Thought { turn: d.turn, note: d.note.to_string(), score: d.score, sims: d.sims, ms, playbook: pb.name.clone(), sync: tr.sync.clone(), root, enemy_visible: t.enemy_visible, believed_enemy, possible }
    }

    /// The turn the side not played by hand plays at `node`: what the replay recorded for
    /// that tick (when asked to keep it), else the bot's decision there, made once and kept
    /// so that trying another move of ours meets the same reply.
    fn reply_at(&mut self, node: usize, seat: usize, pb: &Playbook, keep_recorded: bool) -> (RawTurn, u8) {
        if keep_recorded {
            if let Some(&c) = self.nodes[node].children.iter().find(|c| self.nodes[**c].recorded) {
                return (self.nodes[c].turns[seat], BY_RECORDED);
            }
        }
        if self.nodes[node].thoughts[seat].is_none() {
            let th = self.think(node, seat, pb);
            self.nodes[node].thoughts[seat] = Some(th);
        }
        (self.nodes[node].thoughts[seat].as_ref().unwrap().turn, BY_BOT)
    }

    /// Plays the tick at the current node: `mine` for the seat played by hand.
    fn play(&mut self, mine: RawTurn, by_me: u8, pb: &Playbook, keep_recorded: bool, forced_reply: Option<RawTurn>) -> Result<(), String> {
        let n = self.cur;
        if self.nodes[n].state.result.is_some() {
            return Err("The game is over at this position. Rewind to play on from an earlier one.".into());
        }
        let h = self.human.ok_or("Pick a side to play first.")?;
        // `forced_reply`: the other side's turn chosen by hand too, to ask "what if it had done this".
        let (reply, by_reply) = match forced_reply {
            Some(t) => (t, BY_YOU),
            None => self.reply_at(n, 1 - h, pb, keep_recorded),
        };
        let mut turns = [RawTurn::IDLE; 2];
        let mut by = [BY_RECORDED; 2];
        turns[h] = mine;
        by[h] = by_me;
        turns[1 - h] = reply;
        by[1 - h] = by_reply;
        let child = match self.nodes[n].children.iter().find(|c| self.nodes[**c].turns == turns).copied() {
            Some(c) => c,
            None => self.add_child(n, turns, by, false),
        };
        self.nodes[n].fwd = Some(child);
        self.cur = child;
        Ok(())
    }

    fn state_value(&self, s: &State) -> Value {
        let mut text = String::with_capacity(2048);
        self.engine.state_json(s, &mut text);
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    fn view_json(&self, s: &State, seat: usize) -> Value {
        let e = &self.engine;
        let v = View::from_state(e, s, seat);
        let from = s.fighters[seat].pos;
        let mut sight = String::with_capacity((e.map.w * e.map.h) as usize);
        for y in 0..e.map.h {
            for x in 0..e.map.w {
                sight.push(if can_see(&e.rules, &e.map, from, [x, y]) { '1' } else { '0' });
            }
        }
        json!({
            "moves": v.moves.iter().map(|(d, p)| json!({ "dir": DIR_NAMES[*d as usize], "tile": p })).collect::<Vec<_>>(),
            "dashes": v.dashes.iter().map(|d| json!({ "dir": DIR_NAMES[d.dir as usize], "tile": d.tile, "distance": d.distance })).collect::<Vec<_>>(),
            "danger": v.danger.iter().map(|d| json!({ "tile": d.tile, "in": d.tick - s.tick + 1, "damage": d.damage, "area": d.from_area })).collect::<Vec<_>>(),
            "enemy_visible": v.enemy.is_some(),
            "sight": sight,
            "slow": v.me.slow_ticks_left,
            "state_left": v.me.state_ticks_left,
            "power_left": v.me.power_ticks_left,
        })
    }

    fn thought_json(&self, th: &Thought) -> Value {
        let e = &self.engine;
        let side = |acts: &Vec<(RawTurn, u32, f64)>| {
            let total: u32 = acts.iter().map(|a| a.1).sum();
            let mut list: Vec<&(RawTurn, u32, f64)> = acts.iter().collect();
            list.sort_by(|a, b| b.1.cmp(&a.1));
            json!({
                "total": total,
                "count": acts.len(),
                "top": list.iter().take(40).map(|(t, n, q)| json!({ "label": label(e, t), "turn": turn_json(0, t, e), "n": n, "q": q })).collect::<Vec<_>>(),
            })
        };
        json!({
            "label": label(e, &th.turn),
            "turn": turn_json(0, &th.turn, e),
            "note": th.note,
            "score": th.score,
            "sims": th.sims,
            "ms": th.ms,
            "playbook": th.playbook,
            "sync": th.sync,
            "enemy_visible": th.enemy_visible,
            "believed_enemy": th.believed_enemy,
            "possible": th.possible,
            "root": th.root.as_ref().map(|r| json!({ "value": r.value, "reused": r.reused, "sides": [side(&r.acts[0]), side(&r.acts[1])] })),
        })
    }

    fn node_brief(&self, id: usize) -> Value {
        let n = &self.nodes[id];
        let tick = n.state.tick - 1;
        json!({ "id": id, "labels": [label(&self.engine, &n.turns[0]), label(&self.engine, &n.turns[1])], "by": n.by, "recorded": n.recorded,
            "turns": [turn_json(tick, &n.turns[0], &self.engine), turn_json(tick, &n.turns[1], &self.engine)] })
    }

    fn pos_json(&self) -> Value {
        let e = &self.engine;
        let n = &self.nodes[self.cur];
        let s = &n.state;
        let ids: Vec<u32> = s.projectiles().iter().map(|p| p.id).collect();
        let thoughts = |node: usize| -> Value { Value::Array(self.nodes[node].thoughts.iter().map(|t| t.as_ref().map(|t| self.thought_json(t)).unwrap_or(Value::Null)).collect()) };
        json!({
            "node": self.cur,
            "parent": n.parent,
            "tick": s.tick,
            "state": self.state_value(s),
            "hash": e.state_hash(s),
            "hash_ok": n.hash_ok,
            "recorded": n.recorded,
            "events": n.events.iter().map(|ev| ev.to_json(e)).collect::<Vec<_>>(),
            "labels": n.parent.map(|_| json!([label(e, &n.turns[0]), label(e, &n.turns[1])])),
            "turns": n.parent.map(|_| json!([turn_json(s.tick - 1, &n.turns[0], e), turn_json(s.tick - 1, &n.turns[1], e)])),
            "by": n.by,
            "views": if s.result.is_none() { json!([self.view_json(s, 0), self.view_json(s, 1)]) } else { Value::Null },
            "flights": trace(e, *s, &ids),
            "areas": s.areas().iter().map(|b| json!({ "id": b.id, "owner": b.owner + 1, "pos": b.pos, "in": b.explodes_tick - s.tick + 1, "damage": b.damage, "tiles": blast_tiles(&e.map, b.pos, b.radius) })).collect::<Vec<_>>(),
            "result": s.result.map(|r| json!({ "winner": r.winner, "reason": r.reason.name(), "final_tick": r.final_tick })),
            "variations": n.children.iter().map(|c| self.node_brief(*c)).collect::<Vec<_>>(),
            "siblings": n.parent.map(|p| self.nodes[p].children.iter().map(|c| self.node_brief(*c)).collect::<Vec<_>>()),
            "thoughts": thoughts(self.cur),
            "prev_thoughts": n.parent.map(thoughts),
            "pwin": n.pwin,
            "next_shrink": victris_engine::observe::next_shrink(&e.rules, &e.map, s.tick - 1).map(|(t, r)| json!({ "radius": r, "in": t - s.tick })),
        })
    }

    fn line_json(&self) -> Value {
        let line = self.line();
        Value::Array(
            line.iter()
                .map(|&id| {
                    let n = &self.nodes[id];
                    let variations = n.parent.map(|p| self.nodes[p].children.len()).unwrap_or(1);
                    // the bot's own estimate when it chose the turn played from here
                    let eval = (0..2).find_map(|x| n.thoughts[x].as_ref().filter(|t| t.note == "tree").map(|t| json!([x, t.score])));
                    json!({
                        "id": id,
                        "tick": n.state.tick,
                        "hp": [n.state.fighters[0].hp.max(0), n.state.fighters[1].hp.max(0)],
                        "var": variations,
                        "by": n.by,
                        "rec": n.recorded,
                        "pwin": (n.pwin * 1000.0).round() / 1000.0,
                        "eval": eval,
                    })
                })
                .collect(),
        )
    }

    fn map_json_view(&self) -> Value {
        let m = &self.engine.map;
        let tiles: Vec<String> = (0..m.h).map(|y| (0..m.w).map(|x| if m.is_wall([x, y]) { '#' } else { '.' }).collect()).collect();
        json!({
            "name": m.name, "w": m.w, "h": m.h, "tiles": tiles, "center": m.center, "spawns": m.spawns,
            "pads": m.pads.iter().map(|p| json!({ "pos": p.pos, "kind": p.kind.name() })).collect::<Vec<_>>(),
        })
    }

    /// The current line as a replay in the ladder's format, so every replay tool can read it.
    fn replay_text(&self) -> String {
        let e = &self.engine;
        let header = json!({
            "type": "header", "replay_format": 1, "engine": victris_engine::ENGINE_VERSION, "rules": self.rules_json, "map": e.map.name,
            "map_data": self.map_json, "rules_hash": e.rules.hash, "seed": self.seed,
            "bots": [{ "id": 1, "name": self.names[0] }, { "id": 2, "name": self.names[1] }],
            "playground": { "by_hand": self.human.map(|h| h + 1), "from": self.source },
        });
        let mut text = format!("{header}\n");
        let line = self.line();
        for &id in line.iter().skip(1) {
            let n = &self.nodes[id];
            let tick = n.state.tick - 1;
            let who = |b: u8| ["recorded", "hand", "bot"][b as usize];
            let row = json!({ "type": "tick", "tick": tick, "turns": { "1": turn_json(tick, &n.turns[0], e), "2": turn_json(tick, &n.turns[1], e) },
                "hash": e.state_hash(&n.state), "by": [who(n.by[0]), who(n.by[1])] });
            text.push_str(&format!("{row}\n"));
        }
        if let Some(r) = line.last().and_then(|id| self.nodes[*id].state.result) {
            text.push_str(&format!("{}\n", json!({ "type": "result", "winner": r.winner, "reason": r.reason.name(), "final_tick": r.final_tick })));
        }
        text
    }
}

struct Book {
    label: String,
    path: PathBuf,
}

struct App {
    root: PathBuf,
    web: Option<PathBuf>,
    sess: Session,
    next_id: u64,
    /// path of the playbook file the bot plays from
    book: PathBuf,
    /// think time per tick in ms, instead of the playbook's own
    think_ms: Option<u64>,
    /// playbook settings laid over the file's
    overrides: Value,
    /// at the first tick of a branch off a replay, the other side plays what it played there
    keep_recorded: bool,
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    kind: &'static str,
    body: Vec<u8>,
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if path.extension().map(|x| x == "gz").unwrap_or(false) {
        let mut out = String::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_string(&mut out).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(out)
    } else {
        String::from_utf8(bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

impl App {
    fn books(&self) -> Vec<Book> {
        let mut out = Vec::new();
        let live = self.root.join("data/releases/live");
        let release = std::fs::read_link(&live).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| "live".into());
        let mut add = |label: String, path: PathBuf| {
            if path.is_file() && !out.iter().any(|b: &Book| b.path == path) {
                out.push(Book { label, path });
            }
        };
        add(format!("field-marshal live ({release}, flagship.json)"), live.join("playbooks/flagship.json"));
        add(format!("scouts live ({release}, default.json)"), live.join("playbooks/default.json"));
        add("working tree flagship.json".into(), self.root.join("field-marshal/playbooks/flagship.json"));
        add("working tree default.json".into(), self.root.join("field-marshal/playbooks/default.json"));
        if let Ok(dir) = std::fs::read_dir(self.root.join("data/playground/playbooks")) {
            let mut extra: Vec<PathBuf> = dir.filter_map(|d| d.ok().map(|d| d.path())).filter(|p| p.extension().map(|x| x == "json").unwrap_or(false)).collect();
            extra.sort();
            for p in extra {
                add(format!("mine: {}", p.file_name().unwrap_or_default().to_string_lossy()), p);
            }
        }
        out
    }

    /// The playbook the bot plays from now, against an opponent of this name.
    fn playbook(&self, opponent: &str) -> Playbook {
        let raw: Value = std::fs::read_to_string(&self.book).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        let books = Playbooks::from_json(&raw);
        let mut over = self.overrides.as_object().cloned().unwrap_or_default();
        if let Some(ms) = self.think_ms {
            over.insert("tree_ms".into(), json!(ms));
        }
        if over.is_empty() {
            books.pick(opponent).clone()
        } else {
            let base = books.pick(opponent).name.clone();
            books.pick_with(opponent, &format!("{base}*"), &Value::Object(over))
        }
    }

    fn new_game(&mut self, map: &str, seed: i64, seat: usize) -> Result<(), String> {
        use victris_engine::shipped::{MAPS_JSON, MAP_NAMES, RULES_JSON};
        let i = MAP_NAMES.iter().position(|n| *n == map).ok_or(format!("unknown map {map}"))?;
        let rules: Value = serde_json::from_str(RULES_JSON).map_err(|e| e.to_string())?;
        let map_json: Value = serde_json::from_str(MAPS_JSON[i]).map_err(|e| e.to_string())?;
        let engine = Engine::from_json(&rules, &map_json)?;
        let mut names = ["bot".to_string(), "bot".to_string()];
        names[seat] = "Mike".to_string();
        self.next_id += 1;
        self.sess = Session::new(self.next_id, engine, rules, map_json, seed, names, "game".into());
        self.sess.human = Some(seat);
        Ok(())
    }

    fn replay_path(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return None;
        }
        let prefix: String = id.chars().take(5).collect();
        [
            self.root.join("replays/corpus").join(format!("{id}.jsonl.gz")),
            self.root.join("data/corpus-remote/replays").join(&prefix).join(format!("{id}.jsonl.gz")),
            self.root.join("data/playground/games").join(format!("{id}.jsonl")),
            self.root.join("data/playground/games").join(id),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    fn load_replay(&mut self, path: &Path, name: &str) -> Result<(), String> {
        let text = read_text(path)?;
        let header: Value = serde_json::from_str(text.lines().next().unwrap_or("")).map_err(|e| format!("replay header: {e}"))?;
        let rep = parse_replay(&text)?;
        let names = [rep.bots.first().cloned().unwrap_or_else(|| "fighter 1".into()), rep.bots.get(1).cloned().unwrap_or_else(|| "fighter 2".into())];
        self.next_id += 1;
        let mut s = Session::new(self.next_id, rep.engine.clone(), header.get("rules").cloned().unwrap_or(Value::Null), header.get("map_data").cloned().unwrap_or(Value::Null), rep.seed, names, name.to_string());
        let (mut checked, mut wrong) = (0usize, 0usize);
        let mut at = 0usize;
        for t in &rep.ticks {
            if s.nodes[at].state.result.is_some() {
                break;
            }
            let child = s.add_child(at, t.turns, [BY_RECORDED; 2], true);
            if let Some(h) = &t.hash {
                checked += 1;
                let ok = s.engine.state_hash(&s.nodes[child].state) == *h;
                s.nodes[child].hash_ok = Some(ok);
                if !ok {
                    wrong += 1;
                }
            }
            s.nodes[at].fwd = Some(child);
            at = child;
        }
        s.verified = Some((checked, wrong));
        s.recorded_result = rep.result.as_ref().map(|r| json!({ "winner": r.winner, "reason": r.reason, "final_tick": r.final_tick }));
        self.sess = s;
        Ok(())
    }

    /// Field-marshal's ladder games against the bots now ranked in the top `top` of the
    /// ladder (from the standings the fleet fetches, `data/standings-remote/standings.json`),
    /// newest first, with the filters the page offers. Only games whose replay is on this machine.
    fn top_games(&self, ours: &str, top: u64, map: &str, seat: u64, result: &str, opponent: &str) -> Value {
        let standings: Value = std::fs::read_to_string(self.root.join("data/standings-remote/standings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let mut ranked: Vec<(String, u64, Value)> = standings
            .get("leaderboard")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let rank = r.get("rank").and_then(Value::as_u64)?;
                        (rank <= top).then(|| (r.get("name").and_then(Value::as_str).unwrap_or("").to_string(), rank, r.get("rating").cloned().unwrap_or(Value::Null)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ranked.sort_by_key(|r| r.1);
        let rank_of = |name: &str| ranked.iter().find(|r| r.0 == name).map(|r| r.1);
        let mut rows: Vec<Value> = Vec::new();
        let (mut wins, mut losses, mut draws, mut all) = (0, 0, 0, 0);
        if let Ok(text) = std::fs::read_to_string(self.root.join("data/corpus-remote/index.jsonl")) {
            for line in text.lines() {
                if !line.contains(ours) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                let bots = v.get("bots").and_then(Value::as_array).cloned().unwrap_or_default();
                let Some(me) = bots.iter().find(|b| b.get("name").and_then(Value::as_str) == Some(ours)) else { continue };
                let Some(opp) = bots.iter().find(|b| b.get("name").and_then(Value::as_str) != Some(ours)) else { continue };
                let opp_name = opp.get("name").and_then(Value::as_str).unwrap_or("");
                let Some(rank) = rank_of(opp_name) else { continue };
                if v.get("voided").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                if self.replay_path(id).is_none() {
                    continue;
                }
                let fighter = me.get("fighter").and_then(Value::as_u64).unwrap_or(0);
                let res = match v.get("winner") {
                    Some(Value::Null) | None => "draw",
                    Some(w) if Some(w) == me.get("id") => "win",
                    _ => "loss",
                };
                all += 1;
                let m = v.get("map").and_then(Value::as_str).unwrap_or("");
                if (!map.is_empty() && m != map) || (seat != 0 && fighter != seat) || (!opponent.is_empty() && opp_name != opponent) {
                    continue;
                }
                match res {
                    "win" => wins += 1,
                    "loss" => losses += 1,
                    _ => draws += 1,
                }
                if !result.is_empty() && res != result {
                    continue;
                }
                rows.push(json!({
                    "id": id, "ended": v.get("ended"), "map": m, "seat": fighter, "result": res, "reason": v.get("reason"), "ticks": v.get("final_tick"),
                    "version": me.get("version"), "rating": me.get("rating_before"),
                    "opponent": { "name": opp_name, "rank": rank, "rating": opp.get("rating_before"), "version": opp.get("version") },
                }));
            }
        }
        rows.sort_by(|a, b| b["ended"].as_str().unwrap_or("").cmp(a["ended"].as_str().unwrap_or("")));
        json!({
            "ours": ours, "top": top, "fetched": standings.get("fetched"), "all": all,
            "counts": { "win": wins, "loss": losses, "draw": draws },
            "opponents": ranked.iter().map(|r| json!({ "name": r.0, "rank": r.1, "rating": r.2 })).collect::<Vec<_>>(),
            "rows": rows,
        })
    }

    fn list_replays(&self, q: &str, limit: usize) -> Value {
        let q = q.to_lowercase();
        let mut rows: Vec<Value> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(self.root.join("data/corpus-remote/index.jsonl")) {
            for line in text.lines() {
                if !q.is_empty() && !line.to_lowercase().contains(&q) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                let bots = v.get("bots").and_then(Value::as_array).cloned().unwrap_or_default();
                let winner = bots.iter().find(|b| b.get("id") == v.get("winner")).and_then(|b| b.get("name")).cloned().unwrap_or(Value::Null);
                let mut seats = [Value::Null, Value::Null];
                for b in &bots {
                    if let Some(f) = b.get("fighter").and_then(Value::as_u64) {
                        if (1..=2).contains(&f) {
                            seats[f as usize - 1] = json!({ "name": b.get("name"), "rating": b.get("rating_before"), "version": b.get("version") });
                        }
                    }
                }
                rows.push(json!({ "id": v.get("id"), "ended": v.get("ended"), "map": v.get("map"), "ticks": v.get("final_tick"), "reason": v.get("reason"), "winner": winner, "bots": seats }));
            }
        }
        rows.sort_by(|a, b| b["ended"].as_str().unwrap_or("").cmp(a["ended"].as_str().unwrap_or("")));
        let total = rows.len();
        rows.truncate(limit);
        let mut mine: Vec<String> = std::fs::read_dir(self.root.join("data/playground/games"))
            .map(|d| d.filter_map(|x| x.ok()).map(|x| x.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".jsonl")).collect())
            .unwrap_or_default();
        mine.sort();
        mine.reverse();
        mine.truncate(100);
        json!({ "total": total, "rows": rows, "mine": mine })
    }

    fn save(&mut self) -> Result<String, String> {
        let dir = self.root.join("data/playground/games");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{}.jsonl", self.sess.save_name));
        std::fs::write(&path, self.sess.replay_text()).map_err(|e| format!("{}: {e}", path.display()))?;
        let shown = path.display().to_string();
        self.sess.saved = Some(shown.clone());
        Ok(shown)
    }

    fn opponent_name(&self) -> String {
        self.sess.human.map(|h| self.sess.names[h].clone()).unwrap_or_default()
    }

    fn snapshot(&self) -> Value {
        let s = &self.sess;
        let books = self.books();
        json!({
            "session": {
                "id": s.id, "source": s.source, "seed": s.seed, "names": s.names, "human": s.human,
                "map": s.map_json_view(), "rules": s.rules_json, "attacks": s.engine.rules.attacks.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                "recorded_result": s.recorded_result, "verified": s.verified.map(|(c, w)| json!({ "checked": c, "wrong": w })),
                "saved": s.saved, "positions": s.nodes.len(),
            },
            "settings": {
                "book": self.book.display().to_string(),
                "books": books.iter().map(|b| json!({ "label": b.label, "path": b.path.display().to_string() })).collect::<Vec<_>>(),
                "playbook": self.playbook(&self.opponent_name()).name,
                "tree_ms": self.playbook(&self.opponent_name()).tree_ms,
                "think_ms": self.think_ms,
                "overrides": self.overrides,
                "keep_recorded": self.keep_recorded,
                "root_hook": cfg!(root_hook),
                "maps": victris_engine::shipped::MAP_NAMES,
            },
            "cur": s.cur,
            "line": s.line_json(),
            "pos": s.pos_json(),
        })
    }

    fn api(&mut self, req: &Request) -> Result<Value, String> {
        let body: Value = if req.body.is_empty() { Value::Null } else { serde_json::from_slice(&req.body).map_err(|e| format!("bad JSON: {e}"))? };
        let seat_of = |v: Option<&Value>| -> Option<usize> { v.and_then(Value::as_u64).filter(|x| *x < 2).map(|x| x as usize) };
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/state") => {}
            ("GET", "/api/topgames") => {
                let get = |key: &str| req.query.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(percent_decode)).unwrap_or_default();
                let ours = get("bot");
                let ours = if ours.is_empty() { "field-marshal".to_string() } else { ours };
                let top = get("top").parse().unwrap_or(10u64).clamp(1, 100);
                return Ok(self.top_games(&ours, top, &get("map"), get("seat").parse().unwrap_or(0), &get("result"), &get("opponent")));
            }
            ("GET", "/api/replays") => {
                let get = |key: &str| req.query.split('&').find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(percent_decode)).unwrap_or_default();
                let limit = get("limit").parse().unwrap_or(200usize).min(2000);
                return Ok(self.list_replays(&get("q"), limit));
            }
            ("POST", "/api/new") => {
                let map = body.get("map").and_then(Value::as_str).unwrap_or("arena-21").to_string();
                let seed = body.get("seed").and_then(Value::as_i64).unwrap_or_else(|| (SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1) as i64) ^ (now_secs() as i64 & 0x3fff_ffff));
                let seat = seat_of(body.get("seat")).unwrap_or(0);
                self.new_game(&map, seed, seat)?;
            }
            ("POST", "/api/turn") => {
                let mine = RawTurn::from_json(body.get("turn"), &self.sess.engine.rules);
                let pb = self.playbook(&self.opponent_name());
                let reply = body.get("reply").filter(|r| !r.is_null()).map(|r| RawTurn::from_json(Some(r), &self.sess.engine.rules));
                self.sess.play(mine, BY_YOU, &pb, self.keep_recorded, reply)?;
                self.autosave();
            }
            ("POST", "/api/botmove") => {
                // the bot's brain plays this tick for the seat played by hand
                let h = self.sess.human.ok_or("Pick a side to play first.")?;
                let n = self.sess.cur;
                if self.sess.nodes[n].state.result.is_some() {
                    return Err("The game is over at this position.".into());
                }
                let mine_book = self.playbook(&self.sess.names[1 - h].clone());
                if self.sess.nodes[n].thoughts[h].is_none() {
                    let th = self.sess.think(n, h, &mine_book);
                    self.sess.nodes[n].thoughts[h] = Some(th);
                }
                let mine = self.sess.nodes[n].thoughts[h].as_ref().unwrap().turn;
                let pb = self.playbook(&self.opponent_name());
                self.sess.play(mine, BY_BOT, &pb, self.keep_recorded, None)?;
                self.autosave();
            }
            ("POST", "/api/goto") => {
                let node = body.get("node").and_then(Value::as_u64).ok_or("node missing")? as usize;
                self.sess.goto(node)?;
            }
            ("POST", "/api/think") => {
                let seat = seat_of(body.get("seat")).ok_or("seat missing")?;
                let n = self.sess.cur;
                if self.sess.nodes[n].state.result.is_some() {
                    return Err("The game is over at this position.".into());
                }
                if body.get("redo").and_then(Value::as_bool).unwrap_or(false) || self.sess.nodes[n].thoughts[seat].is_none() {
                    let mut pb = self.playbook(&self.sess.names[1 - seat].clone());
                    // a longer (or shorter) think for this one question
                    if let Some(ms) = body.get("think_ms").and_then(Value::as_u64).filter(|x| *x > 0) {
                        pb.tree_ms = ms.min(60_000);
                    }
                    let th = self.sess.think(n, seat, &pb);
                    self.sess.nodes[n].thoughts[seat] = Some(th);
                }
            }
            ("POST", "/api/preview") => {
                let s = &self.sess;
                let seat = seat_of(body.get("seat")).or(s.human).ok_or("seat missing")?;
                let n = &s.nodes[s.cur];
                if n.state.result.is_some() {
                    return Ok(json!({}));
                }
                let e = &s.engine;
                let mut st = n.state;
                let first_new = st.next_projectile_id;
                let mut turns = [RawTurn::IDLE; 2];
                turns[seat] = RawTurn::from_json(body.get("turn"), &e.rules);
                let mut events = Vec::new();
                e.step_events(&mut st, &turns, &mut events);
                let ids: Vec<u32> = st.projectiles().iter().filter(|p| p.id >= first_new && p.owner as usize == seat).map(|p| p.id).collect();
                return Ok(json!({
                    "label": label(e, &turns[seat]),
                    "events": events.iter().map(|ev| ev.to_json(e)).collect::<Vec<_>>(),
                    "me": st.fighters[seat].pos,
                    "paths": trace(e, st, &ids),
                    "areas": st.areas().iter().filter(|b| b.owner as usize == seat && b.launched_tick == n.state.tick)
                        .map(|b| json!({ "pos": b.pos, "in": b.explodes_tick - st.tick + 1, "tiles": blast_tiles(&e.map, b.pos, b.radius) })).collect::<Vec<_>>(),
                }));
            }
            ("POST", "/api/load") => {
                let (path, name) = if let Some(id) = body.get("id").and_then(Value::as_str) {
                    (self.replay_path(id).ok_or(format!("replay {id} is not on this machine"))?, id.to_string())
                } else {
                    let p = PathBuf::from(body.get("path").and_then(Value::as_str).ok_or("id or path missing")?);
                    let p = if p.is_absolute() { p } else { self.root.join(p) };
                    let p = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
                    let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if !p.starts_with(&root) || !(name.ends_with(".jsonl") || name.ends_with(".jsonl.gz")) {
                        return Err(format!("only replay files under {} can be loaded", root.display()));
                    }
                    (p, name)
                };
                self.load_replay(&path, &name)?;
                // "seat": take the seat of the bot of this name (or this fighter number) at once,
                // so a move can be changed at any tick
                match body.get("seat") {
                    Some(Value::String(who)) => self.sess.human = self.sess.names.iter().position(|n| n == who),
                    Some(Value::Number(x)) => self.sess.human = x.as_u64().filter(|x| (1..=2).contains(x)).map(|x| x as usize - 1),
                    _ => {}
                }
                // "tick": open the game at this tick
                if let Some(t) = body.get("tick").and_then(Value::as_i64) {
                    let line = self.sess.line();
                    if let Some(&n) = line.iter().find(|&&n| self.sess.nodes[n].state.tick as i64 == t).or(line.last()) {
                        self.sess.cur = n;
                    }
                }
            }
            ("POST", "/api/seat") => {
                self.sess.human = seat_of(body.get("seat"));
            }
            ("POST", "/api/settings") => {
                if let Some(p) = body.get("book").and_then(Value::as_str) {
                    let p = PathBuf::from(p);
                    if !self.books().iter().any(|b| b.path == p) {
                        return Err("unknown playbook file".into());
                    }
                    self.book = p;
                }
                if let Some(v) = body.get("think_ms") {
                    self.think_ms = v.as_u64().filter(|x| *x > 0).map(|x| x.min(60_000));
                }
                if let Some(v) = body.get("keep_recorded").and_then(Value::as_bool) {
                    self.keep_recorded = v;
                }
                if let Some(v) = body.get("overrides") {
                    self.overrides = if v.is_object() { v.clone() } else { json!({}) };
                }
            }
            ("POST", "/api/save") => {
                self.save()?;
            }
            _ => return Err("no such endpoint".into()),
        }
        Ok(self.snapshot())
    }

    /// A line played by hand is written out after every tick, so it can be handed to the
    /// replay tools and a restart of the server loses no game.
    fn autosave(&mut self) {
        let _ = self.save();
    }

    fn handle(&mut self, req: &Request) -> Response {
        if req.method == "GET" && (req.path == "/" || req.path == "/index.html") {
            let page = self.web.as_ref().and_then(|p| std::fs::read(p).ok()).unwrap_or_else(|| WEB.as_bytes().to_vec());
            return Response { status: 200, kind: "text/html; charset=utf-8", body: page };
        }
        if !req.path.starts_with("/api/") {
            return Response { status: 404, kind: "text/plain", body: b"not found".to_vec() };
        }
        match self.api(req) {
            Ok(v) => Response { status: 200, kind: "application/json", body: v.to_string().into_bytes() },
            Err(e) => Response { status: 400, kind: "application/json", body: json!({ "error": e }).to_string().into_bytes() },
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && s.is_char_boundary(i + 1) && s.is_char_boundary(i + 3) => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(x) => {
                    out.push(x);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok()?;
            }
        }
    }
    if length > 64 << 20 {
        return None;
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target.clone(), String::new()));
    Some(Request { method, path, query, body })
}

fn write_response(mut stream: &TcpStream, r: &Response) {
    let reason = if r.status == 200 { "OK" } else { "Error" };
    let head = format!("HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", r.status, r.kind, r.body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
    let _ = stream.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("playground [--port 8790] [--bind 0.0.0.0] [--root <victris folder>] [--playbooks <file>] [--web <index.html>]");
        return;
    }
    let root = PathBuf::from(arg("--root").unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string()));
    let root = root.canonicalize().unwrap_or(root);
    let port: u16 = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(8790);
    let bind = arg("--bind").unwrap_or_else(|| "0.0.0.0".to_string());
    // The page is read from the source folder when it is there, so it can change without a build.
    let web = arg("--web").map(PathBuf::from).or_else(|| Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html")))).filter(|p| p.is_file());

    let rules: Value = serde_json::from_str(victris_engine::shipped::RULES_JSON).expect("shipped rules");
    let map_json: Value = serde_json::from_str(victris_engine::shipped::MAPS_JSON[0]).expect("shipped map");
    let engine = Engine::from_json(&rules, &map_json).expect("shipped engine");
    let sess = Session::new(0, engine, rules, map_json, 1, ["Mike".to_string(), "bot".to_string()], "game".into());
    let mut app = App { root: root.clone(), web, sess, next_id: 0, book: PathBuf::new(), think_ms: None, overrides: json!({}), keep_recorded: true };
    app.book = match arg("--playbooks") {
        Some(p) => PathBuf::from(p),
        None => app.books().first().map(|b| b.path.clone()).unwrap_or_default(),
    };
    app.sess.human = Some(0);

    let listener = match TcpListener::bind((bind.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("playground: cannot listen on {bind}:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!("playground: http://localhost:{port}/  (folder {}, playbook {})", root.display(), app.book.display());

    // One thread owns the game and the bot's brain (the search keeps its tree between
    // ticks in thread-local storage); connections hand it requests one at a time.
    let (tx, rx) = mpsc::channel::<(Request, mpsc::Sender<Response>)>();
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            for (req, reply) in rx {
                let resp = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.handle(&req))) {
                    Ok(r) => r,
                    Err(p) => {
                        // A panic in the tracker or the policy costs this request, not the game.
                        app.sess.trackers = [None, None];
                        let what = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into());
                        Response { status: 500, kind: "application/json", body: json!({ "error": format!("the engine or the bot's code panicked: {what}") }).to_string().into_bytes() }
                    }
                };
                let _ = reply.send(resp);
            }
        })
        .expect("game thread");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(20)));
            let Some(req) = read_request(&stream) else { return };
            let (rtx, rrx) = mpsc::channel();
            if tx.send((req, rtx)).is_err() {
                return;
            }
            if let Ok(resp) = rrx.recv() {
                write_response(&stream, &resp);
            }
        });
    }
}
