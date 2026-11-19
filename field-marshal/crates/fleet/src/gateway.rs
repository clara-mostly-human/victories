//! The gateway: the one process on this machine that talks to the ladder.
//!
//! It runs under launchd, holds one WebSocket per bot, and nothing else here opens a
//! connection to the ladder. The programs that decide the turns ("bot processes": a
//! `marshal --stdio` from whatever release the bot is on) are separate local processes.
//! Each attaches to the gateway over a Unix socket (`data/run/gateway.sock`) and from then
//! on reads the ladder's frames from it and writes its turns to it. A bot process can be
//! stopped, replaced or crash, and the bot's connection to the ladder stays up.
//!
//! What the gateway owns:
//!
//! - **The sockets.** It connects with backoff, one connection at a time for the whole
//!   fleet, and it tests each socket: the round trip of a ping when it opens, and in play
//!   the wire time of every turn (what the ladder charged us minus the time the bot process
//!   took). A slow socket is replaced, politely: spaced out across the fleet, later each
//!   time for the same bot.
//! - **The ladder's side of the protocol.** Queueing, the version label (a refused label is
//!   bumped here), quotas (the ladder's guide: on `quota_exceeded` stay queued and wait, do
//!   not queue again or reconnect), fatal refusals, resumed matches.
//! - **A queue per bot.** Frames for a bot process wait in its own queue; a stale
//!   observation is dropped for the newer one; a process that does not read is detached.
//!   While no process is attached, or it is late, the gateway answers the ladder in time
//!   itself, so silence never forfeits a match.
//! - **The truth.** The gateway knows the connection, the match, the tick and the label.
//!   It tells the bot process (status frames) and sets it right when the process is out of
//!   step: a process that attaches or disagrees mid-match is sent the match start and the
//!   latest observation again.
//! - **Health.** One verdict per bot, from turns flowing, written to
//!   the store's `gateway_state` every second; stalls are logged with their cause. Every
//!   socket gets an ID (`sockets` in `fleet.db`) and everything measured on it goes to the
//!   time-series store `data/telemetry.db` (see `telemetry`).
//!
//! The bot process side of the protocol (one JSON object per line, both ways):
//!
//! - first line from the process: `{"type":"attach","bot":..,"release":..,"version":..,"proto":N,"pid":N}`;
//! - then the ladder's frames come down unchanged (`hello` with `_remote` added,
//!   `observation`, `end`, `error`), and the process writes `{"type":"ready"}` and turns;
//! - a process with `proto` 2 or more also gets `{"type":"status",..}` (on change and once a
//!   second), `{"type":"ping","id":N}` (answered with `{"type":"pong","id":N,"match":..,"tick":..}`)
//!   and `{"type":"sync",..}` before a replayed match start.
//!
//! A bot is run this way when its entry in `fleet/bots.json` has `"gateway": true`.

use crate::config::{BotConfig, Config};
use crate::online::log_line;
use crate::store::{data_dir, default_db, now_ms, Store};
use crate::telemetry::{self, Telemetry};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const DEFAULT_URL: &str = "wss://victrisarena.com/bot";

static TEL: std::sync::OnceLock<Telemetry> = std::sync::OnceLock::new();
static TEL_OFF: std::sync::OnceLock<Telemetry> = std::sync::OnceLock::new();

/// The gateway's telemetry: every socket's measurements, the events and the per-bot state
/// (see `telemetry`). Records nothing until `run` has started it.
fn tel() -> &'static Telemetry {
    TEL.get().unwrap_or_else(|| TEL_OFF.get_or_init(Telemetry::off))
}

/// Errors that reconnecting cannot fix (the ladder's guide: stop on these).
const FATAL_CODES: [&str; 15] = [
    "missing_authorization",
    "invalid_token",
    "suspended",
    "banned",
    "retired",
    "account_banned",
    "account_required",
    "human_only",
    "replaced",
    "unknown_ladder",
    "ladder_closed",
    "enrollment_required",
    "wrong_ladder",
    "membership_revoked",
    "ladder_unavailable",
];

/// Where bot processes attach.
pub fn sock_path(root: &Path) -> PathBuf {
    let path = data_dir(root).join("run").join("gateway.sock");
    // A Unix socket's path has a hard limit (104 bytes on a Mac). A root too deep for it
    // gets a socket in the system's temporary folder, named after the root.
    if path.as_os_str().len() <= 100 {
        return path;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.as_os_str().as_encoded_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    std::env::temp_dir().join(format!("victris-gw-{h:016x}.sock"))
}

#[derive(Clone)]
pub struct Options {
    /// least time between two connection attempts, fleet-wide
    pub connect_gap: Duration,
    /// least time between two `queue` requests, fleet-wide
    pub queue_gap: Duration,
    /// a bot process that has not answered an observation after this long is answered for
    pub answer_deadline: Duration,
    /// ... and one that has not said `ready` after this long
    pub ready_deadline: Duration,
    /// a socket whose median wire time in a match is above this is slow
    pub slow_wire_ms: f64,
    /// a socket whose ping round trip at connect is above this is replaced (0: measure only)
    pub ping_reject_ms: f64,
    /// least time between two replacements of slow sockets, fleet-wide
    pub replace_gap: Duration,
    /// at most this many of our bots in a match or queued at once (the flagship always may)
    pub max_concurrent: usize,
    /// the flagship waits this long at most before queueing while a scout is held back by the
    /// ladder's match limit, so the scouts get the slot that frees first (Mike, 2026-11-19:
    /// "push the scouts up in priority. The flagship is just the control"); 0: no waiting
    pub flagship_yield: Duration,
    /// give up a socket in a match once it has lost three turns on the wire there (once a match)
    pub swap_in_match: bool,
    /// a bot queued this long without a match is reported as stalled
    pub stall_after: Duration,
    /// at most this many sockets are given up for a better one in any ten minutes, across
    /// the whole fleet. Every new connection counts against whatever the ladder's edge
    /// allows us, and on 2026-11-18 an eager version of this test reconnected 33 times in
    /// four minutes and left most bots on slow sockets. The cap is what keeps it polite.
    pub replacements_per_10min: usize,
}

impl Default for Options {
    fn default() -> Self {
        // The ladder allows one second a turn; our reply takes about 100 ms to get there.
        Options {
            connect_gap: Duration::from_millis(2000),
            queue_gap: Duration::from_millis(300),
            answer_deadline: Duration::from_millis(800),
            ready_deadline: Duration::from_millis(1500),
            // 0: a socket is not replaced in play for being slow (only for losing turns)
            slow_wire_ms: 0.0,
            ping_reject_ms: 0.0,
            replace_gap: Duration::from_secs(90),
            max_concurrent: 5,
            flagship_yield: Duration::from_secs(120),
            swap_in_match: true,
            stall_after: Duration::from_secs(120),
            replacements_per_10min: 8,
        }
    }
}

impl Options {
    /// The defaults with whatever `"gateway": {..}` in `fleet/bots.json` changes.
    pub fn load(root: &Path) -> Options {
        let mut o = Options::default();
        let v: Value = std::fs::read_to_string(root.join("fleet").join("bots.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        let g = v.get("gateway").cloned().unwrap_or(Value::Null);
        let ms = |k: &str, d: Duration| g.get(k).and_then(Value::as_u64).map(Duration::from_millis).unwrap_or(d);
        let num = |k: &str, d: f64| g.get(k).and_then(Value::as_f64).unwrap_or(d);
        o.connect_gap = ms("connect_gap_ms", o.connect_gap);
        o.queue_gap = ms("queue_gap_ms", o.queue_gap);
        o.answer_deadline = ms("answer_deadline_ms", o.answer_deadline);
        o.ready_deadline = ms("ready_deadline_ms", o.ready_deadline);
        o.slow_wire_ms = num("slow_wire_ms", o.slow_wire_ms);
        o.ping_reject_ms = num("ping_reject_ms", o.ping_reject_ms);
        o.replace_gap = ms("replace_gap_ms", o.replace_gap);
        o.max_concurrent = g.get("max_concurrent").and_then(Value::as_u64).map(|n| n.max(1) as usize).unwrap_or(o.max_concurrent);
        o.stall_after = ms("stall_after_ms", o.stall_after);
        o.flagship_yield = ms("flagship_yield_ms", o.flagship_yield);
        o.swap_in_match = g.get("swap_in_match").and_then(Value::as_bool).unwrap_or(o.swap_in_match);
        o.replacements_per_10min = g.get("replacements_per_10min").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(o.replacements_per_10min);
        o
    }
}

/// A budget: at most `limit` of something in any stretch of `window`. Whoever asks when it
/// is spent waits for the oldest entry to age out.
pub struct Budget {
    pub name: &'static str,
    pub limit: usize,
    pub window: Duration,
    times: Mutex<VecDeque<Instant>>,
    waits: AtomicU64,
}

impl Budget {
    pub fn new(name: &'static str, limit: usize, window: Duration) -> Budget {
        Budget { name, limit: limit.max(1), window, times: Mutex::new(Default::default()), waits: AtomicU64::new(0) }
    }

    /// How much of the budget is in use right now.
    pub fn used(&self) -> usize {
        let mut t = self.times.lock().unwrap_or_else(|p| p.into_inner());
        while t.front().map(|f| f.elapsed() >= self.window).unwrap_or(false) {
            t.pop_front();
        }
        t.len()
    }

    /// Books one use without waiting (for things the ladder decides, like a match starting).
    pub fn record(&self) {
        self.times.lock().unwrap_or_else(|p| p.into_inner()).push_back(Instant::now());
    }

    /// When there will be room, if there is none now.
    pub fn room_at(&self) -> Option<Instant> {
        let mut t = self.times.lock().unwrap_or_else(|p| p.into_inner());
        while t.front().map(|f| f.elapsed() >= self.window).unwrap_or(false) {
            t.pop_front();
        }
        if t.len() < self.limit {
            None
        } else {
            t.front().map(|f| *f + self.window)
        }
    }

    /// Waits for room without booking anything. False when asked to stop meanwhile.
    pub fn wait_for_room(&self, stop: &dyn Fn() -> bool) -> bool {
        let mut said = false;
        while let Some(at) = self.room_at() {
            if !said {
                said = true;
                self.waits.fetch_add(1, Ordering::Relaxed);
                eprintln!("gateway: the {} budget is spent ({} in {} s); waiting {:.1} s", self.name, self.limit, self.window.as_secs(), at.saturating_duration_since(Instant::now()).as_secs_f64());
            }
            if stop() {
                return false;
            }
            std::thread::sleep(at.saturating_duration_since(Instant::now()).min(Duration::from_millis(250)));
        }
        true
    }

    /// Books one use if there is room right now; never waits.
    pub fn try_take(&self) -> bool {
        let mut t = self.times.lock().unwrap_or_else(|p| p.into_inner());
        while t.front().map(|f| f.elapsed() >= self.window).unwrap_or(false) {
            t.pop_front();
        }
        if t.len() >= self.limit {
            return false;
        }
        t.push_back(Instant::now());
        true
    }

    /// Waits for room, then books one use.
    pub fn take(&self, stop: &dyn Fn() -> bool) -> bool {
        if !self.wait_for_room(stop) {
            return false;
        }
        self.record();
        true
    }
}

/// What all bot threads share: the pacing of everything that goes to the ladder, the bot
/// processes that have attached and not yet been taken up, and each bot's health.
pub struct Shared {
    pub opt: Options,
    next_connect: Mutex<Instant>,
    next_queue: Mutex<Instant>,
    next_replace: Mutex<Instant>,
    pub connects: AtomicU64,
    pub drops: AtomicU64,
    pub replaced: AtomicU64,
    pub frames_out: AtomicU64,
    pub frames_in: AtomicU64,
    pub idle_turns: AtomicU64,
    pub refusals: AtomicU64,
    pub resyncs: AtomicU64,
    /// ladder addresses on which a socket was slow, most recent last
    pub slow: Mutex<Vec<std::net::IpAddr>>,
    /// the comms budget: new connections, queue requests and matches started
    pub connect_budget: Budget,
    pub queue_budget: Budget,
    pub match_budget: Budget,
    /// sockets given up for a better one: the cap on the gateway's own reconnects
    pub replace_budget: Budget,
    /// bot processes that have attached, by bot, waiting for the bot's thread to take them
    pub attach: Mutex<HashMap<String, Engine>>,
    /// each bot's health, as its thread last published it
    pub health: Mutex<HashMap<String, Value>>,
    /// which of our bots are queued (1) or in a match (2)
    active: Mutex<HashMap<String, u8>>,
    /// bots the ladder holds back from a match because our match limit is reached
    held: Mutex<std::collections::HashSet<String>>,
    /// the rotation queue: bots on a slow socket, waiting for their turn to get a new one
    rotation: Mutex<Vec<Waiting>>,
    /// when the ladder last dropped our connections (not counting the ones we closed)
    resets: Mutex<VecDeque<Instant>>,
    /// replacement sockets in a row that came out slow, and until when replacing is off
    replace_fails: AtomicU64,
    replace_suspended: Mutex<Option<Instant>>,
    /// the socket-test thresholds, in ms; re-read from `fleet/bots.json` while running
    pub ping_reject_ms: AtomicU64,
    pub slow_wire_ms: AtomicU64,
}

impl Shared {
    pub fn new(opt: Options, cfg: &Config) -> Shared {
        let now = Instant::now();
        Shared {
            rotation: Mutex::new(Vec::new()),
            resets: Mutex::new(VecDeque::new()),
            replace_fails: AtomicU64::new(0),
            replace_suspended: Mutex::new(None),
            ping_reject_ms: AtomicU64::new(opt.ping_reject_ms.max(0.0) as u64),
            slow_wire_ms: AtomicU64::new(opt.slow_wire_ms.max(0.0) as u64),
            connect_budget: Budget::new("connection", cfg.comms_connects_per_minute, Duration::from_secs(60)),
            queue_budget: Budget::new("queue request", cfg.comms_queues_per_minute, Duration::from_secs(60)),
            match_budget: Budget::new("match", cfg.comms_matches_per_day, Duration::from_secs(86_400)),
            replace_budget: Budget::new("socket replacement", opt.replacements_per_10min.max(1), Duration::from_secs(600)),
            opt,
            next_connect: Mutex::new(now),
            next_queue: Mutex::new(now),
            next_replace: Mutex::new(now),
            connects: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            replaced: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            frames_in: AtomicU64::new(0),
            idle_turns: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            slow: Mutex::new(Vec::new()),
            attach: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            held: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Takes the next free slot of a paced action and waits for it. Bots that ask at the
    /// same moment get slots one gap apart.
    fn pace(slot: &Mutex<Instant>, gap: Duration) {
        let at = {
            let mut next = slot.lock().unwrap_or_else(|p| p.into_inner());
            let at = (*next).max(Instant::now());
            *next = at + gap;
            at
        };
        let wait = at.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }

    /// Puts a bot whose socket is slow in the rotation queue (or updates why it is there).
    /// The flagship goes ahead of every other bot; the rest wait in the order they came.
    fn want_rotation(&self, bot: &str, flagship: bool, reason: String) {
        let mut q = self.rotation.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(w) = q.iter_mut().find(|w| w.bot == bot) {
            w.reason = reason;
            return;
        }
        let w = Waiting { bot: bot.to_string(), flagship, since: Instant::now(), since_ms: now_ms(), reason };
        let at = if flagship { q.iter().position(|w| !w.flagship).unwrap_or(q.len()) } else { q.len() };
        q.insert(at, w);
    }

    /// The ladder dropped one of our connections.
    fn note_reset(&self) {
        let mut r = self.resets.lock().unwrap_or_else(|p| p.into_inner());
        r.push_back(Instant::now());
        while r.len() > 64 {
            r.pop_front();
        }
    }

    /// How many of our connections the ladder dropped in the last two minutes. Two or more
    /// is a burst: then every reconnect is already a new socket (a fresh draw), and a
    /// rotation would only add connections while the ladder is shedding them.
    fn recent_resets(&self) -> usize {
        self.resets.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|t| t.elapsed() < Duration::from_secs(120)).count()
    }

    /// Takes a bot out of the rotation queue (its socket is good again, or gone).
    fn cancel_rotation(&self, bot: &str) {
        self.rotation.lock().unwrap_or_else(|p| p.into_inner()).retain(|w| w.bot != bot);
    }

    /// May this bot give up its socket for a new one now? Only the bot at the head of the
    /// queue, one rotation at a time across the fleet, `replace_gap` apart, inside the
    /// ten-minute cap, and none while rotating has been shown to be futile (see
    /// `replacement_came_out`). On yes the bot leaves the queue and gets the reason back.
    fn may_rotate(&self, bot: &str) -> Option<String> {
        let mut q = self.rotation.lock().unwrap_or_else(|p| p.into_inner());
        if q.first().map(|w| w.bot != bot).unwrap_or(true) {
            return None;
        }
        if self.recent_resets() >= 2 {
            return None;
        }
        if let Some(until) = *self.replace_suspended.lock().unwrap_or_else(|p| p.into_inner()) {
            if Instant::now() < until {
                return None;
            }
        }
        let mut next = self.next_replace.lock().unwrap_or_else(|p| p.into_inner());
        if Instant::now() < *next {
            return None;
        }
        if !self.replace_budget.try_take() {
            return None;
        }
        *next = Instant::now() + self.opt.replace_gap;
        Some(q.remove(0).reason)
    }

    /// Where a bot stands in the rotation queue, for its health line and the status page:
    /// whether a rotation is scheduled, its place, why, what it waits for, and when it
    /// should come (an estimate: the gate's next opening, then one gap per bot ahead).
    pub fn rotation_status(&self, bot: &str) -> Value {
        let q = self.rotation.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pos) = q.iter().position(|w| w.bot == bot) else {
            return json!({ "scheduled": false, "queue_length": q.len() });
        };
        let w = &q[pos];
        let now = Instant::now();
        let suspended = (*self.replace_suspended.lock().unwrap_or_else(|p| p.into_inner())).filter(|t| *t > now);
        let mut open = *self.next_replace.lock().unwrap_or_else(|p| p.into_inner());
        let mut waiting_for = if pos == 0 { "its next free moment: at connect, between matches or while waiting in the queue" } else { "the bots ahead of it" };
        if let Some(at) = self.replace_budget.room_at() {
            if at > open {
                open = at;
                waiting_for = "the cap on rotations in ten minutes, which is spent";
            }
        }
        if let Some(until) = suspended {
            if until > open {
                open = until;
                waiting_for = "rotating is paused: the last three new sockets were all slow";
            }
        }
        let resets = self.recent_resets();
        if resets >= 2 {
            // Until the last two resets are more than two minutes old.
            let r = self.resets.lock().unwrap_or_else(|p| p.into_inner());
            let second_last = r.iter().rev().nth(1).copied();
            drop(r);
            if let Some(t) = second_last {
                let until = t + Duration::from_secs(120);
                if until > open {
                    open = until;
                }
            }
            waiting_for = "a quiet spell: the ladder is resetting our connections, and each reconnect is already a new socket";
        }
        let eta = open.max(now) + self.opt.replace_gap * pos as u32;
        json!({
            "scheduled": true, "position": pos + 1, "queue_length": q.len(), "reason": w.reason, "since_ms": w.since_ms,
            "waiting_s": w.since.elapsed().as_secs(), "waiting_for": waiting_for,
            "eta_ms": now_ms() + eta.saturating_duration_since(now).as_millis() as i64,
        })
    }

    /// The whole queue, for the gateway's own state.
    fn rotation_queue(&self) -> Value {
        let q = self.rotation.lock().unwrap_or_else(|p| p.into_inner());
        json!(q.iter().map(|w| json!({ "bot": w.bot, "reason": w.reason, "since_ms": w.since_ms })).collect::<Vec<_>>())
    }

    /// What the socket that replaced a slow one turned out to be. Three slow ones in a row,
    /// fleet-wide, and replacing stops for twenty minutes: when every new socket is slow
    /// (as on 2026-11-18 from 16:30Z, after we had opened some eighty connections in half
    /// an hour), asking for more only adds connections.
    fn replacement_came_out(&self, root: &Path, good: bool) {
        if good {
            self.replace_fails.store(0, Ordering::Relaxed);
            return;
        }
        let fails = self.replace_fails.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= 3 {
            self.replace_fails.store(0, Ordering::Relaxed);
            *self.replace_suspended.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + Duration::from_secs(1200));
            log_line(root, "SOCKETS", "the last three replacement sockets were all slow: no socket is replaced for the next 20 minutes");
        }
    }

    fn set_held(&self, bot: &str, held: bool) {
        let mut h = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if held {
            h.insert(bot.to_string());
        } else {
            h.remove(bot);
        }
    }

    /// How many of our other bots the ladder is holding back from a match right now.
    fn others_held(&self, bot: &str) -> usize {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|n| n.as_str() != bot).count()
    }

    fn set_active(&self, bot: &str, state: u8) {
        let mut a = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if state == 0 {
            a.remove(bot);
        } else {
            a.insert(bot.to_string(), state);
        }
    }

    /// How many of our other bots are queued or in a match.
    fn others_active(&self, bot: &str) -> usize {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|(n, s)| n.as_str() != bot && **s > 0).count()
    }

    /// A bot's health: kept here for the gateway's verdict, and written to the store's
    /// `gateway_state` for everyone else.
    fn publish(&self, bot: &str, health: Value) {
        let level = health.get("level").and_then(Value::as_str).unwrap_or("bad").to_string();
        let verdict = health.get("verdict").and_then(Value::as_str).unwrap_or("?").to_string();
        let socket_id = health.get("socket").and_then(|s| s.get("socket_id")).and_then(Value::as_i64);
        tel().state(bot, &level, &verdict, socket_id, &health);
        self.health.lock().unwrap_or_else(|p| p.into_inner()).insert(bot.to_string(), health);
    }
}

/// A bot in the rotation queue.
struct Waiting {
    bot: String,
    flagship: bool,
    since: Instant,
    since_ms: i64,
    reason: String,
}

/// A frame waiting in a bot process's queue.
struct Queued {
    bytes: Vec<u8>,
    observation: bool,
    at: Instant,
}

/// A bot process that has attached over the local socket, with its queue and its health.
pub struct Engine {
    stream: UnixStream,
    buf: Vec<u8>,
    out: VecDeque<Queued>,
    /// bytes of the front frame already written
    out_off: usize,
    pub release: String,
    pub version: String,
    pub proto: i64,
    pub pid: i64,
    attached: Instant,
    // health
    answers: u64,
    late: u64,
    late_streak: u32,
    latencies_ms: VecDeque<f32>,
    dropped_observations: u64,
    // the status protocol (proto 2 and up)
    last_status: Instant,
    last_status_text: String,
    last_ping: Instant,
    next_ping_id: u64,
    last_pong: Instant,
    view: Option<(Option<String>, Option<i64>)>,
    disagreements: u32,
}

impl Engine {
    fn new(stream: UnixStream, release: String, version: String, proto: i64, pid: i64) -> Engine {
        let now = Instant::now();
        Engine {
            stream,
            buf: Vec::new(),
            out: VecDeque::new(),
            out_off: 0,
            release,
            version,
            proto,
            pid,
            attached: now,
            answers: 0,
            late: 0,
            late_streak: 0,
            latencies_ms: VecDeque::new(),
            dropped_observations: 0,
            last_status: now - Duration::from_secs(60),
            last_status_text: String::new(),
            last_ping: now,
            next_ping_id: 1,
            last_pong: now,
            view: None,
            disagreements: 0,
        }
    }

    /// Puts a frame in the process's queue and writes what the socket will take. A newer
    /// observation replaces older ones still waiting: the process should think about the
    /// present. False when the process has gone.
    fn push(&mut self, frame: &str, observation: bool) -> bool {
        if observation {
            let keep_front = self.out_off > 0;
            let before = self.out.len();
            let mut k = 0;
            self.out.retain(|q| {
                k += 1;
                !q.observation || (keep_front && k == 1)
            });
            self.dropped_observations += (before - self.out.len()) as u64;
        }
        let mut bytes = Vec::with_capacity(frame.len() + 1);
        bytes.extend_from_slice(frame.as_bytes());
        bytes.push(b'\n');
        self.out.push_back(Queued { bytes, observation, at: Instant::now() });
        self.flush()
    }

    /// Writes as much of the queue as the socket takes without blocking.
    fn flush(&mut self) -> bool {
        while let Some(front) = self.out.front() {
            match (&self.stream).write(&front.bytes[self.out_off..]) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out_off += n;
                    if self.out_off >= front.bytes.len() {
                        self.out.pop_front();
                        self.out_off = 0;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        true
    }

    /// How many frames wait and how long the oldest has waited.
    fn backlog(&self) -> (usize, Duration) {
        (self.out.len(), self.out.front().map(|q| q.at.elapsed()).unwrap_or(Duration::ZERO))
    }

    /// Whole lines the process has written since the last call. `Err` when it has gone.
    fn lines(&mut self) -> Result<Vec<String>, ()> {
        let mut chunk = [0u8; 8192];
        loop {
            match (&self.stream).read(&mut chunk) {
                Ok(0) => return Err(()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(()),
            }
        }
        let mut out = Vec::new();
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
            if !text.is_empty() {
                out.push(text);
            }
        }
        Ok(out)
    }

    fn answered(&mut self, ms: f64, late: bool) {
        if late {
            self.late += 1;
            self.late_streak += 1;
        } else {
            self.answers += 1;
            self.late_streak = 0;
            self.latencies_ms.push_back(ms as f32);
            if self.latencies_ms.len() > 300 {
                self.latencies_ms.pop_front();
            }
        }
    }

    fn latency(&self, q: f64) -> Option<f64> {
        if self.latencies_ms.is_empty() {
            return None;
        }
        let mut v: Vec<f32> = self.latencies_ms.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[((v.len() - 1) as f64 * q) as usize] as f64)
    }

    /// Lets the process go: its end of the socket closes, it exits, and whoever runs it
    /// starts the next one.
    fn detach(mut self, why: &str) {
        if self.proto >= 2 {
            self.push(&json!({ "type": "detach", "why": why }).to_string(), false);
        }
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

type Socket = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

fn socket_fd(ws: &Socket) -> Option<i32> {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s.as_raw_fd()),
        MaybeTlsStream::NativeTls(s) => Some(s.get_ref().as_raw_fd()),
        _ => None,
    }
}

fn tcp(ws: &Socket) -> Option<&std::net::TcpStream> {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::NativeTls(s) => Some(s.get_ref()),
        _ => None,
    }
}

fn connect(url: &str, token: &str, prefer: &[std::net::IpAddr], shared: &Shared) -> Result<(Socket, std::net::IpAddr), String> {
    // Addresses that were slow for any bot are tried last by every bot.
    let avoid = shared.slow.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let (ws, ip) = crate::net::connect(url, token, prefer, &avoid)?;
    if let Some(s) = tcp(&ws) {
        // Short read timeout: the loop waits in poll(), a read only drains what is there.
        let _ = s.set_read_timeout(Some(Duration::from_millis(1)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    }
    Ok((ws, ip))
}

fn send(ws: &mut Socket, shared: &Shared, text: String) -> Result<(), String> {
    shared.frames_out.fetch_add(1, Ordering::Relaxed);
    ws.send(Message::Text(text)).map_err(|e| format!("send failed: {e}"))
}

/// Waits until one of the descriptors has something to read, at most `timeout`.
fn wait_readable(fds: &[i32], timeout: Duration) {
    let mut set: Vec<libc::pollfd> = fds.iter().map(|fd| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }).collect();
    unsafe {
        libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, timeout.as_millis() as libc::c_int);
    }
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2]
}

/// The socket test at connect: three pings, the median round trip in milliseconds. Text
/// frames that arrive meanwhile are kept for the caller. `None` when no pong came back.
/// Pings a new socket. `closed` gets the reason when the ladder closes the socket during the
/// test (2026-11-18 21:34Z: three resume sockets in a row were closed at once, the reason was
/// thrown away, and the match was lost to the reconnect grace).
fn ping_test(ws: &mut Socket, early: &mut VecDeque<String>, pings: u8, wait: Duration, closed: &mut Option<String>) -> Option<f64> {
    let mut samples = Vec::new();
    for k in 0..pings {
        let sent = Instant::now();
        if let Err(e) = ws.send(Message::Ping(vec![b'g', b'w', k])) {
            *closed = Some(format!("send failed: {e}"));
            return None;
        }
        let mut got = false;
        while sent.elapsed() < wait && !got {
            if let Some(fd) = socket_fd(ws) {
                wait_readable(&[fd], Duration::from_millis(20));
            }
            loop {
                match ws.read() {
                    Ok(Message::Pong(p)) => {
                        if p.as_slice() == [b'g', b'w', k] {
                            samples.push(sent.elapsed().as_secs_f64() * 1000.0);
                            got = true;
                        }
                    }
                    Ok(Message::Text(t)) => early.push_back(t),
                    Ok(Message::Close(frame)) => {
                        *closed = Some(match frame {
                            Some(f) => format!("closed by the ladder: code {} {}", u16::from(f.code), f.reason),
                            None => "closed by the ladder (no reason given)".into(),
                        });
                        return None;
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
                    Err(e) => {
                        *closed = Some(format!("read failed: {e}"));
                        return None;
                    }
                }
            }
        }
        if !got {
            break;
        }
        if k + 1 < pings {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    if samples.is_empty() {
        None
    } else {
        Some(median(&samples))
    }
}

struct Plan {
    cfg: Config,
    bot: BotConfig,
    /// the version label the store says this bot plays under, when it says
    version: Option<String>,
    release: Option<String>,
}

/// What this bot should run as, or why the gateway should not run it at all.
fn plan(root: &Path, name: &str) -> Result<Plan, String> {
    let cfg = Config::load(root)?;
    let bot = cfg.bot(name).cloned().ok_or_else(|| format!("{name} is not in fleet/bots.json"))?;
    if !bot.gateway {
        return Err("not set to run through the gateway in fleet/bots.json".into());
    }
    let store = Store::open(&default_db(root), Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let st = store.bot_state(name).map_err(|e| e.to_string())?;
    if let Some(h) = st.as_ref().and_then(|s| s.halted.clone()) {
        return Err(format!("halted ({h})"));
    }
    let (release, version) = (st.as_ref().and_then(|s| s.release.clone()), st.as_ref().and_then(|s| s.version.clone()));
    if bot.managed && (release.is_none() || version.is_none()) {
        return Err("no release deployed".into());
    }
    Ok(Plan { cfg, bot, version, release })
}

fn event(root: &Path, bot: &str, kind: &str, detail: Value) {
    eprintln!("[{bot}] gateway: {kind} {detail}");
    if TEL.get().is_some() {
        // Written by the telemetry thread, in its once-a-second batch.
        tel().event(bot, kind, &detail);
        return;
    }
    if let Ok(store) = Store::open(&default_db(root), Duration::from_millis(500)) {
        store.event(Some(bot), &format!("gateway_{kind}"), &detail);
    }
}

fn queue_frame(version: &str) -> String {
    json!({ "type": "queue", "version": version }).to_string()
}


/// What is true about this bot on the ladder's side. It outlives a connection and a bot
/// process: this is the state a process is told and set back to.
#[derive(Default)]
struct Flow {
    in_match: bool,
    match_id: Option<String>,
    tick: Option<i64>,
    last_hello: Option<String>,
    last_obs: Option<String>,
    last_turn: Option<Instant>,
    last_result: Option<Value>,
    matches: u64,
    /// the last tick seen before the connection dropped mid-match (to count the turns the
    /// ladder played idle for us until the match resumed)
    dropped_at_tick: Option<i64>,
    /// the match as it passes, written to `data/fights` (see `fights`)
    record: Option<crate::fights::FightRecord>,
}

/// Runs one bot until asked to stop: its connection to the ladder, the bot process attached
/// to it, and everything between.
pub fn run_bot(root: &Path, name: &str, shared: &Shared, stop: &dyn Fn() -> bool) {
    let opt = shared.opt.clone();
    // The same lock a bot's own client takes: one connection per token, whoever holds it.
    let lock_dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".victris").join("run");
    let _ = std::fs::create_dir_all(&lock_dir);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_dir.join(format!("{name}.lock")));
    let mut said = String::new();
    let mut note = |text: String| {
        if said != text {
            eprintln!("[{name}] gateway: {text}");
            said = text;
        }
    };
    let Ok(lock) = lock else {
        note("cannot open the bot's lock file".into());
        return;
    };
    while lock.try_lock().is_err() {
        note("another process holds this bot's connection; waiting for it to let go".into());
        shared.publish(name, json!({ "bot": name, "verdict": "another process holds this bot's connection", "level": "bad" }));
        if !nap(Duration::from_secs(2), stop) {
            return;
        }
    }

    let mut engine: Option<Engine> = None;
    let mut flow = Flow::default();
    let mut backoff = Duration::ZERO;
    // the socket in use: its ID in the store's `sockets` table
    let mut sid: Option<i64> = None;
    // the last connection was given up for being slow (its ID); the next one's test says if that paid
    let mut after_replacement = false;
    let mut rotated_from: Option<i64> = None;
    let mut down_why = String::from("not connected yet");
    // When the ladder last said another connection took this bot over ("replaced").
    let mut last_replaced: Option<Instant> = None;
    // A socket that loses turns on the wire in a match is given up once in that match: the
    // match resumes on a new socket in about two seconds (2026-11-19 01:00Z: one socket cost
    // quarter-master 38 turns in a game, another the flagship 50, while the rotation waited
    // for the match to end). The match and how many turns the wire lost in it on this bot.
    let mut wire_lost_in_match: (Option<String>, u32) = (None, 0);
    let mut swapped_in_match: Option<String> = None;
    let mut hello_fields_said = false;

    // Health as one JSON object; `conn` is the connection's part, when there is one.
    let health = |flow: &Flow, engine: &Option<Engine>, conn: Option<Value>, label: &Option<String>, role: &str, down_why: &str, backoff: Duration| -> Value {
        let (verdict, level): (String, &str) = match (&conn, engine) {
            (None, _) => (format!("socket down ({down_why}); next try in {} s", backoff.as_secs()), "bad"),
            (Some(_), None) if flow.in_match => ("no bot process attached; the match is being played idle".into(), "bad"),
            (Some(_), None) => ("no bot process attached".into(), "bad"),
            (Some(c), Some(_)) => {
                if flow.in_match {
                    match flow.last_turn.map(|t| t.elapsed().as_secs_f64()) {
                        Some(s) if s > 5.0 && c.get("match_s").and_then(Value::as_f64).unwrap_or(0.0) > 5.0 => (format!("in a match but no turn answered for {s:.0} s"), "bad"),
                        _ => ("playing".into(), "ok"),
                    }
                } else if c.get("queued").and_then(Value::as_bool) == Some(true) {
                    let s = c.get("queued_s").and_then(Value::as_f64).unwrap_or(0.0);
                    if s >= opt.stall_after.as_secs_f64() {
                        (format!("stalled {s:.0} s: {}", c.get("stall_cause").and_then(Value::as_str).unwrap_or("queued, no match offered")), "warn")
                    } else {
                        (format!("waiting for a match, {s:.0} s"), "ok")
                    }
                } else {
                    ("between matches".into(), "ok")
                }
            }
        };
        json!({
            "bot": name, "role": role, "verdict": verdict, "level": level, "label": label, "ts_ms": now_ms(),
            "socket": conn.unwrap_or(json!({ "connected": false, "why": down_why, "retry_in_s": backoff.as_secs() })),
            "flow": {
                "in_match": flow.in_match, "match": flow.match_id, "tick": flow.tick, "matches": flow.matches,
                "last_turn_s": flow.last_turn.map(|t| (t.elapsed().as_secs_f64() * 10.0).round() / 10.0), "last_result": flow.last_result,
            },
            "process": engine.as_ref().map(|e| {
                let (n, age) = e.backlog();
                json!({
                    "attached": true, "release": e.release, "version": e.version, "proto": e.proto, "pid": e.pid, "attached_s": e.attached.elapsed().as_secs(),
                    "answers": e.answers, "late": e.late, "answer_ms_p50": e.latency(0.5).map(|x| x.round()), "answer_ms_p99": e.latency(0.99).map(|x| x.round()),
                    "queue": n, "queue_age_ms": age.as_millis() as u64, "stale_observations_dropped": e.dropped_observations,
                })
            }).unwrap_or(json!({ "attached": false })),
        })
    };

    'outer: while !stop() {
        // A socket that closed without passing the drop code below (a halt, a refusal).
        if let Some(s) = sid.take() {
            tel().socket_closed(s, "closed by the gateway");
            shared.cancel_rotation(name);
        }
        let p = match plan(root, name) {
            Ok(p) => p,
            Err(why) => {
                note(format!("not running: {why}"));
                shared.publish(name, json!({ "bot": name, "verdict": format!("not running: {why}"), "level": "bad", "ts_ms": now_ms() }));
                if let Some(e) = engine.take() {
                    e.detach(&why);
                }
                shared.set_active(name, 0);
                shared.cancel_rotation(name);
                nap(Duration::from_secs(5), stop);
                continue;
            }
        };
        let token = match std::fs::read_to_string(root.join("bots").join(name)) {
            Ok(t) => t.split_whitespace().collect::<String>(),
            Err(_) => {
                note(format!("not running: no token file bots/{name}"));
                shared.publish(name, json!({ "bot": name, "verdict": format!("not running: no token file bots/{name}"), "level": "bad", "ts_ms": now_ms() }));
                nap(Duration::from_secs(15), stop);
                continue;
            }
        };
        let url = p.bot.url.clone().unwrap_or_else(|| DEFAULT_URL.to_string());
        let role = p.bot.role.clone();
        let mut label: Option<String> = p.version.clone().or_else(|| engine.as_ref().map(|e| e.version.clone()));

        // A bot process may attach while there is no connection.
        if let Some(e) = shared.attach.lock().unwrap_or_else(|p| p.into_inner()).remove(name) {
            event(root, name, "process_attached", json!({ "release": e.release, "version": e.version, "proto": e.proto, "pid": e.pid, "in_match": flow.in_match }));
            if let Some(old) = engine.replace(e) {
                old.detach("another bot process attached");
            }
        }

        // Connect, in the fleet's turn.
        {
            let mut h = health(&flow, &engine, None, &label, &role, &down_why, backoff);
            h["rotation"] = shared.rotation_status(name);
            shared.publish(name, h);
        }
        if !backoff.is_zero() && !nap(backoff, stop) {
            break;
        }
        if !shared.connect_budget.take(stop) {
            break;
        }
        // A match waiting to be resumed has ten seconds before the ladder gives up on it, and
        // every second costs a turn played idle: it goes at once. Every other connection
        // waits its turn, one every `connect_gap` across the fleet.
        if !flow.in_match {
            Shared::pace(&shared.next_connect, opt.connect_gap);
        }
        shared.connects.fetch_add(1, Ordering::Relaxed);
        let prefer: Vec<std::net::IpAddr> = p.cfg.prefer_addresses.iter().filter_map(|a| a.parse().ok()).collect();
        let (mut ws, remote) = match connect(&url, &token, &prefer, shared) {
            Ok(x) => x,
            Err(why) => {
                backoff = if flow.in_match { Duration::from_millis(300) } else { (backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30)) };
                event(root, name, "connect_failed", json!({ "why": why, "retry_in_s": backoff.as_secs_f64(), "in_match": flow.in_match }));
                down_why = why;
                continue;
            }
        };
        let connected = Instant::now();

        // The socket test: three pings before queueing. When a match waits to be resumed
        // every second counts, so there it is one ping and a short wait.
        let mut inbox: VecDeque<String> = VecDeque::new();
        let mut ping_ms: Option<f64>;
        let reject = shared.ping_reject_ms.load(Ordering::Relaxed) as f64;
        let resuming = flow.in_match;
        let mut closed_at_connect: Option<String> = None;
        ping_ms = if !resuming {
            ping_test(&mut ws, &mut inbox, 3, Duration::from_millis(1500), &mut closed_at_connect)
        } else {
            ping_test(&mut ws, &mut inbox, 1, Duration::from_millis(400), &mut closed_at_connect)
        };
        if let Some(why) = closed_at_connect {
            // The ladder closed it before anything was played: record why, and try again
            // (at once while a match waits to be resumed, with the usual backoff otherwise).
            let this = telemetry::socket_opened(root, name, &remote.to_string(), None, resuming, rotated_from.take());
            tel().socket_closed(this, &format!("at connect: {why}"));
            event(root, name, "closed_at_connect", json!({ "socket": this, "remote": remote.to_string(), "why": why, "in_match": flow.in_match }));
            backoff = if flow.in_match { Duration::from_millis(300) } else { (backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30)) };
            down_why = format!("closed at connect: {why}");
            continue;
        }
        // Every socket is registered, slow or not: the store counts how many a bot was given.
        let this = telemetry::socket_opened(root, name, &remote.to_string(), ping_ms, resuming, rotated_from.take());
        sid = Some(this);
        if let Some(ms) = ping_ms {
            tel().point(this, "ping_ms", ms);
        }
        if let (true, Some(ms), true) = (after_replacement, ping_ms, reject > 0.0) {
            // This socket replaced a slow one: was that worth it?
            after_replacement = false;
            shared.replacement_came_out(root, ms <= reject);
        }
        if let Some(ms) = ping_ms.filter(|ms| reject > 0.0 && *ms > reject) {
            // A slow socket. The bot joins the rotation queue; if it is its turn, and no match
            // is waiting on this socket, the socket is given up now, before anything is played
            // on it. Otherwise the bot plays on it and rotates when its turn comes.
            shared.want_rotation(name, role == "flagship", format!("slow ping, {ms:.0} ms at connect"));
            let hello = inbox.iter().any(|t| t.contains("\"hello\""));
            if !resuming && !hello {
                if let Some(reason) = shared.may_rotate(name) {
                    after_replacement = true;
                    rotated_from = sid.take();
                    tel().socket_closed(this, &format!("rotated at connect: {reason}"));
                    shared.replaced.fetch_add(1, Ordering::Relaxed);
                    event(root, name, "rotated", json!({ "socket": this, "remote": remote.to_string(), "ping_ms": ms.round(), "when": "at connect" }));
                    let _ = ws.close(None);
                    down_why = format!("rotated at connect (ping {ms:.0} ms)");
                    backoff = Duration::ZERO;
                    continue;
                }
            }
        } else if reject > 0.0 && ping_ms.is_some() {
            shared.cancel_rotation(name);
        }
        event(root, name, "connected", json!({ "socket": this, "remote": remote.to_string(), "ping_ms": ping_ms.map(|x| x.round()), "resuming": flow.in_match }));

        // -- this connection ----------------------------------------------------------------
        let mut queued = false;
        let mut queued_since: Option<Instant> = None;
        let mut queue_due: Option<Instant> = Some(Instant::now());
        // A match was in progress when the last connection went: the ladder's guide says to
        // queue as usual, and the ladder sends that match's start and its current observation
        // again. If it has not by this time, the match is over without us.
        let mut resume_until: Option<Instant> = flow.in_match.then(|| Instant::now() + Duration::from_secs(12));
        // the observation the bot process owes an answer to
        let mut pending: Option<(i64, Instant)> = None;
        // the process's quick answer for the tick it is working on
        let mut fallback: Option<(i64, String)> = None;
        // a match just ended and the process has not yet written its last word on it
        let mut after_end: Option<Instant> = None;
        let mut hello_at: Option<Instant> = None;
        let mut ready_sent = false;
        let mut match_started: Option<Instant> = None;
        let mut last_heard = Instant::now();
        let mut last_ping = Instant::now();
        let mut ping_sent: Option<Instant> = None;
        let mut last_slow_check = Instant::now();
        let mut no_process_since: Option<Instant> = None;
        // keepalive pings in a row that were slower than the limit (the test at connect counts)
        let mut slow_pings: u32 = match ping_ms {
            Some(ms) if shared.ping_reject_ms.load(Ordering::Relaxed) > 0 && ms > shared.ping_reject_ms.load(Ordering::Relaxed) as f64 => 2,
            _ => 0,
        };
        let mut last_tick_1s = Instant::now() - Duration::from_secs(2);
        // socket testing in play
        let mut last_budget: Option<i64> = None;
        let mut answered: Option<(i64, f64)> = None;
        let mut wires: Vec<f64> = Vec::new();
        let mut lost = 0u32;
        let mut wire_ms: Option<f64> = None;
        // refusals and stalls
        let mut refusal: Option<(String, Instant, Value)> = None;
        let mut soft_refusals = 0u32;
        let mut stall_said = false;
        let mut stall_cause = String::new();
        let mut voluntary = false;
        let why_closed: String;

        'conn: loop {
            // -- a bot process that has just attached ---------------------------------------
            if let Some(mut e) = shared.attach.lock().unwrap_or_else(|p| p.into_inner()).remove(name) {
                event(root, name, "process_attached", json!({ "release": e.release, "version": e.version, "proto": e.proto, "pid": e.pid, "in_match": flow.in_match }));
                if flow.in_match {
                    // Set it to the truth: the match start, then the latest observation.
                    shared.resyncs.fetch_add(1, Ordering::Relaxed);
                    if e.proto >= 2 {
                        e.push(&json!({ "type": "sync", "why": "attached during a match", "match": flow.match_id, "tick": flow.tick }).to_string(), false);
                    }
                    if let Some(h) = &flow.last_hello {
                        e.push(h, false);
                    }
                    if let Some(o) = &flow.last_obs {
                        e.push(o, true);
                    }
                }
                if let Some(old) = engine.replace(e) {
                    old.detach("another bot process attached");
                }
                if label.is_none() {
                    label = engine.as_ref().map(|e| e.version.clone());
                }
                if !flow.in_match && !queued && queue_due.is_none() {
                    queue_due = Some(Instant::now());
                }
            }

            // -- what the ladder sent --------------------------------------------------------
            loop {
                match ws.read() {
                    Ok(Message::Text(text)) => inbox.push_back(text),
                    Ok(Message::Close(_)) => {
                        why_closed = "the ladder closed the connection".into();
                        break 'conn;
                    }
                    Ok(Message::Pong(_)) => {
                        last_heard = Instant::now();
                        if let Some(at) = ping_sent.take() {
                            let ms = at.elapsed().as_secs_f64() * 1000.0;
                            ping_ms = Some(ms);
                            tel().point(this, "ping_ms", ms);
                            let reject = shared.ping_reject_ms.load(Ordering::Relaxed) as f64;
                            slow_pings = if reject > 0.0 && ms > reject { slow_pings + 1 } else { 0 };
                        }
                    }
                    Ok(_) => last_heard = Instant::now(),
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
                    Err(e) => {
                        why_closed = format!("read failed: {e}");
                        break 'conn;
                    }
                }
            }
            while let Some(text) = inbox.pop_front() {
                last_heard = Instant::now();
                shared.frames_in.fetch_add(1, Ordering::Relaxed);
                let msg: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let kind = msg.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                let mut forward: Option<(String, bool)> = Some((text.clone(), false));
                match kind.as_str() {
                    "hello" => {
                        let id = msg.get("match_id").and_then(Value::as_str).unwrap_or("").to_string();
                        if flow.match_id.as_deref() == Some(id.as_str()) {
                            if let Some(r) = flow.record.as_mut() {
                                r.note("connected", json!({ "socket": this, "resumed": true, "remote": remote.to_string() }));
                            }
                        } else {
                            if let Some(r) = flow.record.take() {
                                r.finish("superseded by the next match", None);
                            }
                            let release = engine.as_ref().map(|e| e.release.clone()).unwrap_or_default();
                            flow.record = crate::fights::FightRecord::open(root, name, &id, label.as_deref().unwrap_or("?"), &release, Some(this));
                        }
                        if flow.match_id.as_deref() != Some(id.as_str()) || !flow.in_match {
                            if flow.match_id.as_deref() != Some(id.as_str()) {
                                flow.dropped_at_tick = None;
                                shared.match_budget.record();
                                flow.matches += 1;
                            }
                            flow.match_id = Some(id);
                            wires.clear();
                            lost = 0;
                            match_started = Some(Instant::now());
                        }
                        flow.in_match = true;
                        flow.tick = None;
                        flow.last_obs = None;
                        shared.set_active(name, 2);
                        if stall_said {
                            log_line(root, "STALL", &format!("over: {name} was given a match after {} s in the queue", queued_since.map(|t| t.elapsed().as_secs()).unwrap_or(0)));
                            stall_said = false;
                        }
                        queued = false;
                        queued_since = None;
                        queue_due = None;
                        after_end = None;
                        refusal = None;
                        shared.set_held(name, false);
                        soft_refusals = 0;
                        last_budget = None;
                        answered = None;
                        pending = None;
                        fallback = None;
                        hello_at = Some(Instant::now());
                        ready_sent = false;
                        resume_until = None;
                        if !hello_fields_said {
                            // Once: what a match start carries, so we know what the socket tells us.
                            hello_fields_said = true;
                            let keys: Vec<String> = msg.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                            event(root, name, "hello_fields", json!({ "keys": keys, "opponent": msg.get("opponent"), "deadlines": msg.get("deadlines"), "you": msg.get("you") }));
                        }
                        // The process cannot see the socket, so the hello says which address it is on.
                        let mut m = msg.clone();
                        m["_remote"] = json!(remote.to_string());
                        let t = m.to_string();
                        flow.last_hello = Some(t.clone());
                        forward = Some((t, false));
                        if engine.is_none() {
                            // Nobody to say ready: say it, and play idle until a process attaches.
                            if let Some(r) = flow.record.as_mut() {
                                r.frame_out("gateway", r#"{"type":"ready"}"#);
                            }
                            if let Err(why) = send(&mut ws, shared, json!({ "type": "ready" }).to_string()) {
                                why_closed = why;
                                break 'conn;
                            }
                            ready_sent = true;
                        }
                    }
                    "observation" => {
                        let tick = msg.get("tick").and_then(Value::as_i64);
                        let budget = msg.get("self").and_then(|s| s.get("budget_ms_left")).and_then(Value::as_i64);
                        // The socket test in play: what the ladder charged for the last turn,
                        // less the time the bot process took, is the time on the wire.
                        if let (Some(prev), Some(b), Some(t)) = (last_budget, budget, tick) {
                            let charge = (prev - b).clamp(0, 60_000) as f64;
                            if let Some((at, took)) = answered {
                                if at == t - 1 {
                                    wires.push((charge - took).max(0.0));
                                    tel().point(this, "charge_ms", charge);
                                    tel().point(this, "answer_ms", took);
                                    tel().point(this, "wire_ms", (charge - took).max(0.0));
                                    if charge >= 1000.0 && took < 600.0 {
                                        lost += 1;
                                        if wire_lost_in_match.0 != flow.match_id {
                                            wire_lost_in_match = (flow.match_id.clone(), 0);
                                        }
                                        wire_lost_in_match.1 += 1;
                                        { let d = json!({ "cause": "wire", "socket": this, "match": flow.match_id, "tick": t - 1, "turns": 1, "charge_ms": charge, "answer_ms": took.round() }); if let Some(r) = flow.record.as_mut() { r.note("turn_lost", d.clone()); } event(root, name, "turn_lost", d); }
                                    }
                                }
                            }
                        }
                        if let (Some(old), Some(t)) = (flow.dropped_at_tick.take(), tick) {
                            if t - old > 1 {
                                { let d = json!({ "cause": "reset", "socket": this, "match": flow.match_id, "tick": old + 1, "turns": t - old - 1 }); if let Some(r) = flow.record.as_mut() { r.note("turn_lost", d.clone()); } event(root, name, "turn_lost", d); }
                            }
                        }
                        last_budget = budget;
                        answered = None;
                        if flow.in_match && wire_lost_in_match.0 == flow.match_id && wire_lost_in_match.1 >= 3 && swapped_in_match != flow.match_id && opt.swap_in_match {
                            swapped_in_match = flow.match_id.clone();
                            let n = wire_lost_in_match.1;
                            event(root, name, "rotated", json!({ "socket": this, "remote": remote.to_string(), "wire_ms": wire_ms.map(|x| x.round()), "when": "in a match", "turns_lost": n, "match": flow.match_id }));
                            let _ = ws.close(None);
                            voluntary = true;
                            why_closed = format!("replaced in a match: the wire lost {n} turns on it");
                            break 'conn;
                        }
                        flow.tick = tick;
                        flow.last_obs = Some(text.clone());
                        pending = tick.map(|t| (t, Instant::now()));
                        fallback = None;
                        forward = Some((text.clone(), true));
                        if wires.len() >= 20 && wires.len() % 5 == 0 || lost >= 2 {
                            let m = median(&wires);
                            wire_ms = Some(m);
                        }
                    }
                    "end" => {
                        flow.in_match = false;
                        flow.last_hello = None;
                        flow.last_obs = None;
                        flow.tick = None;
                        flow.last_result = Some(json!({ "match": flow.match_id, "result": msg.get("result"), "reason": msg.get("reason"), "rating_change": msg.get("rating_change"), "final_tick": msg.get("final_tick"), "ended_ms": now_ms() }));
                        pending = None;
                        fallback = None;
                        hello_at = None;
                        match_started = None;
                        shared.set_active(name, 0);
                        if wires.len() >= 20 {
                            wire_ms = Some(median(&wires));
                        }
                        // The process writes the match down before anything else happens.
                        after_end = Some(Instant::now());
                    }
                    "queued" => {
                        queued = true;
                        queued_since.get_or_insert_with(Instant::now);
                    }
                    "error" => {
                        let code = msg.get("code").and_then(Value::as_str).unwrap_or("?").to_string();
                        if code == "version_limit" {
                            // The ladder will not take this version label any more: the next
                            // label, same release, on the same socket. The bot process is let go
                            // so that it comes back knowing its new label.
                            let old = label.clone().unwrap_or_default();
                            let release = p.release.clone().or_else(|| engine.as_ref().map(|e| e.release.clone())).unwrap_or_else(|| "unmanaged".into());
                            if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
                                if let Ok(next) = store.next_version(name) {
                                    // Never the refused label again, whatever the store knows.
                                    let n = |v: &str| v.strip_prefix('v').and_then(|x| x.parse::<i64>().ok());
                                    let next = match (n(&next), n(&old)) {
                                        (Some(a), Some(b)) if a <= b => format!("v{}", b + 1),
                                        _ => next,
                                    };
                                    let _ = store.set_deployment(name, &release, &next, "the ladder refused the old version label");
                                    log_line(root, "PIPELINE", &format!("{name}: the ladder refused version label {old}; the gateway now queues it as {next}"));
                                    label = Some(next);
                                }
                            }
                            if let Some(e) = engine.take() {
                                e.detach("the version label changed");
                            }
                            queued = false;
                            queued_since = None;
                            shared.set_active(name, 0);
                            queue_due = Some(Instant::now() + Duration::from_secs(2));
                            forward = None;
                        } else if code == "replaced" && last_replaced.map(|t| t.elapsed() > Duration::from_secs(15 * 60)).unwrap_or(true) {
                            // 2026-11-18 21:43Z: the ladder said "replaced" to spy-master's only
                            // connection, right after a reset and a resume; no other client
                            // existed and the bot sat halted for half an hour. The first time in
                            // fifteen minutes it is taken for the ladder's confusion over the
                            // reset: wait a minute and connect again. A second time halts it.
                            last_replaced = Some(Instant::now());
                            log_line(root, "WARN", &format!("{name}: the ladder said another connection took over (replaced); none of ours did, so it connects again in 60 s (a second time within 15 min halts it)"));
                            let _ = ws.close(None);
                            flow.in_match = false;
                            shared.set_active(name, 0);
                            down_why = "the ladder said another connection took over (replaced); trying again in 60 s".into();
                            backoff = Duration::from_secs(60);
                            continue 'outer;
                        } else if FATAL_CODES.contains(&code.as_str()) {
                            if let Some(e) = engine.as_mut() {
                                e.push(&text, false);
                            }
                            if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
                                let _ = store.set_halted(name, Some(&format!("the ladder refused the bot: {code}")));
                            }
                            log_line(root, "ALERT", &format!("{name} is halted: the ladder refused it ({code}). `fleet resume {name}` starts it again."));
                            let _ = ws.close(None);
                            flow.in_match = false;
                            shared.set_active(name, 0);
                            down_why = format!("the ladder refused the bot ({code})");
                            continue 'outer;
                        } else if code == "quota_exceeded" {
                            // The ladder's guide: the bot stays queued and plays as soon as the
                            // quota allows. Wait; do not queue again, do not reconnect.
                            shared.refusals.fetch_add(1, Ordering::Relaxed);
                            refusal = Some((code.clone(), Instant::now(), json!({ "quota": msg.get("quota"), "limit": msg.get("limit"), "used": msg.get("used"), "retry_after_s": msg.get("retry_after_s") })));
                            if msg.get("message").and_then(Value::as_str).map(|m| m.contains("concurrent")).unwrap_or(true) {
                                shared.set_held(name, true);
                            }
                            queued = true;
                            queued_since.get_or_insert_with(Instant::now);
                        } else if code == "already_queued" {
                            queued = true;
                            queued_since.get_or_insert_with(Instant::now);
                        } else if code == "not_in_match" {
                            // Harmless: a last turn that arrived after the match ended.
                        } else if !flow.in_match {
                            // Some other refusal while waiting: ask again, later each time.
                            soft_refusals += 1;
                            shared.refusals.fetch_add(1, Ordering::Relaxed);
                            refusal = Some((code.clone(), Instant::now(), json!({ "message": msg.get("message") })));
                            let wait = msg.get("retry_after_s").and_then(Value::as_u64).unwrap_or((2 + soft_refusals as u64).min(30)).clamp(1, 120);
                            queued = false;
                            queued_since = None;
                            shared.set_active(name, 0);
                            queue_due = Some(Instant::now() + Duration::from_secs(wait));
                        }
                    }
                    _ => {}
                }
                if let Some(r) = flow.record.as_mut() {
                    r.frame_in(this, &text, msg.get("tick").and_then(Value::as_i64));
                }
                if kind == "end" {
                    if let Some(r) = flow.record.take() {
                        r.finish("end", Some(&msg));
                    }
                }
                // The bot process sees every frame, so its own log and tracker stay whole.
                if let Some((frame, is_obs)) = forward {
                    let gone = match engine.as_mut() {
                        Some(e) => !e.push(&frame, is_obs),
                        None => false,
                    };
                    if gone {
                        if let Some(e) = engine.take() {
                            e.detach("its socket closed");
                        }
                        event(root, name, "process_gone", json!({ "in_match": flow.in_match }));
                    }
                }
            }

            // -- what the bot process wrote -----------------------------------------------------
            let lines = match engine.as_mut().map(|e| e.lines()) {
                Some(Ok(lines)) => lines,
                Some(Err(())) => {
                    // It has gone. Whoever runs it starts another, which attaches and is set
                    // to the truth; until then the clock below answers for it.
                    if let Some(e) = engine.take() {
                        e.detach("it went away");
                    }
                    event(root, name, "process_gone", json!({ "in_match": flow.in_match }));
                    Vec::new()
                }
                None => Vec::new(),
            };
            let mut process_spoke = false;
            for line in lines {
                process_spoke = true;
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match v.get("type").and_then(Value::as_str) {
                    // The gateway decides when to queue and whether a socket is good; older
                    // processes still say what they would have done.
                    Some("queue") | Some("reconnect") | Some("leave") | Some("fatal") => continue,
                    Some("ready") => {
                        if flow.in_match && !ready_sent {
                            ready_sent = true;
                            if let Some(r) = flow.record.as_mut() {
                                r.frame_out("process", &line);
                            }
                            if let Err(why) = send(&mut ws, shared, line) {
                                why_closed = why;
                                break 'conn;
                            }
                        }
                        continue;
                    }
                    Some("pong") => {
                        if let Some(e) = engine.as_mut() {
                            e.last_pong = Instant::now();
                            let view = (v.get("match").and_then(Value::as_str).map(str::to_string), v.get("tick").and_then(Value::as_i64));
                            // Out of step: it is in another match than the ladder says, or in
                            // one when there is none.
                            let agrees = if flow.in_match { view.0 == flow.match_id } else { view.0.is_none() };
                            e.disagreements = if agrees { 0 } else { e.disagreements + 1 };
                            e.view = Some(view);
                        }
                        continue;
                    }
                    Some(_) => continue,
                    None => {}
                }
                // The process's quick fallback for this tick: kept, not sent. The ladder takes
                // the first answer for a tick as final (tested 2026-11-18), so it only goes
                // out if the real turn has not come by the deadline.
                if v.get("provisional").and_then(Value::as_bool) == Some(true) {
                    if let Some(tick) = v.get("tick").and_then(Value::as_i64) {
                        let mut f = v.clone();
                        if let Some(o) = f.as_object_mut() {
                            o.remove("provisional");
                        }
                        fallback = Some((tick, f.to_string()));
                    }
                    continue;
                }
                let Some(tick) = v.get("tick").and_then(Value::as_i64) else { continue };
                match pending {
                    Some((want, since)) if want == tick => {
                        pending = None;
                        let took = since.elapsed().as_secs_f64() * 1000.0;
                        answered = Some((tick, took));
                        flow.last_turn = Some(Instant::now());
                        if let Some(e) = engine.as_mut() {
                            e.answered(took, false);
                        }
                    }
                    // A turn for a tick already answered, or a stale one: never sent.
                    _ => continue,
                }
                if let Some(r) = flow.record.as_mut() {
                    r.frame_out("process", &line);
                }
                if let Err(why) = send(&mut ws, shared, line) {
                    why_closed = why;
                    break 'conn;
                }
            }

            // -- between two matches -------------------------------------------------------------
            if let Some(since) = after_end {
                if process_spoke || engine.is_none() || since.elapsed() > Duration::from_millis(500) {
                    after_end = None;
                    if stop() {
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        let _ = ws.close(None);
                        break 'outer;
                    }
                    match plan(root, name) {
                        Ok(np) => {
                            // The moment to change bot processes: the old one has written its
                            // match. It is let go; the one for the new release attaches.
                            let stale = np.bot.managed && engine.as_ref().map(|e| Some(&e.release) != np.release.as_ref() || Some(&e.version) != np.version.as_ref()).unwrap_or(false);
                            if stale {
                                if let Some(e) = engine.take() {
                                    event(root, name, "process_swap", json!({ "from": e.release, "to": np.release, "version": np.version }));
                                    e.detach("a new release is deployed");
                                }
                            }
                            if np.version.is_some() {
                                label = np.version.clone();
                            }
                            // Between two matches a slow socket costs nothing to give up.
                            if let Some(reason) = shared.may_rotate(name) {
                                event(root, name, "rotated", json!({ "socket": this, "remote": remote.to_string(), "ping_ms": ping_ms.map(|x| x.round()), "wire_ms": wire_ms.map(|x| x.round()), "when": "between matches" }));
                                let _ = ws.close(None);
                                voluntary = true;
                                why_closed = format!("rotated between matches: {reason}");
                                break 'conn;
                            }
                            // The scouts first (Mike, 2026-11-19): the flagship leaves the freed
                            // match slot alone for a moment, the scouts ask at once.
                            let gap = if np.bot.role == "flagship" { np.cfg.queue_gap_ms.max(3000) } else { 0 };
                            queue_due = Some(Instant::now() + Duration::from_millis(gap));
                        }
                        Err(why) => {
                            // Halted or taken out: leave the ladder until that changes.
                            note(format!("not running: {why}"));
                            let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                            let _ = ws.close(None);
                            down_why = why;
                            continue 'outer;
                        }
                    }
                }
            }

            // -- the clock ---------------------------------------------------------------------
            if let Some((tick, since)) = pending {
                if since.elapsed() >= opt.answer_deadline || (engine.is_none() && since.elapsed() >= Duration::from_millis(300)) {
                    // The bot process is late or absent. Its quick fallback for this tick goes
                    // out if it gave one; an empty turn if not. Either keeps the bot in the match.
                    pending = None;
                    shared.idle_turns.fetch_add(1, Ordering::Relaxed);
                    let waited = since.elapsed().as_secs_f64() * 1000.0;
                    answered = Some((tick, waited));
                    let (frame, kind) = match fallback.take() {
                        Some((t, f)) if t == tick => (f, "fallback_turn"),
                        _ => (json!({ "tick": tick }).to_string(), "idle_turn"),
                    };
                    // The turn was not the bot's own: its process was late, or there was none.
                    { let d = json!({ "cause": if engine.is_none() { "no process" } else { "late answer" }, "sent": kind, "socket": this, "match": flow.match_id, "tick": tick, "turns": 1, "waited_ms": waited.round() }); if let Some(r) = flow.record.as_mut() { r.note("turn_lost", d.clone()); } event(root, name, "turn_lost", d); }
                    if let Some(r) = flow.record.as_mut() {
                        r.frame_out("gateway", &frame);
                    }
                    if let Err(why) = send(&mut ws, shared, frame) {
                        why_closed = why;
                        break 'conn;
                    }
                    let mut unresponsive = false;
                    if let Some(e) = engine.as_mut() {
                        e.answered(waited, true);
                        unresponsive = e.late_streak >= 30;
                        if e.late_streak <= 3 || e.late_streak % 10 == 0 {
                            event(root, name, kind, json!({ "tick": tick, "waited_ms": waited.round(), "late_in_a_row": e.late_streak }));
                        }
                    }
                    if unresponsive {
                        if let Some(e) = engine.take() {
                            event(root, name, "process_unresponsive", json!({ "late_in_a_row": e.late_streak, "pid": e.pid }));
                            e.detach("it answered no turn in time, thirty times in a row");
                        }
                    }
                }
            }
            if let Some(at) = hello_at {
                if !ready_sent && at.elapsed() >= opt.ready_deadline {
                    // The process has not said ready; the ladder must not wait for it.
                    ready_sent = true;
                    if let Some(r) = flow.record.as_mut() {
                                r.frame_out("gateway", r#"{"type":"ready"}"#);
                            }
                            if let Err(why) = send(&mut ws, shared, json!({ "type": "ready" }).to_string()) {
                        why_closed = why;
                        break 'conn;
                    }
                    event(root, name, "ready_sent_for_process", json!({ "waited_ms": at.elapsed().as_millis() as u64 }));
                }
            }
            if let Some(until) = resume_until {
                if Instant::now() >= until {
                    // The match did not come back: it ended while the connection was down.
                    resume_until = None;
                    event(root, name, "match_not_resumed", json!({ "match": flow.match_id, "tick": flow.tick }));
                    if let Some(r) = flow.record.take() {
                        r.finish("not resumed: the match ended while the connection was down", None);
                    }
                    flow.in_match = false;
                    flow.last_hello = None;
                    flow.last_obs = None;
                    flow.tick = None;
                    pending = None;
                    hello_at = None;
                    match_started = None;
                    shared.set_active(name, if queued { 1 } else { 0 });
                }
            }
            if let Some(at) = queue_due {
                if queued || (flow.in_match && resume_until.is_none()) {
                    queue_due = None;
                } else if Instant::now() >= at && (engine.is_some() || resume_until.is_some()) && after_end.is_none() {
                    // The match limit is ours to respect: the flagship always asks, the others
                    // only while there is room.
                    let room = if role == "flagship" {
                        // The scouts first: while one of them is held back by the ladder's
                        // match limit, the flagship does not queue for the slot that frees
                        // next, for up to `flagship_yield`.
                        resume_until.is_some() || shared.others_held(name) == 0 || Instant::now() >= at + opt.flagship_yield
                    } else {
                        resume_until.is_some() || shared.others_active(name) < opt.max_concurrent
                    };
                    // The label: the store's for a bot the fleet deploys, else the one its
                    // process attached with (the flagship's thread chooses its own).
                    if label.is_none() {
                        label = engine.as_ref().map(|e| e.version.clone()).filter(|v| v != "?");
                    }
                    if let (true, Some(l)) = (room, label.clone()) {
                        queue_due = None;
                        if !shared.match_budget.wait_for_room(stop) || !shared.queue_budget.take(stop) {
                            let _ = ws.close(None);
                            break 'outer;
                        }
                        Shared::pace(&shared.next_queue, opt.queue_gap);
                        if let Err(why) = send(&mut ws, shared, queue_frame(&l)) {
                            why_closed = why;
                            break 'conn;
                        }
                        queued = true;
                        queued_since = Some(Instant::now());
                        shared.set_active(name, if flow.in_match { 2 } else { 1 });
                    }
                }
            }
            // How long there has been no bot process. A swap takes a second; the bot stays
            // queued through it (leaving and queueing again is traffic the ladder has to
            // account for, and a match that starts meanwhile is answered idle until the new
            // process is in).
            if engine.is_some() {
                no_process_since = None;
            } else if no_process_since.is_none() {
                no_process_since = Some(Instant::now());
            }
            if queued && !flow.in_match && resume_until.is_none() && no_process_since.map(|t| t.elapsed() >= Duration::from_secs(10)).unwrap_or(false) {
                // Nobody to play a match for ten seconds: step out of the queue until a process attaches.
                let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                queued = false;
                queued_since = None;
                shared.set_active(name, 0);
                queue_due = Some(Instant::now());
                if stall_said {
                    stall_said = false;
                }
            }
            if last_heard.elapsed() > Duration::from_secs(45) {
                why_closed = "no traffic and no pong for 45 s".into();
                break 'conn;
            }
            // The keepalive ping doubles as the socket test between matches: every 20 s, and
            // again after 3 s when the last one was slow, to be sure before acting on it.
            if last_ping.elapsed() > Duration::from_secs(if slow_pings > 0 && !flow.in_match { 3 } else { 20 }) && ping_sent.is_none() {
                last_ping = Instant::now();
                ping_sent = Some(Instant::now());
                if ws.send(Message::Ping(vec![b'g', b'w'])).is_err() {
                    why_closed = "ping failed".into();
                    break 'conn;
                }
            }
            // While waiting for a match on a slow socket, the bot rotates when its turn comes.
            if !flow.in_match && after_end.is_none() && resume_until.is_none() && last_slow_check.elapsed() > Duration::from_secs(2) {
                last_slow_check = Instant::now();
                if let Some(reason) = shared.may_rotate(name) {
                    event(root, name, "rotated", json!({ "socket": this, "remote": remote.to_string(), "ping_ms": ping_ms.map(|x| x.round()), "wire_ms": wire_ms.map(|x| x.round()), "when": "waiting for a match" }));
                    if queued {
                        // Out of the queue first, so no match starts on the socket we close.
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        shared.set_active(name, 0);
                    }
                    let _ = ws.close(None);
                    voluntary = true;
                    why_closed = format!("rotated while waiting for a match: {reason}");
                    break 'conn;
                }
            }

            // -- once a second: the bot process's side, health, stalls, changes of plan ------------
            let mut process_bad: Option<String> = None;
            if let Some(e) = engine.as_mut() {
                if !e.flush() {
                    process_bad = Some("its socket closed".into());
                }
                let (n, age) = e.backlog();
                if n > 256 || age > Duration::from_secs(3) {
                    process_bad = Some(format!("it is not reading: {n} frames waiting, the oldest for {} ms", age.as_millis()));
                }
            }
            if last_tick_1s.elapsed() >= Duration::from_millis(1000) {
                last_tick_1s = Instant::now();
                let _ = ws.flush();
                // Stalls: a queue may wait as long as the ladder likes, on a live socket. Say
                // so, with the reason the ladder gave, once.
                if queued && !flow.in_match {
                    let waited = queued_since.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
                    stall_cause = match &refusal {
                        Some((code, at, d)) => format!(
                            "the ladder holds it back ({code}{}; told {} s ago). It stays queued and plays when the quota allows",
                            match (d.get("quota").and_then(Value::as_str), d.get("used"), d.get("limit")) {
                                (Some(q), Some(u), Some(l)) if !u.is_null() => format!(": {q}, {u} of {l}"),
                                (Some(q), _, _) => format!(": {q}"),
                                _ => String::new(),
                            },
                            at.elapsed().as_secs()
                        ),
                        None => format!("queued on a live socket (last frame {} s ago, ping {}); the ladder has offered no match", last_heard.elapsed().as_secs(), ping_ms.map(|x| format!("{x:.0} ms")).unwrap_or_else(|| "not measured".into())),
                    };
                    if waited >= opt.stall_after && !stall_said {
                        stall_said = true;
                        log_line(root, "STALL", &format!("{name} has been queued {} s without a match: {stall_cause}", waited.as_secs()));
                        event(root, name, "stall", json!({ "queued_s": waited.as_secs(), "cause": stall_cause }));
                    }
                }
                let label_now = label.clone();
                let mut resync = false;
                if let Some(e) = engine.as_mut() {
                    if e.proto >= 2 {
                        // The truth, for the bot process: on change and once a second.
                        let status = json!({
                            "type": "status", "connection": "up", "remote": remote.to_string(), "match": if flow.in_match { flow.match_id.clone() } else { None },
                            "tick": flow.tick, "queued": queued, "version": label_now, "wire_ms": wire_ms.map(|x| x.round()),
                        })
                        .to_string();
                        if status != e.last_status_text || e.last_status.elapsed() >= Duration::from_secs(1) {
                            e.last_status_text = status.clone();
                            e.last_status = Instant::now();
                            e.push(&status, false);
                        }
                        if e.last_ping.elapsed() >= Duration::from_secs(2) {
                            e.last_ping = Instant::now();
                            let id = e.next_ping_id;
                            e.next_ping_id += 1;
                            e.push(&json!({ "type": "ping", "id": id }).to_string(), false);
                        }
                        if e.last_pong.elapsed() > Duration::from_secs(10) && pending.is_none() {
                            process_bad = Some("it has not answered a heartbeat for 10 s".into());
                        }
                        if e.disagreements >= 2 {
                            e.disagreements = 0;
                            resync = true;
                        }
                    }
                }
                if resync {
                    if let Some(e) = engine.as_mut() {
                        shared.resyncs.fetch_add(1, Ordering::Relaxed);
                        e.push(&json!({ "type": "sync", "why": "the process was out of step", "match": if flow.in_match { flow.match_id.clone() } else { None }, "tick": flow.tick }).to_string(), false);
                        if flow.in_match {
                            if let Some(h) = &flow.last_hello {
                                e.push(h, false);
                            }
                            if let Some(o) = &flow.last_obs {
                                e.push(o, true);
                            }
                        }
                    }
                    event(root, name, "resync", json!({ "match": flow.match_id, "tick": flow.tick, "in_match": flow.in_match }));
                }
                // Is this socket slow? Two slow pings in a row (the test at connect counts as
                // two), or a wire time over 200 ms through a match. Then the bot waits in the
                // rotation queue; when the socket recovers it leaves it.
                let reject = shared.ping_reject_ms.load(Ordering::Relaxed) as f64;
                let slow_ping = reject > 0.0 && slow_pings >= 2;
                let slow_wire = reject > 0.0 && wire_ms.map(|w| w > 200.0).unwrap_or(false);
                if slow_ping || slow_wire {
                    let why = if slow_ping { format!("slow ping, {} ms", ping_ms.map(|x| x.round()).unwrap_or(0.0)) } else { format!("slow wire, {} ms a turn", wire_ms.map(|x| x.round()).unwrap_or(0.0)) };
                    shared.want_rotation(name, role == "flagship", why);
                } else {
                    shared.cancel_rotation(name);
                }
                let conn = json!({
                    "connected": true, "socket_id": this, "remote": remote.to_string(), "up_s": connected.elapsed().as_secs(), "ping_ms": ping_ms.map(|x| x.round()), "wire_ms": wire_ms.map(|x| x.round()),
                    "slow": slow_ping || slow_wire, "last_frame_s": last_heard.elapsed().as_secs(), "queued": queued, "queued_s": queued_since.map(|t| t.elapsed().as_secs_f64().round()),
                    "match_s": match_started.map(|t| t.elapsed().as_secs_f64().round()), "stall_cause": if queued { Some(stall_cause.clone()) } else { None },
                    "refused": refusal.as_ref().map(|(code, at, d)| json!({ "code": code, "ago_s": at.elapsed().as_secs(), "detail": d })),
                });
                let mut h = health(&flow, &engine, Some(conn), &label, &role, &down_why, backoff);
                h["rotation"] = shared.rotation_status(name);
                shared.publish(name, h);

                if !flow.in_match {
                    if stop() {
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        let _ = ws.close(None);
                        break 'outer;
                    }
                    // A halt, or a new release or label, while waiting.
                    match plan(root, name) {
                        Ok(np) => {
                            let stale = np.bot.managed && engine.as_ref().map(|e| Some(&e.release) != np.release.as_ref() || Some(&e.version) != np.version.as_ref()).unwrap_or(false);
                            if stale && after_end.is_none() {
                                if let Some(e) = engine.take() {
                                    event(root, name, "process_swap", json!({ "from": e.release, "to": np.release, "version": np.version }));
                                    e.detach("a new release is deployed");
                                }
                            }
                            if np.version.is_some() && np.version != label {
                                // A new label: leave the queue and come back under it.
                                label = np.version.clone();
                                if queued {
                                    let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                                    queued = false;
                                    queued_since = None;
                                    shared.set_active(name, 0);
                                    queue_due = Some(Instant::now() + Duration::from_millis(500));
                                }
                            }
                        }
                        Err(why) => {
                            note(format!("not running: {why}"));
                            let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                            let _ = ws.close(None);
                            down_why = why;
                            continue 'outer;
                        }
                    }
                }
            }
            if let Some(why) = process_bad {
                if let Some(e) = engine.take() {
                    event(root, name, "process_detached", json!({ "why": why, "pid": e.pid, "in_match": flow.in_match }));
                    e.detach(&why);
                }
            }

            // Sleep until the ladder or the bot process has something, or the next clock check.
            let mut fds = Vec::new();
            fds.extend(socket_fd(&ws));
            fds.extend(engine.as_ref().map(|e| e.stream.as_raw_fd()));
            let nap = match pending {
                Some((_, since)) => opt.answer_deadline.saturating_sub(since.elapsed()).min(Duration::from_millis(100)),
                None => Duration::from_millis(100),
            };
            wait_readable(&fds, nap.max(Duration::from_millis(1)));
        }

        // The connection is gone. A match in progress goes on at the ladder (ten seconds of
        // grace): reconnect at once if this one had been working, and the ladder sends the
        // match start and the current observation again. Turns held for the old socket are
        // never replayed.
        shared.drops.fetch_add(1, Ordering::Relaxed);
        if !flow.in_match {
            shared.set_active(name, 0);
        }
        if flow.in_match {
            flow.dropped_at_tick = flow.tick.or(flow.dropped_at_tick);
            if let Some(r) = flow.record.as_mut() {
                r.note("disconnected", json!({ "socket": this, "why": why_closed, "tick": flow.tick }));
            }
        }
        if !voluntary {
            shared.note_reset();
        }
        // The socket's record: closed, and why. A new socket is judged afresh.
        if let Some(s) = sid.take() {
            tel().socket_closed(s, &if voluntary { why_closed.clone() } else { format!("dropped: {why_closed}") });
            if voluntary {
                rotated_from = Some(s);
                after_replacement = true;
                shared.replaced.fetch_add(1, Ordering::Relaxed);
            }
        }
        shared.cancel_rotation(name);
        backoff = if voluntary || flow.in_match {
            // A rotation's next socket is spaced by the queue; a match waits to be resumed.
            Duration::ZERO
        } else if connected.elapsed() > Duration::from_secs(30) {
            Duration::ZERO
        } else {
            (backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30))
        };
        if stall_said {
            log_line(root, "STALL", &format!("{name}: the connection it was queued on closed ({why_closed}); reconnecting"));
        }
        down_why = why_closed.clone();
        shared.set_held(name, false);
        // The bot's own log keeps the record of drops, in the form its client wrote them
        // when it held the socket, so everything that counts resets still sees them.
        let version = label.clone().unwrap_or_default();
        let line = json!({ "t": "disconnected", "detail": { "why": why_closed, "in_match": flow.in_match, "retry_in_s": backoff.as_secs(), "by": "gateway", "replaced": voluntary }, "ts": now_ms(), "bot": name, "version": version });
        let day = crate::online::utc_stamp()[..10].to_string();
        let _ = std::fs::create_dir_all(root.join("logs").join(name));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("logs").join(name).join(format!("{name}-{day}.jsonl"))) {
            // One write call: the bot process appends to the same file, and a line written
            // in pieces could end up inside one of its lines.
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
        event(root, name, "disconnected", json!({ "why": why_closed, "in_match": flow.in_match, "retry_in_s": backoff.as_secs(), "replaced": voluntary }));
        if let Some(e) = engine.as_mut() {
            if e.proto >= 2 {
                e.push(&json!({ "type": "status", "connection": "down", "why": why_closed, "match": if flow.in_match { flow.match_id.clone() } else { None }, "tick": flow.tick, "queued": false, "version": label }).to_string(), false);
            }
        }
    }

    if let Some(e) = engine.take() {
        e.detach("the gateway is stopping");
    }
    if let Some(s) = sid.take() {
        tel().socket_closed(s, "the gateway stopped");
    }
    if let Some(r) = flow.record.take() {
        r.finish("the gateway stopped during the match", None);
    }
    shared.cancel_rotation(name);
    shared.set_active(name, 0);
    shared.publish(name, json!({ "bot": name, "verdict": "the gateway has stopped running this bot", "level": "bad", "ts_ms": now_ms() }));
    eprintln!("[{name}] gateway: stopped at {}", now_ms());
}

fn nap(d: Duration, stop: &dyn Fn() -> bool) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if stop() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !stop()
}

/// A bot process has connected to the local socket: read who it is and hand it to its
/// bot's thread.
fn greet(root: &Path, stream: UnixStream, shared: &Shared) {
    // On a Mac an accepted socket inherits the listener's non-blocking mode.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    // One byte at a time: nothing after the first line may be swallowed here.
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match (&stream).read(&mut b) {
            Ok(1) if b[0] == b'\n' => break,
            Ok(1) => {
                line.push(b[0]);
                if line.len() > 4096 {
                    return;
                }
            }
            _ => return,
        }
    }
    let v: Value = serde_json::from_slice(&line).unwrap_or(Value::Null);
    let bot = v.get("bot").and_then(Value::as_str).unwrap_or("").to_string();
    let known = Config::load(root).ok().and_then(|c| c.bot(&bot).map(|b| b.gateway)).unwrap_or(false);
    if v.get("type").and_then(Value::as_str) != Some("attach") || !known {
        eprintln!("gateway: refused a local connection that did not attach as a gateway bot: {}", String::from_utf8_lossy(&line));
        let _ = (&stream).write_all(format!("{}\n", json!({ "type": "error", "code": "not_a_gateway_bot", "bot": bot })).as_bytes());
        return;
    }
    let _ = stream.set_read_timeout(None);
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let e = Engine::new(stream, s("release"), s("version"), v.get("proto").and_then(Value::as_i64).unwrap_or(1), v.get("pid").and_then(Value::as_i64).unwrap_or(0));
    eprintln!("gateway: a bot process for {bot} attached ({} as {}, protocol {})", e.release, e.version, e.proto);
    if let Some(old) = shared.attach.lock().unwrap_or_else(|p| p.into_inner()).insert(bot, e) {
        old.detach("another bot process attached before this one was taken up");
    }
}

/// Listens on the local socket for bot processes.
fn listen(root: PathBuf, shared: Arc<Shared>, stop: Arc<dyn Fn() -> bool + Send + Sync>) {
    let path = sock_path(&root);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("gateway: cannot listen on {}: {e}", path.display());
            log_line(&root, "ALERT", &format!("the gateway cannot listen on {}: {e}. No bot process can attach.", path.display()));
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = listener.set_nonblocking(true);
    eprintln!("gateway: bot processes attach at {}", path.display());
    while !stop() {
        match listener.accept() {
            Ok((stream, _)) => {
                let (root, shared) = (root.clone(), shared.clone());
                let _ = std::thread::Builder::new().name("gw-greet".into()).spawn(move || greet(&root, stream, &shared));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// The one verdict: every bot's health and what is wrong, if anything.
pub fn verdict(health: &HashMap<String, Value>, expected: &[String]) -> (String, Vec<String>) {
    let mut problems = Vec::new();
    for name in expected {
        match health.get(name) {
            None => problems.push(format!("{name}: the gateway is not running it")),
            Some(h) => {
                if h.get("level").and_then(Value::as_str) != Some("ok") {
                    problems.push(format!("{name}: {}", h.get("verdict").and_then(Value::as_str).unwrap_or("unknown")));
                }
            }
        }
    }
    (if problems.is_empty() { "ok".to_string() } else { "degraded".to_string() }, problems)
}

/// Runs every bot that is set to go through the gateway, each on its own thread, and
/// starts a thread for any bot that is switched over while it runs.
pub fn run(root: &Path, stop: Arc<dyn Fn() -> bool + Send + Sync>) {
    let cfg = Config::load(root).unwrap_or_default();
    let shared = Arc::new(Shared::new(Options::load(root), &cfg));
    let _ = TEL.set(Telemetry::start(root));
    // Sockets a previous gateway left open in the record are closed now.
    if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(5)) {
        let _ = store.conn.execute("UPDATE sockets SET closed_ms = ?1, close_reason = 'the gateway restarted' WHERE closed_ms IS NULL", [now_ms()]);
    }
    let mut last_state = Instant::now() - Duration::from_secs(60);
    let mut running: Vec<(String, std::thread::JoinHandle<()>)> = Vec::new();
    let mut last_report = Instant::now();
    let mut last_verdict = String::new();
    let mut last_options = Instant::now();
    eprintln!("gateway started");
    {
        let (root, shared, stop) = (root.to_path_buf(), shared.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("gw-listen".into()).spawn(move || listen(root, shared, stop));
    }
    while !stop() {
        let mut expected: Vec<String> = Vec::new();
        if let Ok(cfg) = Config::load(root) {
            for b in cfg.bots.iter().filter(|b| b.gateway) {
                expected.push(b.name.clone());
                if running.iter().any(|(n, h)| *n == b.name && !h.is_finished()) {
                    continue;
                }
                running.retain(|(n, _)| *n != b.name);
                let (root, name, shared, stop) = (root.to_path_buf(), b.name.clone(), shared.clone(), stop.clone());
                let handle = std::thread::Builder::new().name(format!("gw-{}", b.name)).spawn(move || run_bot(&root, &name, &shared, &*stop));
                if let Ok(h) = handle {
                    running.push((b.name.clone(), h));
                }
            }
        }
        if last_options.elapsed() >= Duration::from_secs(10) {
            // The socket-test limits can be changed while the gateway runs.
            last_options = Instant::now();
            let o = Options::load(root);
            shared.ping_reject_ms.store(o.ping_reject_ms.max(0.0) as u64, Ordering::Relaxed);
            shared.slow_wire_ms.store(o.slow_wire_ms.max(0.0) as u64, Ordering::Relaxed);
        }
        if last_state.elapsed() >= Duration::from_secs(2) {
            // The gateway's state and the health verdict, for `fleet status`, `fleet health`
            // and the status page.
            last_state = Instant::now();
            let b = |x: &Budget| json!({ "used": x.used(), "limit": x.limit, "window_s": x.window.as_secs(), "times_waited": x.waits.load(Ordering::Relaxed) });
            let health = shared.health.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let (word, problems) = verdict(&health, &expected);
            let line = format!("{word}: {}", problems.join("; "));
            if line != last_verdict {
                if word == "ok" {
                    if !last_verdict.is_empty() {
                        log_line(root, "HEALTH", "ok: every bot is playing or waiting for a match");
                    }
                } else {
                    log_line(root, "HEALTH", &format!("degraded: {}", problems.join("; ")));
                }
                last_verdict = line;
            }
            let state = json!({
                "ts_ms": now_ms(),
                "verdict": word, "problems": problems,
                "bots": running.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                "rotation_queue": shared.rotation_queue(),
                "connections": b(&shared.connect_budget), "queue_requests": b(&shared.queue_budget), "matches": b(&shared.match_budget),
                "socket_replacements": b(&shared.replace_budget),
                "since_start": {
                    "connects": shared.connects.load(Ordering::Relaxed), "drops": shared.drops.load(Ordering::Relaxed), "slow_sockets_replaced": shared.replaced.load(Ordering::Relaxed),
                    "frames_out": shared.frames_out.load(Ordering::Relaxed), "frames_in": shared.frames_in.load(Ordering::Relaxed),
                    "idle_turns": shared.idle_turns.load(Ordering::Relaxed), "refused_queues": shared.refusals.load(Ordering::Relaxed), "resyncs": shared.resyncs.load(Ordering::Relaxed),
                },
                "slow_addresses": shared.slow.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            });
            // The gateway's own row in the store's `gateway_state` (each bot has its own).
            tel().state("*", if word == "ok" { "ok" } else { "bad" }, &if problems.is_empty() { "every bot is playing or waiting for a match".to_string() } else { problems.join("; ") }, None, &state);
        }
        if last_report.elapsed() >= Duration::from_secs(300) {
            last_report = Instant::now();
            eprintln!(
                "gateway: {} bots; since start {} connects, {} drops, {} slow sockets replaced, {} frames out, {} in, {} turns answered for a late or absent process, {} refused queues, {} resyncs",
                running.len(),
                shared.connects.load(Ordering::Relaxed),
                shared.drops.load(Ordering::Relaxed),
                shared.replaced.load(Ordering::Relaxed),
                shared.frames_out.load(Ordering::Relaxed),
                shared.frames_in.load(Ordering::Relaxed),
                shared.idle_turns.load(Ordering::Relaxed),
                shared.refusals.load(Ordering::Relaxed),
                shared.resyncs.load(Ordering::Relaxed)
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    for (_, h) in running {
        let _ = h.join();
    }
    tel().state("*", "bad", "the gateway has stopped", None, &json!({ "ts_ms": now_ms() }));
    tel().flush();
    eprintln!("gateway stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_holds_the_line() {
        let b = Budget::new("test", 3, Duration::from_millis(300));
        let never = || false;
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.take(&never));
        }
        assert!(t0.elapsed() < Duration::from_millis(100), "the first three are free");
        assert_eq!(b.used(), 3);
        // The fourth waits until the first has aged out of the window.
        assert!(b.take(&never));
        assert!(t0.elapsed() >= Duration::from_millis(290), "{:?}", t0.elapsed());
        assert_eq!(b.waits.load(Ordering::Relaxed), 1);
        // Asked to stop while waiting: gives up and books nothing.
        let stop = || true;
        assert!(b.take(&never));
        assert!(b.take(&never));
        assert!(!b.take(&stop));
    }

    #[test]
    fn a_newer_observation_replaces_the_ones_still_waiting() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut e = Engine::new(a, "r1".into(), "v1".into(), 1, 0);
        // Nothing is written yet in this test's terms: fill the queue by hand.
        e.out.push_back(Queued { bytes: b"{\"type\":\"hello\"}\n".to_vec(), observation: false, at: Instant::now() });
        e.out.push_back(Queued { bytes: b"{\"type\":\"observation\",\"tick\":1}\n".to_vec(), observation: true, at: Instant::now() });
        e.out.push_back(Queued { bytes: b"{\"type\":\"observation\",\"tick\":2}\n".to_vec(), observation: true, at: Instant::now() });
        assert!(e.push("{\"type\":\"observation\",\"tick\":3}", true));
        assert_eq!(e.dropped_observations, 2);
        // The other end reads the hello and only the newest observation.
        let mut got = String::new();
        b.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut buf = [0u8; 512];
        while let Ok(n) = (&b).read(&mut buf) {
            if n == 0 {
                break;
            }
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
            if got.matches('\n').count() >= 2 {
                break;
            }
        }
        assert_eq!(got, "{\"type\":\"hello\"}\n{\"type\":\"observation\",\"tick\":3}\n");
    }

    #[test]
    fn the_verdict_names_what_is_wrong() {
        let mut h = HashMap::new();
        h.insert("a".to_string(), json!({ "level": "ok", "verdict": "playing" }));
        h.insert("b".to_string(), json!({ "level": "bad", "verdict": "no bot process attached" }));
        let (word, problems) = verdict(&h, &["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(word, "degraded");
        assert_eq!(problems, vec!["b: no bot process attached".to_string(), "c: the gateway is not running it".to_string()]);
        h.insert("b".to_string(), json!({ "level": "ok", "verdict": "waiting for a match, 3 s" }));
        assert_eq!(verdict(&h, &["a".to_string(), "b".to_string()]).0, "ok");
    }

    #[test]
    fn the_rotation_queue_puts_the_flagship_first_and_rotates_one_at_a_time() {
        let mut opt = Options::default();
        opt.replace_gap = Duration::from_millis(200);
        opt.replacements_per_10min = 2;
        let shared = Shared::new(opt, &Config::default());
        shared.want_rotation("scout-a", false, "ping 190 ms".into());
        shared.want_rotation("scout-b", false, "ping 180 ms".into());
        shared.want_rotation("flag", true, "ping 200 ms".into());
        // The flagship goes first, the scouts keep their order.
        assert_eq!(shared.rotation_status("flag")["position"], 1);
        assert_eq!(shared.rotation_status("scout-a")["position"], 2);
        assert_eq!(shared.rotation_status("scout-b")["position"], 3);
        assert_eq!(shared.rotation_status("nobody")["scheduled"], false);
        // Only the head may rotate.
        assert!(shared.may_rotate("scout-a").is_none());
        assert_eq!(shared.may_rotate("flag").as_deref(), Some("ping 200 ms"));
        assert_eq!(shared.rotation_status("flag")["scheduled"], false);
        // The next one waits for the gap.
        assert!(shared.may_rotate("scout-a").is_none());
        let eta = shared.rotation_status("scout-a")["eta_ms"].as_i64().unwrap();
        assert!(eta > now_ms() && eta <= now_ms() + 250);
        std::thread::sleep(Duration::from_millis(220));
        assert!(shared.may_rotate("scout-a").is_some());
        // The cap (two in ten minutes) is spent: the third waits, and says why.
        std::thread::sleep(Duration::from_millis(220));
        assert!(shared.may_rotate("scout-b").is_none());
        assert!(shared.rotation_status("scout-b")["waiting_for"].as_str().unwrap().contains("cap"));
        // A bot whose socket recovers leaves the queue.
        shared.cancel_rotation("scout-b");
        assert_eq!(shared.rotation_status("scout-b")["scheduled"], false);
        // The cap on replacements never waits and never goes over.
        let b = Budget::new("socket replacement", 2, Duration::from_secs(600));
        assert!(b.try_take() && b.try_take());
        assert!(!b.try_take());
        assert_eq!(median(&[5.0, 1.0, 9.0]), 5.0);
    }
}
