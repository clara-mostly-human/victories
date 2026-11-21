//! The policy: a one-tick matrix search over a safe heuristic.
//!
//! Every turn we could take is given its own merits by the heuristic: staying inside the
//! zone, getting out of what is already in the air (planned over a frostburst's whole
//! fuse), keeping off walls a repulsor could slam us into, taking pickups when they are
//! cheap. With the opponent in view the exchange itself is then solved as a matrix game:
//! each of our turns against each movement the opponent could make, played on the exact
//! engine and a few ticks on, solved by regret matching. Out of
//! view, or with `search` off, the turn with the best worst case is taken ("safe-2").
//!
//! matrix-2 adds what the first ladder games taught: most wall slams there are a dash
//! to the opponent's side with a repulsor in the same tick. We now consider that turn
//! ourselves (`dash_slam`) and avoid tiles where it would work against us (`slam_risk`).
//!
//! matrix-3 looks a second tick ahead: the sixteen turns the first solve likes best are
//! played out with the next tick itself solved as a game (`deepen`). Deepening more
//! turns, or adding dodge-only ticks after the second, did not help.
//!
//! matrix-4 gives the opponent its close shot and its repulsor on both ticks. Replays of
//! the ladder games against bots rated 2100 and up showed why: most of their hits and
//! wall slams land the tick after we step, when we cannot move. A search that plays the
//! opponent's punishment on the second tick stops stepping into it.
//!
//! matrix-5 values the leaves of the search with a tempo evaluation fitted from 1,287
//! ladder replays (`victris_engine::tempo`): what it is worth, in hp over the next
//! twelve ticks of real games, to hold the step and the shot while the opponent does not.
//!
//! All numbers live in [`Playbook`], so a playbook for a named opponent can change them.
//! They were tuned in the arena and then confirmed against other bots. Two findings are
//! built into the defaults and deserve suspicion later, against stronger opponents: the
//! search does not model the opponent's attacks (doing so made it far too timid against
//! everything tried so far), and it plays its most likely turn instead of sampling.

use crate::tracker::Tracker;
use serde_json::Value;
use std::collections::VecDeque;
use std::time::Instant;
use victris_engine::geom::{add, chebyshev, octant, DIR_VEC, P};
use victris_engine::map::PadKind;
use victris_engine::observe::{can_see, View};
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State, NO_BUFF};
use victris_engine::turn::RawTurn;
use victris_engine::Engine;

/// A named set of policy settings. One is chosen per match, by opponent.
#[derive(Clone, Debug)]
pub struct Playbook {
    pub name: String,
    /// what a ranged shot must be worth before it is fired (it costs the cooldown)
    pub fire_cost: f64,
    pub repulsor_cost: f64,
    pub frost_cost: f64,
    /// value of casting a frostburst on a visible opponent (it slows and forces a move)
    pub frost_value: f64,
    /// extra value of stunning, on top of the damage (a stun sets up a sure shot)
    pub stun_bonus: f64,
    /// weight of the average outcome next to the worst case
    pub average_weight: f64,
    /// weight of what the opponent could do to us if it guessed our tile
    pub threat_weight: f64,
    /// cost of stepping while the opponent is close (the step is our dodge)
    pub step_cost_near: f64,
    pub dash_cost: f64,
    /// distance we like to keep from a visible opponent
    pub preferred_distance: f64,
    pub distance_weight: f64,
    /// pull toward the current goal tile, per tile of walking distance
    pub goal_weight: f64,
    /// go for a health pad when this much hp is missing
    pub health_pad_missing_hp: i32,
    /// weight of damage already in the air that the turn leaves us unable to dodge
    pub incoming_weight: f64,
    /// cost of stepping within four tiles of an opponent whose shot is ready
    pub step_cost_under_fire: f64,
    /// choose turns by solving the tick as a matrix game (our turns against theirs)
    /// instead of taking the best worst case; only used while the opponent is in view
    pub search: bool,
    /// ticks each cell of the matrix is played forward, both sides only dodging
    pub rollout_ticks: i32,
    /// search: include the opponent's attacks among its options
    pub model_attacks: bool,
    /// search: play from the mixed strategy (true) or always its most likely turn (false)
    pub sample: bool,
    /// search: consider shots aimed where the opponent could step
    pub lead_shots: bool,
    /// search: give the opponent a shot at the tile we stand on among its options, on both
    /// ticks. Within two tiles such a shot lands the tick it is fired, so this is what
    /// makes the search respect a ready shooter at close range.
    pub model_shots: bool,
    /// search: ticks looked ahead for the turns in contention (2, or 3 with time to spare)
    pub depth: u32,
    /// search: how many of our turns may get the third tick
    pub deep3_rows: usize,
    /// search: at inner nodes, how many of each side's likeliest turns go a tick further
    pub inner_rows: usize,
    pub inner_cols: usize,
    /// search: think-time cap per decision in ms for the third tick (0 = no clock: always
    /// finish, which keeps arena runs repeatable)
    pub think_ms: u64,
    /// search threads
    pub threads: usize,
    /// search: hard limit in ms on the second tick's deepening; turns not reached by then
    /// are left out of the choice
    pub hard_ms: u64,
    /// search: in rollouts, each side takes the hit the other cannot avoid (a close shot
    /// or a slam on a fighter that cannot move), instead of only dodging
    pub rollout_punish: bool,
    /// value of closing in while the opponent's shot is on cooldown and ours is ready,
    /// per tile gained: the proactive half of the tempo game
    pub press_bonus: f64,
    /// search: how much of each turn's value comes from a plain opponent (stands still
    /// `prior_hold` of the time, otherwise steps at random, never attacks) instead of the
    /// solved one. 0 = pure game solution.
    pub prior_weight: f64,
    pub prior_hold: f64,
    /// the plain opponent of `prior_weight` is replaced by the measured one: how the strong
    /// bots on the ladder really move and attack in the opponent's situation (`oppmodel`)
    pub prior_model: bool,
    /// walk to a power pad early enough to be standing there when it appears, instead of
    /// setting out three ticks before. Replays: the strong bots are on the first power pad
    /// at tick 23 (it appears at tick 20) and took it in 39 of our 50 games against them;
    /// they average 1.64 power pads a game against us to our 0.54.
    pub pad_race: bool,
    /// what ending a turn on a power pad that is there (or appears next tick) is worth, in
    /// hp, while we have no buff; above 0 we also stop yielding a pad just because the
    /// opponent is nearer, and walk to a pad goal with more weight. Today's replays: in
    /// our games against the top group they take 1.75 power pads a game and we 0.40, and
    /// a buffed Shelbot3 took us from 118 hp to 21 in six ticks while we stood one tile
    /// from the second pad. The buff is damage x3/2 for 50 ticks.
    pub pad_bonus: f64,
    /// above 0, the turn comes from the tree search for both sides (`victris_engine::mcts`)
    /// run for this many milliseconds, with none of the hand-set terms of this file. Used
    /// while the opponent is in view; out of view the old policy looks for it.
    pub tree_ms: u64,
    /// ticks the tree search looks ahead
    pub tree_depth: u32,
    /// weight of the fitted judgment of positions at the tree's leaves (0 = off; with it
    /// on, the fixed value per buff tick is dropped, since the judgment covers it)
    pub tree_value: f64,
    /// the tree search also runs with the opponent out of sight, on the tracker's best guess
    /// of where it is, for this many ticks after it was last seen (0 = only in view)
    pub tree_fog: i32,
    /// which generation of the judgment the tree search uses (0 = the latest)
    pub tree_gen: u32,
    /// the tree search may lead its shots (aim where the opponent could step) when standing still
    pub tree_lead: bool,
    /// the tree search's play-out at a leaf also plays a wall slam that is there for the taking
    pub tree_slam: bool,
    /// the tree search's priors come from its own choices instead of the ladder bots'
    pub tree_own_priors: bool,
    /// the tree search considers dash + repulsor from beyond the repulsor's reach (both sides)
    pub tree_dash_burst: bool,
    /// the tree search expects the opponent's standing shots to lead (aim where we could
    /// step), without leading ours
    pub tree_lead_them: bool,
    /// visits a new position in the tree gets before its move lists are made (0 = at once)
    pub tree_expand_at: u32,
    /// the most positions one search's tree may hold (0 = no limit)
    pub tree_max_nodes: u32,
    /// not 0: our turn comes from solving the root as a matrix game over each side's this
    /// many most visited turns (one search thread only), instead of the most visited turn
    pub tree_solve: u32,
    /// weight of the win-probability judgment at the tree's leaves (hp per unit of 2p - 1)
    pub tree_net: f64,
    /// The win-probability network as the whole leaf, in hp (`mcts::Config::net_hp`): 1.0 replaces
    /// the hp count and the judgment at the end of the play-out with the network's verdict.
    pub tree_net_hp: f64,
    /// The hp-to-come network as the leaf's judgment (`mcts::Config::net_value`), with this weight.
    pub tree_net_value: f64,
    /// Cap in hp on that network's term (`mcts::Config::net_cap`; 0: none).
    pub tree_net_cap: f64,
    /// not 0: while either fighter holds the power buff, the tree searches this deep and plays
    /// out `tree_quiet_buff` ticks at its leaves, so it sees the buff's 50 ticks run out
    pub tree_depth_buff: u32,
    /// keep the search tree between ticks: the branch the game went into seeds the next
    /// tick's search (one thread, in view only)
    pub tree_reuse: bool,
    /// Plans as moves in the search (`mcts::Config::plans`, part 4); with `tree_reuse` the kept tree loses only positions under old plans.
    pub tree_plans: bool,
    /// Plans in the leaf's play-out instead of the tree (`mcts::Config::plan_playout`, part 4 second design).
    pub tree_plan_playout: bool,
    /// The win-chance leaf with this scale (`mcts::Config::win_shape`; 0: off).
    pub tree_win_shape: f64,
    /// hp for a slam threat held at the leaf, and for being loaded (`mcts::Config::slam_value`, `loaded_value`).
    pub tree_slam_value: f64,
    pub tree_loaded_value: f64,
    /// The heal race at the leaf (`mcts::Config::heal_value`).
    pub tree_heal_value: f64,
    /// Judgment row per phase of the game (`mcts::Config::phase_gens`), e.g. [16, 17, 18, 19, 20]; empty: off.
    pub tree_phase_gens: Vec<usize>,
    /// The strategy layer's weight (`mcts::Config::strategy`; 0: off).
    pub tree_strategy: f64,
    /// Score only our own plan at the leaves (`mcts::Config::strategy_side` = our seat), not the opponent's.
    pub tree_strategy_own: bool,
    /// Share of the move prior for turns whose repulsor slams the opponent if it holds
    /// (`mcts::Config::slam_prior`; 0: off; measured on the ladder: 0.228).
    pub tree_slam_prior: f64,
    /// Opening book (part 5): per "<map>/<seat>", the turn to play at a tick when we stand on
    /// the book's tile then (written by `openbook`); off the line, the search decides.
    pub book: std::collections::HashMap<String, Vec<(i32, P, Value)>>,
    pub tree_quiet_buff: u32,
    /// tree search settings, for tuning offline: ticks of quiet play-out at a leaf (6),
    /// the exploration constant (1.5), the share of the prior spread evenly (0.25)
    pub tree_quiet: u32,
    /// searches run side by side on this many threads, their visits added (1 = one search)
    pub tree_threads: u32,
    /// out of sight, each search thread splits its time over this many guesses of where
    /// the opponent stands (needs `tree_fog`)
    pub tree_fog_samples: u32,
    pub tree_c: f64,
    pub tree_even: f64,
    /// weight (hp per unit of log probability) of the learned movement prior: how likely
    /// a strong bot is to hold, step toward, away, sideways or dash in our situation
    /// (`movemodel`, fitted on 922 replay games; it predicts 50% of their moves in unseen
    /// games against 43% for a plain table). 0 = off.
    pub move_prior_weight: f64,
    /// weight (hp per unit of log odds) of the learned attack prior: how likely a strong
    /// bot is to fire, throw the frostburst or use the repulsor in our situation. Added
    /// to that attack's merit; negative when they would hold it. 0 = off.
    pub attack_prior_weight: f64,
    /// search: what a dash costs the opponent in our model of it. We value our own dash
    /// highly (`dash_cost`), but the ladder's best spend theirs freely to dash in and
    /// fire, so their price can be set lower.
    pub their_dash_cost: f64,
    /// how often the opponent's reactive dodge of a shot already in the air succeeds, in
    /// rollouts and plans. 1 = always (the old assumption). Replays of the top twelve put
    /// it near 0.2 for a target whose step was spent when the shot left.
    pub dodge_prob: f64,
    /// the same for a shot that needs a tick more to arrive (negative = use `dodge_prob`).
    /// Replays: a target with its step in hand is hit 43 to 52% of the time by a shot two
    /// ticks out and 18 to 38% by one three ticks out.
    pub dodge_prob_far: f64,
    /// added to a frostburst's merit when the target's dash stays on cooldown until the
    /// burst goes off. Replays: such bursts are worth 11 to 13 hp over ten ticks (the
    /// target is slowed nine times in ten and the next shot lands), all others about 0.
    pub frost_combo: f64,
    /// taken from a frostburst's merit when the target can dash before it goes off: the
    /// price of having it on cooldown when the opening comes.
    pub frost_wait_cost: f64,
    /// with a value above 0, the burst is held (`frost_wait_cost`) only against an
    /// opponent that dashes: one seen dashing in this match, or any opponent before this
    /// tick. Bots that never dash give no opening, and against them the burst is best
    /// thrown at once (scout games: holding wins 89% against 98% under a rating of 2000).
    pub frost_hold_grace: i32,
    /// the price of our dash while we stand in the opponent's frostburst (negative = the
    /// usual `dash_cost`). Replays: the top bots dash out of a burst 70% of the time when
    /// they can and come out 5 hp ahead; we dash out 26% of the time, walk out slowed and
    /// come out 2 ahead. The dash (12 ticks) is back before the next burst (14).
    pub dash_cost_frosted: f64,
    /// how often the opponent dashes out of a hit it cannot step away from, in rollouts
    /// (0 = never, the old assumption). Replays: a shot at a target whose step is spent
    /// and whose dash is ready lands about 60% of the time against 88% with the dash gone.
    pub dash_dodge: f64,
    /// the same for a slowed opponent (two or more ticks from its next step), which
    /// dashes far more often: our shots at such targets land 8 to 36% of the time.
    pub dash_dodge_slowed: f64,
    /// plan search: ticks looked ahead along our own plans against the opponent model
    /// (0 or 1 = off)
    pub plan_depth: u32,
    /// plan search: plans kept at each depth
    pub plan_width: usize,
    /// plan search: only turns within this many hp of the game solve's best are eligible
    pub plan_margin: f64,
    /// plan search: how much the game solve's value still counts when ranking eligible turns
    pub plan_solve_weight: f64,
    /// plan search: cost of each shot inside a plan
    pub plan_fire_cost: f64,
    /// search: weight of the fitted tempo evaluation at the leaves (0 = off)
    pub tempo_weight: f64,
    /// cost of ending a step (so with no step next tick) within `stuck_range` tiles of an
    /// opponent whose shot is ready
    pub stuck_penalty: f64,
    pub stuck_range: i32,
    /// search: on the second tick, give the opponent its repulsor after any movement.
    /// Strong ladder bots slam us the tick after we step, when we cannot move away.
    pub model_slams: bool,
    /// search: how many of the best turns to play out a second tick (0 = one tick only)
    pub deepen: usize,
    /// search: dodge-only ticks played after the second tick
    pub deep_rollout: i32,
    /// search: deepen only when the opponent is within this many tiles
    pub deep_range: i32,
    /// consider dashing to the opponent's side and firing the repulsor in the same tick
    pub dash_slam: bool,
    /// cost of ending on a tile where a step-in or dash-in repulsor would slam us into a wall
    pub slam_risk: f64,
    /// search: include the opponent's repulsor among its options even when its other
    /// attacks are left out (a step in and a shove into a wall is the ladder's staple)
    pub model_repulsor: bool,
}

impl Default for Playbook {
    fn default() -> Self {
        Playbook {
            name: "matrix-5".into(),
            fire_cost: 3.0,
            repulsor_cost: 1.0,
            frost_cost: 0.5,
            frost_value: 2.5,
            stun_bonus: 8.0,
            average_weight: 0.15,
            threat_weight: 0.0,
            step_cost_near: 0.3,
            dash_cost: 10.0,
            preferred_distance: 3.5,
            distance_weight: 0.4,
            goal_weight: 0.7,
            health_pad_missing_hp: 10,
            incoming_weight: 1.0,
            step_cost_under_fire: 0.3,
            search: true,
            rollout_ticks: 4,
            model_attacks: false,
            sample: false,
            lead_shots: true,
            model_repulsor: true,
            dash_slam: true,
            slam_risk: 5.0,
            depth: 2,
            deep3_rows: 4,
            inner_rows: 3,
            inner_cols: 3,
            think_ms: 0,
            threads: 1,
            hard_ms: 250,
            dodge_prob: 1.0,
            dodge_prob_far: -1.0,
            frost_combo: 0.0,
            frost_wait_cost: 0.0,
            frost_hold_grace: 0,
            dash_cost_frosted: -1.0,
            dash_dodge: 0.0,
            dash_dodge_slowed: 0.0,
            plan_depth: 0,
            plan_width: 12,
            plan_margin: 2.0,
            plan_solve_weight: 1.0,
            plan_fire_cost: 1.0,
            their_dash_cost: 10.0,
            prior_weight: 0.0,
            prior_hold: 0.5,
            prior_model: false,
            pad_race: false,
            pad_bonus: 0.0,
            tree_ms: 0,
            tree_depth: 12,
            tree_value: 0.0,
            tree_fog: 0,
            tree_gen: 0,
            tree_lead: false,
            tree_slam: false,
            tree_own_priors: false,
            tree_dash_burst: false,
            tree_lead_them: false,
            tree_expand_at: 0,
            tree_max_nodes: 0,
            tree_solve: 0,
            tree_net: 0.0,
            tree_net_hp: 0.0,
            tree_net_value: 0.0,
            tree_net_cap: 0.0,
            tree_depth_buff: 0,
            tree_reuse: false,
            tree_plans: false,
            tree_plan_playout: false,
            tree_win_shape: 0.0,
            tree_slam_value: 0.0,
            tree_loaded_value: 0.0,
            tree_heal_value: 0.0,
            tree_phase_gens: Vec::new(),
            tree_strategy: 0.0,
            tree_strategy_own: false,
            tree_slam_prior: 0.0,
            book: std::collections::HashMap::new(),
            tree_quiet_buff: 0,
            tree_quiet: 6,
            tree_threads: 1,
            tree_fog_samples: 1,
            tree_c: 1.5,
            tree_even: 0.25,
            move_prior_weight: 0.0,
            attack_prior_weight: 0.0,
            rollout_punish: false,
            press_bonus: 0.0,
            tempo_weight: 1.0,
            stuck_penalty: 0.0,
            stuck_range: 3,
            model_shots: true,
            model_slams: true,
            deepen: 16,
            deep_rollout: 1,
            deep_range: 6,
        }
    }
}

impl Playbook {
    /// The default playbook with any fields in `v` replaced.
    pub fn from_json(name: &str, v: &Value) -> Playbook {
        let mut p = Playbook { name: name.to_string(), ..Playbook::default() };
        // "book": {"<map>/<seat>": {"book": [{"tick", "pos", "turn"}, ...]} or [...] }
        if let Some(books) = v.get("book").and_then(Value::as_object) {
            for (key, b) in books {
                let list = b.get("book").unwrap_or(b);
                let line: Vec<(i32, P, Value)> = list
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| {
                                let tick = x.get("tick")?.as_i64()? as i32;
                                let pos = x.get("pos")?.as_array()?;
                                Some((tick, [pos.first()?.as_i64()? as i32, pos.get(1)?.as_i64()? as i32], x.get("turn")?.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                p.book.insert(key.clone(), line);
            }
        }
        let f = |k: &str, slot: &mut f64| {
            if let Some(x) = v.get(k).and_then(Value::as_f64) {
                *slot = x;
            }
        };
        f("fire_cost", &mut p.fire_cost);
        f("repulsor_cost", &mut p.repulsor_cost);
        f("frost_cost", &mut p.frost_cost);
        f("frost_value", &mut p.frost_value);
        f("stun_bonus", &mut p.stun_bonus);
        f("average_weight", &mut p.average_weight);
        f("threat_weight", &mut p.threat_weight);
        f("step_cost_near", &mut p.step_cost_near);
        f("dash_cost", &mut p.dash_cost);
        f("preferred_distance", &mut p.preferred_distance);
        f("distance_weight", &mut p.distance_weight);
        f("goal_weight", &mut p.goal_weight);
        f("incoming_weight", &mut p.incoming_weight);
        f("step_cost_under_fire", &mut p.step_cost_under_fire);
        f("slam_risk", &mut p.slam_risk);
        f("stuck_penalty", &mut p.stuck_penalty);
        f("tempo_weight", &mut p.tempo_weight);
        f("press_bonus", &mut p.press_bonus);
        f("prior_weight", &mut p.prior_weight);
        f("their_dash_cost", &mut p.their_dash_cost);
        f("dodge_prob", &mut p.dodge_prob);
        f("dodge_prob_far", &mut p.dodge_prob_far);
        f("frost_combo", &mut p.frost_combo);
        f("frost_wait_cost", &mut p.frost_wait_cost);
        f("move_prior_weight", &mut p.move_prior_weight);
        f("tree_value", &mut p.tree_value);
        f("tree_c", &mut p.tree_c);
        f("tree_net", &mut p.tree_net);
        f("tree_net_hp", &mut p.tree_net_hp);
        f("tree_net_value", &mut p.tree_net_value);
        f("tree_net_cap", &mut p.tree_net_cap);
        f("tree_win_shape", &mut p.tree_win_shape);
        f("tree_slam_value", &mut p.tree_slam_value);
        f("tree_loaded_value", &mut p.tree_loaded_value);
        f("tree_heal_value", &mut p.tree_heal_value);
        f("tree_strategy", &mut p.tree_strategy);
        f("tree_slam_prior", &mut p.tree_slam_prior);
        if let Some(a) = v.get("tree_phase_gens").and_then(Value::as_array) {
            p.tree_phase_gens = a.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect();
        }
        f("tree_even", &mut p.tree_even);
        f("pad_bonus", &mut p.pad_bonus);
        f("attack_prior_weight", &mut p.attack_prior_weight);
        f("dash_cost_frosted", &mut p.dash_cost_frosted);
        f("dash_dodge", &mut p.dash_dodge);
        f("dash_dodge_slowed", &mut p.dash_dodge_slowed);
        f("plan_margin", &mut p.plan_margin);
        f("plan_solve_weight", &mut p.plan_solve_weight);
        f("plan_fire_cost", &mut p.plan_fire_cost);
        if let Some(x) = v.get("plan_depth").and_then(Value::as_u64) {
            p.plan_depth = x as u32;
        }
        if let Some(x) = v.get("plan_width").and_then(Value::as_u64) {
            p.plan_width = x as usize;
        }
        f("prior_hold", &mut p.prior_hold);
        let u = |k: &str| v.get(k).and_then(Value::as_u64);
        if let Some(x) = u("depth") {
            p.depth = x as u32;
        }
        if let Some(x) = u("deep3_rows") {
            p.deep3_rows = x as usize;
        }
        if let Some(x) = u("inner_rows") {
            p.inner_rows = x as usize;
        }
        if let Some(x) = u("inner_cols") {
            p.inner_cols = x as usize;
        }
        if let Some(x) = u("think_ms") {
            p.think_ms = x;
        }
        if let Some(x) = u("threads") {
            p.threads = x as usize;
        }
        if let Some(x) = u("hard_ms") {
            p.hard_ms = x;
        }
        if let Some(x) = v.get("stuck_range").and_then(Value::as_i64) {
            p.stuck_range = x as i32;
        }
        if let Some(x) = v.get("health_pad_missing_hp").and_then(Value::as_i64) {
            p.health_pad_missing_hp = x as i32;
        }
        if let Some(x) = v.get("search").and_then(Value::as_bool) {
            p.search = x;
        }
        for (key, slot) in [("model_attacks", &mut p.model_attacks), ("sample", &mut p.sample), ("lead_shots", &mut p.lead_shots), ("model_repulsor", &mut p.model_repulsor), ("dash_slam", &mut p.dash_slam), ("model_shots", &mut p.model_shots), ("model_slams", &mut p.model_slams), ("rollout_punish", &mut p.rollout_punish), ("prior_model", &mut p.prior_model), ("pad_race", &mut p.pad_race), ("tree_lead", &mut p.tree_lead), ("tree_slam", &mut p.tree_slam), ("tree_own_priors", &mut p.tree_own_priors), ("tree_dash_burst", &mut p.tree_dash_burst), ("tree_lead_them", &mut p.tree_lead_them), ("tree_reuse", &mut p.tree_reuse), ("tree_plans", &mut p.tree_plans), ("tree_plan_playout", &mut p.tree_plan_playout), ("tree_strategy_own", &mut p.tree_strategy_own)] {
            if let Some(x) = v.get(key).and_then(Value::as_bool) {
                *slot = x;
            }
        }
        if let Some(x) = v.get("tree_ms").and_then(Value::as_u64) {
            p.tree_ms = x;
        }
        if let Some(x) = v.get("tree_fog").and_then(Value::as_i64) {
            p.tree_fog = x as i32;
        }
        if let Some(x) = v.get("tree_depth_buff").and_then(Value::as_u64) {
            p.tree_depth_buff = x as u32;
        }
        if let Some(x) = v.get("tree_quiet_buff").and_then(Value::as_u64) {
            p.tree_quiet_buff = x as u32;
        }
        if let Some(x) = v.get("tree_solve").and_then(Value::as_u64) {
            p.tree_solve = x as u32;
        }
        if let Some(x) = v.get("tree_max_nodes").and_then(Value::as_u64) {
            p.tree_max_nodes = x as u32;
        }
        if let Some(x) = v.get("tree_expand_at").and_then(Value::as_u64) {
            p.tree_expand_at = x as u32;
        }
        if let Some(x) = v.get("tree_fog_samples").and_then(Value::as_u64) {
            p.tree_fog_samples = x as u32;
        }
        if let Some(x) = v.get("tree_threads").and_then(Value::as_u64) {
            p.tree_threads = x as u32;
        }
        if let Some(x) = v.get("tree_quiet").and_then(Value::as_u64) {
            p.tree_quiet = x as u32;
        }
        if let Some(x) = v.get("tree_gen").and_then(Value::as_u64) {
            p.tree_gen = x as u32;
        }
        if let Some(x) = v.get("tree_depth").and_then(Value::as_u64) {
            p.tree_depth = x as u32;
        }
        if let Some(x) = v.get("frost_hold_grace").and_then(Value::as_i64) {
            p.frost_hold_grace = x as i32;
        }
        if let Some(x) = v.get("rollout_ticks").and_then(Value::as_i64) {
            p.rollout_ticks = x as i32;
        }
        if let Some(x) = v.get("deepen").and_then(Value::as_i64) {
            p.deepen = x.max(0) as usize;
        }
        if let Some(x) = v.get("deep_rollout").and_then(Value::as_i64) {
            p.deep_rollout = x as i32;
        }
        if let Some(x) = v.get("deep_range").and_then(Value::as_i64) {
            p.deep_range = x as i32;
        }
        p
    }
}

pub struct Decision {
    pub turn: RawTurn,
    /// engine ticks simulated to reach it
    pub sims: u32,
    pub score: f64,
    /// short label of why, for the log
    pub note: &'static str,
}

/// Walking distance from every tile to `goal` (8 directions, the engine's corner rule).
fn distances(e: &Engine, goal: P) -> Vec<i32> {
    let (w, h) = (e.map.w, e.map.h);
    let mut dist = vec![i32::MAX; (w * h) as usize];
    if e.map.is_wall(goal) {
        return dist;
    }
    let mut q = VecDeque::new();
    dist[(goal[1] * w + goal[0]) as usize] = 0;
    q.push_back(goal);
    while let Some(p) = q.pop_front() {
        let d = dist[(p[1] * w + p[0]) as usize];
        for dir in 0..8u8 {
            // Walking is symmetric under the corner rule, so searching outward from the goal is fine.
            if e.map.step_blocked(p, dir) {
                continue;
            }
            let n = add(p, DIR_VEC[dir as usize]);
            let i = (n[1] * w + n[0]) as usize;
            if dist[i] == i32::MAX {
                dist[i] = d + 1;
                q.push_back(n);
            }
        }
    }
    dist
}

/// The learned priors: models of how the ladder's strong bots move and attack, fitted
/// from replays (`victris_engine::models`).
mod movemodel {
    use victris_engine::geom::{chebyshev, P};
    use victris_engine::models;
    use victris_engine::movefeat::{features, N};
    use victris_engine::state::State;
    use victris_engine::Engine;

    pub fn feats(e: &Engine, s: &State, x: usize) -> [f32; N] {
        features(e, s, x)
    }

    /// Probabilities of hold, toward, away, sideways, dash.
    pub fn probs(f: &[f32; N]) -> [f32; 5] {
        models::move_probs(f)
    }

    /// Log odds that a strong bot uses the attack here: 0 shot, 1 frostburst, 2 repulsor.
    pub fn attack_log_odds(f: &[f32; N], which: usize) -> f64 {
        let w = match which {
            0 => &models::SHOT_WEIGHTS,
            1 => &models::FROST_WEIGHTS,
            _ => &models::BURST_WEIGHTS,
        };
        let p = models::yes_prob(w, f).clamp(0.02, 0.98) as f64;
        (p / (1.0 - p)).ln()
    }

    /// The class of one of our candidate moves (`kind`: 0 stay, 1 step, 2 dash).
    pub fn class(kind: u8, from: P, dest: P, enemy: P) -> usize {
        match kind {
            0 => 0,
            2 => 4,
            _ => {
                let (before, after) = (chebyshev(from, enemy), chebyshev(dest, enemy));
                if after < before { 1 } else if after > before { 2 } else { 3 }
            }
        }
    }
}

/// The measured opponent: how the ladder's strong bots act in a given tempo situation.
mod oppmodel {
    use super::Kit;
    use victris_engine::geom::{add, chebyshev, DIR_VEC};
    use victris_engine::rules::AttackKind;
    use victris_engine::state::Fighter;
    use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
    use victris_engine::Engine;

    // Generated by `oppstats --emit` from 922 replay games of the three top owners' bots, house-mbot and house-champion.
    // [step toward, step away, step sideways, dash | dash ready, shot, frostburst | ready, repulsor | ready within 2]
    pub const TABLE: [[f32; 7]; 24] = [
        [0.267, 0.225, 0.135, 0.037, 0.000, 0.169, 0.324], // 1-2 tiles | own shot away  | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.192, 0.000, 0.274, 0.243], // 1-2 tiles | own shot away  | their shot away  | step spent
        [0.168, 0.303, 0.204, 0.055, 0.000, 0.179, 0.396], // 1-2 tiles | own shot away  | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.426, 0.000, 0.257, 0.294], // 1-2 tiles | own shot away  | their shot ready | step spent
        [0.262, 0.177, 0.133, 0.022, 0.541, 0.095, 0.176], // 1-2 tiles | own shot ready | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.157, 0.619, 0.091, 0.124], // 1-2 tiles | own shot ready | their shot away  | step spent
        [0.174, 0.309, 0.259, 0.036, 0.569, 0.075, 0.267], // 1-2 tiles | own shot ready | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.320, 0.624, 0.095, 0.207], // 1-2 tiles | own shot ready | their shot ready | step spent
        [0.346, 0.201, 0.161, 0.074, 0.000, 0.186, 0.000], // 3-4 tiles | own shot away  | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.195, 0.000, 0.247, 0.000], // 3-4 tiles | own shot away  | their shot away  | step spent
        [0.157, 0.248, 0.176, 0.076, 0.000, 0.140, 0.000], // 3-4 tiles | own shot away  | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.171, 0.000, 0.238, 0.000], // 3-4 tiles | own shot away  | their shot ready | step spent
        [0.470, 0.113, 0.137, 0.068, 0.271, 0.080, 0.000], // 3-4 tiles | own shot ready | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.236, 0.185, 0.133, 0.000], // 3-4 tiles | own shot ready | their shot away  | step spent
        [0.189, 0.091, 0.119, 0.046, 0.172, 0.040, 0.000], // 3-4 tiles | own shot ready | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.141, 0.203, 0.148, 0.000], // 3-4 tiles | own shot ready | their shot ready | step spent
        [0.571, 0.175, 0.101, 0.043, 0.000, 0.197, 0.000], // 5-6 tiles | own shot away  | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.076, 0.000, 0.312, 0.000], // 5-6 tiles | own shot away  | their shot away  | step spent
        [0.374, 0.204, 0.116, 0.081, 0.000, 0.153, 0.000], // 5-6 tiles | own shot away  | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.074, 0.000, 0.261, 0.000], // 5-6 tiles | own shot away  | their shot ready | step spent
        [0.580, 0.164, 0.088, 0.056, 0.202, 0.162, 0.000], // 5-6 tiles | own shot ready | their shot away  | step ready
        [0.000, 0.000, 0.000, 0.172, 0.314, 0.211, 0.000], // 5-6 tiles | own shot ready | their shot away  | step spent
        [0.372, 0.134, 0.086, 0.089, 0.249, 0.136, 0.000], // 5-6 tiles | own shot ready | their shot ready | step ready
        [0.000, 0.000, 0.000, 0.078, 0.339, 0.251, 0.000], // 5-6 tiles | own shot ready | their shot ready | step spent
    ];

    /// A probability for each of the opponent's columns (its candidate turns), from the
    /// table row of its situation: range, whose shot is ready, whether it can step.
    /// A movement class's share is split evenly over the columns in it; attacks are
    /// taken as independent of movement. Sums to 1.
    pub fn prior(e: &Engine, columns: &[RawTurn], en: &Fighter, my: &Fighter, k: &Kit) -> Vec<f64> {
        let d = chebyshev(en.pos, my.pos);
        let band = if d <= 2 { 0 } else if d <= 4 { 1 } else { 2 };
        let ready = |f: &Fighter| k.ranged.map(|a| f.cd_attack[a as usize] == 0).unwrap_or(false);
        let idx = band * 8 + (ready(en) as usize) * 4 + (ready(my) as usize) * 2 + (en.cd_move != 0) as usize;
        let row = TABLE[idx];
        // 0 hold, 1 toward, 2 away, 3 sideways, 4 dash
        let class = |t: &RawTurn| -> usize {
            if matches!(t.dash, DirIn::Dir(_)) {
                4
            } else if let DirIn::Dir(dir) = t.mv {
                let after = chebyshev(add(en.pos, DIR_VEC[dir as usize]), my.pos);
                if after < d { 1 } else if after > d { 2 } else { 3 }
            } else {
                0
            }
        };
        let mut count = [0usize; 5];
        for t in columns {
            if t.attack == AttackIn::None {
                count[class(t)] += 1;
            }
        }
        let mut share = [0.0f64, row[0] as f64, row[1] as f64, row[2] as f64, row[3] as f64];
        for c in 1..5 {
            if count[c] == 0 {
                share[c] = 0.0;
            }
        }
        share[0] = (1.0 - share[1..].iter().sum::<f64>()).max(0.02);
        let (p_shot, p_burst) = (row[4] as f64, row[6] as f64);
        let mut out: Vec<f64> = columns
            .iter()
            .map(|t| {
                let c = class(t);
                let n = count[c].max(1) as f64;
                let attack = match t.attack {
                    AttackIn::None => {
                        let has = |want: u8| {
                            columns.iter().any(|o| o.mv == t.mv && o.dash == t.dash && matches!(o.attack, AttackIn::Named { attack, .. } if attack == want))
                        };
                        let mut none = 1.0;
                        if k.ranged.map(has).unwrap_or(false) {
                            none -= p_shot;
                        }
                        if k.repulsor.map(has).unwrap_or(false) {
                            none -= p_burst;
                        }
                        none.max(0.0)
                    }
                    AttackIn::Named { attack, target } => match e.rules.attacks[attack as usize].kind {
                        // only the shot at where we stand; a shot led at another tile is the solver's business
                        AttackKind::Projectile { .. } if target == TargetIn::Tile(my.pos) => p_shot,
                        AttackKind::Burst { .. } => p_burst,
                        _ => 0.0,
                    },
                    AttackIn::Unknown => 0.0,
                };
                share[c] / n * attack
            })
            .collect();
        let total: f64 = out.iter().sum();
        if total > 0.0 {
            out.iter_mut().for_each(|p| *p /= total);
        }
        out
    }
}

struct Kit {
    ranged: Option<u8>,
    frost: Option<(u8, i32)>,
    repulsor: Option<u8>,
}

fn kit(e: &Engine) -> Kit {
    let mut k = Kit { ranged: None, frost: None, repulsor: None };
    for (i, a) in e.rules.attacks.iter().enumerate() {
        match a.kind {
            AttackKind::Projectile { .. } if k.ranged.is_none() => k.ranged = Some(i as u8),
            AttackKind::Area { range, .. } if k.frost.is_none() => k.frost = Some((i as u8, range)),
            AttackKind::Burst { .. } if k.repulsor.is_none() => k.repulsor = Some(i as u8),
            _ => {}
        }
    }
    k
}

/// One simulated tick's outcome for us: positive is good.
fn outcome(e: &Engine, before: &State, me: usize, turns: &[RawTurn; 2], pb: &Playbook, sims: &mut u32) -> f64 {
    let mut s = *before;
    e.step(&mut s, turns);
    *sims += 1;
    let (my0, en0) = (before.fighters[me], before.fighters[1 - me]);
    let mut v = (en0.hp - s.fighters[1 - me].hp) as f64 - (my0.hp - s.fighters[me].hp) as f64;
    if s.fighters[1 - me].state == FState::Stunned && en0.state != FState::Stunned {
        v += pb.stun_bonus;
    }
    if s.fighters[me].state == FState::Stunned && my0.state != FState::Stunned {
        v -= pb.stun_bonus;
    }
    if !s.fighters[1 - me].alive {
        v += 1000.0;
    }
    if !s.fighters[me].alive {
        v -= 1000.0;
    }
    // While the opponent cannot act, our shots already in the air land for sure: count them.
    let mut k = 0;
    while s.result.is_none() && k < 3 && s.fighters[1 - me].state != FState::Idle {
        let hp = s.fighters[1 - me].hp;
        e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
        *sims += 1;
        v += (hp - s.fighters[1 - me].hp) as f64;
        if !s.fighters[1 - me].alive {
            v += 1000.0;
        }
        k += 1;
    }
    v
}

/// One cell of the matrix game: both turns played on the exact engine, then a few ticks
/// in which each side only steps out of what would hit it. The value is hp taken from
/// the opponent minus hp lost, with a bonus for stuns and a large one for a kill.
fn tick_value(e: &Engine, before: &State, me: usize, turns: &[RawTurn; 2], pb: &Playbook, rollout: i32, leaf: bool, sims: &mut u32) -> (f64, State) {
    let mut s = *before;
    e.step(&mut s, turns);
    *sims += 1;
    let after_tick = s;
    let (my0, en0) = (before.fighters[me], before.fighters[1 - me]);
    let mut v = 0.0;
    if s.fighters[1 - me].state == FState::Stunned && en0.state != FState::Stunned {
        v += pb.stun_bonus;
    }
    if s.fighters[me].state == FState::Stunned && my0.state != FState::Stunned {
        v -= pb.stun_bonus;
    }
    for r in 0..rollout {
        if s.result.is_some() {
            break;
        }
        let base = s;
        // What each side does unprompted: nothing, or with the punish policy the free
        // hit when the other cannot move.
        let mut plan = [RawTurn::IDLE, RawTurn::IDLE];
        if pb.rollout_punish {
            plan = [quick_turn(e, &base, 0), quick_turn(e, &base, 1)];
        }
        let sum = e.step(&mut s, &plan);
        *sims += 1;
        if sum.hp_lost == [0, 0] {
            continue;
        }
        // Someone would be hit: whoever can still step, and was not already moving,
        // looks for a way out (keeping any attack it had planned).
        let mut dodge = plan;
        let mut any = false;
        let mut dashed = 0.0; // the chance the opponent really dashes out, when the model lets it
        for x in 0..2 {
            let f = &base.fighters[x];
            if sum.hp_lost[x] == 0 || f.state != FState::Idle || plan[x].mv != victris_engine::turn::DirIn::None {
                continue;
            }
            let mut stepped = false;
            if f.cd_move == 0 {
                for d in 0..8u8 {
                    if e.map.step_blocked(f.pos, d) {
                        continue;
                    }
                    let mut trial = base;
                    let mut t2 = dodge;
                    t2[x].mv = victris_engine::turn::DirIn::Dir(d);
                    let r = e.step(&mut trial, &t2);
                    *sims += 1;
                    if r.hp_lost[x] == 0 {
                        dodge[x].mv = victris_engine::turn::DirIn::Dir(d);
                        any = true;
                        stepped = true;
                        break;
                    }
                }
            }
            // No step saves it. The opponent may spend its dash instead, and a slowed one
            // usually does.
            let p = if f.cd_move >= 2 { pb.dash_dodge_slowed } else { pb.dash_dodge };
            if !stepped && x != me && p > 0.0 && f.cd_dash == 0 && plan[x].dash == victris_engine::turn::DirIn::None {
                for d in 0..8u8 {
                    let mut trial = base;
                    let mut t2 = dodge;
                    t2[x].dash = victris_engine::turn::DirIn::Dir(d);
                    let r = e.step(&mut trial, &t2);
                    *sims += 1;
                    if r.hp_lost[x] == 0 {
                        dodge[x].dash = victris_engine::turn::DirIn::Dir(d);
                        any = true;
                        dashed = p;
                        break;
                    }
                }
            }
        }
        if any {
            // A reactive dodge is not certain. Replays show a target whose step was spent
            // when the shot left still gets hit about four times in five, so the damage
            // the opponent side-stepped here is credited at the rate it really lands.
            if dodge[1 - me].mv != plan[1 - me].mv {
                let p = if r > 0 && pb.dodge_prob_far >= 0.0 { pb.dodge_prob_far } else { pb.dodge_prob };
                v += (1.0 - p) * sum.hp_lost[1 - me] as f64;
            }
            if dashed > 0.0 {
                v += (1.0 - dashed) * sum.hp_lost[1 - me] as f64;
            }
            s = base;
            e.step(&mut s, &dodge);
            *sims += 1;
        }
    }
    v += (en0.hp - s.fighters[1 - me].hp) as f64 - (my0.hp - s.fighters[me].hp) as f64;
    if !s.fighters[1 - me].alive {
        v += 1000.0;
    }
    if !s.fighters[me].alive {
        v -= 1000.0;
    }
    // At a leaf, add what the position is worth beyond the ticks played: the fitted
    // tempo value of the state right after the searched tick.
    if leaf && pb.tempo_weight != 0.0 && after_tick.result.is_none() {
        v += pb.tempo_weight * victris_engine::tempo::value(e, &after_tick, me, &victris_engine::tempo::WEIGHTS) as f64;
    }
    (v, after_tick)
}

/// The turn a fighter takes in a rollout under the punish policy: the hit that cannot
/// be avoided, if there is one, and otherwise nothing (it keeps its step in hand).
/// Cheap by design: a few comparisons, no simulation.
fn quick_turn(e: &Engine, s: &State, x: usize) -> RawTurn {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if f.state != FState::Idle || !o.alive {
        return RawTurn::IDLE;
    }
    let d = chebyshev(f.pos, o.pos);
    let stuck = o.state != FState::Idle || o.cd_move > 0;
    if !stuck || d > 3 {
        return RawTurn::IDLE;
    }
    let mut shot = None;
    let mut burst = None;
    for (i, a) in e.rules.attacks.iter().enumerate() {
        if f.cd_attack[i] != 0 {
            continue;
        }
        match a.kind {
            AttackKind::Projectile { .. } if shot.is_none() => shot = Some(i as u8),
            AttackKind::Burst { push } if burst.is_none() => burst = Some((i as u8, push)),
            _ => {}
        }
    }
    // Next to a fighter that cannot move, with a wall behind it: the slam.
    if let Some((a, push)) = burst {
        if d == 1 && !o.invulnerable {
            if let Some(dir) = octant(f.pos, o.pos) {
                let mut p = o.pos;
                for _ in 0..push {
                    if e.map.step_blocked(p, dir) {
                        return RawTurn::IDLE.with_burst(a);
                    }
                    p = add(p, DIR_VEC[dir as usize]);
                }
            }
        }
    }
    if let Some(a) = shot {
        if !e.map.has_line_of_sight(f.pos, o.pos) {
            return RawTurn::IDLE;
        }
        // Within two tiles the shot lands this tick; from three, step in and fire.
        if d <= 2 {
            return RawTurn::IDLE.with_attack(a, o.pos);
        }
        if f.cd_move == 0 {
            let want = [(o.pos[0] - f.pos[0]).signum(), (o.pos[1] - f.pos[1]).signum()];
            if let Some(dir) = (0..8u8).find(|i| DIR_VEC[*i as usize] == want) {
                if !e.map.step_blocked(f.pos, dir) {
                    return RawTurn::step(dir).with_attack(a, o.pos);
                }
            }
        }
    }
    RawTurn::IDLE
}

/// The opponent as the plan search imagines it: it takes the hit we cannot avoid, walks
/// out of our frostburst while it can, steps out of anything else about to hit it, and
/// otherwise keeps its step in hand. Simple on purpose; the plan search is only trusted
/// among turns the game solve already rates as safe.
fn model_turn(e: &Engine, s: &State, x: usize, avoided: &mut i32, sims: &mut u32) -> RawTurn {
    *avoided = 0;
    let mut t = quick_turn(e, s, x);
    let f = &s.fighters[x];
    if f.state != FState::Idle || f.cd_move != 0 || t.mv != victris_engine::turn::DirIn::None {
        return t;
    }
    // Standing in the opponent's area: leave by the shortest way out.
    let inside = |p: P| s.areas().iter().any(|b| b.owner as usize != x && e.map.in_blast(b.pos, b.radius, p));
    if inside(f.pos) {
        let mut best: Option<(i32, u8)> = None;
        for d in 0..8u8 {
            if e.map.step_blocked(f.pos, d) {
                continue;
            }
            let q = add(f.pos, DIR_VEC[d as usize]);
            if q == s.fighters[1 - x].pos {
                continue;
            }
            let score = if inside(q) { 1 } else { 0 };
            if best.map(|(b, _)| score < b).unwrap_or(true) {
                best = Some((score, d));
            }
        }
        if let Some((_, d)) = best {
            t.mv = victris_engine::turn::DirIn::Dir(d);
            return t;
        }
    }
    // About to be hit by something in flight: step aside if a step avoids it.
    let turns = |mine: RawTurn| if x == 0 { [mine, RawTurn::IDLE] } else { [RawTurn::IDLE, mine] };
    let mut probe = *s;
    let would_lose = e.step(&mut probe, &turns(t)).hp_lost[x];
    let hit = would_lose > 0;
    *sims += 1;
    if hit {
        for d in 0..8u8 {
            if e.map.step_blocked(f.pos, d) {
                continue;
            }
            let mut trial = *s;
            let mut alt = t;
            alt.mv = victris_engine::turn::DirIn::Dir(d);
            let lost = e.step(&mut trial, &turns(alt)).hp_lost[x];
            *sims += 1;
            if lost == 0 {
                *avoided = would_lose;
                return alt;
            }
        }
    }
    t
}

/// Our options at a node of the plan search: stay or step, alone or with the attack that
/// fits. Dashes are left out: they are for emergencies, which the game solve handles.
fn plan_options(e: &Engine, s: &State, me: usize, k: &Kit, pb: &Playbook, out: &mut Vec<(RawTurn, f64)>) {
    out.clear();
    let (f, o) = (&s.fighters[me], &s.fighters[1 - me]);
    if f.state != FState::Idle {
        out.push((RawTurn::IDLE, 0.0));
        return;
    }
    let shot = k.ranged.filter(|a| f.cd_attack[*a as usize] == 0);
    let burst = k.repulsor.filter(|a| f.cd_attack[*a as usize] == 0);
    let frost = k.frost.filter(|(a, _)| f.cd_attack[*a as usize] == 0);
    let add_move = |mv: RawTurn, dest: P, out: &mut Vec<(RawTurn, f64)>| {
        out.push((mv, 0.0));
        let d = chebyshev(dest, o.pos);
        if let Some(a) = shot {
            if d <= 4 {
                out.push((mv.with_attack(a, o.pos), pb.plan_fire_cost));
            }
        }
        if let Some(a) = burst {
            if d <= 1 {
                out.push((mv.with_burst(a), pb.repulsor_cost));
            }
        }
    };
    add_move(RawTurn::IDLE, f.pos, out);
    if let Some((a, range)) = frost {
        let d = chebyshev(f.pos, o.pos);
        if d >= 2 && d <= range && can_see(&e.rules, &e.map, f.pos, o.pos) {
            out.push((RawTurn::IDLE.with_attack(a, o.pos), pb.frost_cost));
        }
    }
    if f.cd_move == 0 {
        for d in 0..8u8 {
            if e.map.step_blocked(f.pos, d) {
                continue;
            }
            let dest = add(f.pos, DIR_VEC[d as usize]);
            if dest != o.pos {
                add_move(RawTurn::step(d), dest, out);
            }
        }
    }
}

/// The plan search: after our first turn (and the opponent's modelled reply), a beam
/// search over our next turns against the opponent model, `plan_depth` ticks deep.
/// Returns the best total found: hp taken minus hp lost, a kill bonus, and the tempo
/// value where the plan ends. This is where lines that take several ticks to pay off,
/// such as a frostburst that forces a step and sets up the sure shot, get their value.
fn plan_value(e: &Engine, s: &State, me: usize, first: RawTurn, pb: &Playbook, k: &Kit, sims: &mut u32) -> f64 {
    let turns = |mine: RawTurn, theirs: RawTurn| if me == 0 { [mine, theirs] } else { [theirs, mine] };
    let (my0, en0) = (s.fighters[me].hp, s.fighters[1 - me].hp);
    let score = |st: &State, cost: f64| -> f64 {
        let mut v = (en0 - st.fighters[1 - me].hp) as f64 - (my0 - st.fighters[me].hp) as f64 - cost;
        if !st.fighters[1 - me].alive {
            v += 1000.0;
        }
        if !st.fighters[me].alive {
            v -= 1000.0;
        }
        if st.result.is_none() && pb.tempo_weight != 0.0 {
            v += pb.tempo_weight * victris_engine::tempo::value(e, st, me, &victris_engine::tempo::WEIGHTS) as f64;
        }
        v
    };
    let mut first_state = *s;
    let mut avoided = 0;
    let reply = model_turn(e, s, 1 - me, &mut avoided, sims);
    e.step(&mut first_state, &turns(first, reply));
    *sims += 1;
    // Damage the modelled opponent side-stepped counts at the rate such dodges fail (a negative cost).
    let credit = -(1.0 - pb.dodge_prob) * avoided as f64;
    let mut beam: Vec<(f64, f64, State)> = vec![(score(&first_state, credit), credit, first_state)]; // (score, cost so far, state)
    let mut best = beam[0].0;
    let mut options: Vec<(RawTurn, f64)> = Vec::with_capacity(32);
    let mut next: Vec<(f64, f64, State)> = Vec::with_capacity(512);
    for _ in 1..pb.plan_depth {
        next.clear();
        for (_, cost, st) in &beam {
            if st.result.is_some() {
                next.push((score(st, *cost), *cost, *st));
                continue;
            }
            let mut avoided = 0;
            let reply = model_turn(e, st, 1 - me, &mut avoided, sims);
            let credit = -(1.0 - pb.dodge_prob) * avoided as f64;
            plan_options(e, st, me, k, pb, &mut options);
            for (mine, c) in &options {
                let mut child = *st;
                e.step(&mut child, &turns(*mine, reply));
                *sims += 1;
                let total = cost + c + credit;
                next.push((score(&child, total), total, child));
            }
        }
        if next.is_empty() {
            break;
        }
        next.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        next.truncate(pb.plan_width.max(1));
        std::mem::swap(&mut beam, &mut next);
        best = beam[0].0;
    }
    best
}

fn cell_value(e: &Engine, before: &State, me: usize, turns: &[RawTurn; 2], pb: &Playbook, sims: &mut u32) -> f64 {
    tick_value(e, before, me, turns, pb, pb.rollout_ticks, true, sims).0
}

/// Where a dash from `from` in `dir` would stop, ignoring anything that moves meanwhile.
fn dash_end(e: &Engine, s: &State, mover: usize, dir: u8) -> P {
    let mut p = s.fighters[mover].pos;
    let other = s.fighters[1 - mover].pos;
    for _ in 0..e.rules.dash_distance {
        if e.map.step_blocked(p, dir) {
            break;
        }
        let n = add(p, DIR_VEC[dir as usize]);
        if n == other {
            break;
        }
        p = n;
    }
    p
}

/// The value for `me` of the tick that starts in `s`, with both sides choosing: our
/// movements with or without a shot or a repulsor, against the opponent's movements.
/// This is the second tick of the deepened search, so it keeps to the turns that decide
/// exchanges and leaves positional merits to the first tick.
fn subgame_value(e: &Engine, s: &State, me: usize, pb: &Playbook, k: &Kit, depth: u32, deadline: Option<Instant>, sims: &mut u32) -> f64 {
    let turns = |mine: RawTurn, theirs: RawTurn| if me == 0 { [mine, theirs] } else { [theirs, mine] };
    let (my, en) = (s.fighters[me], s.fighters[1 - me]);
    let mut mine: Vec<(RawTurn, f64)> = Vec::with_capacity(52);
    if my.state != FState::Idle {
        mine.push((RawTurn::IDLE, 0.0));
    } else {
        let shot = k.ranged.filter(|a| my.cd_attack[*a as usize] == 0);
        let burst = k.repulsor.filter(|a| my.cd_attack[*a as usize] == 0);
        let add_with = |mv: RawTurn, dest: P, cost: f64, mine: &mut Vec<(RawTurn, f64)>| {
            mine.push((mv, cost));
            if let Some(a) = shot {
                mine.push((mv.with_attack(a, en.pos), cost + pb.fire_cost));
            }
            if let Some(a) = burst {
                if chebyshev(dest, en.pos) <= 2 {
                    mine.push((mv.with_burst(a), cost + pb.repulsor_cost));
                }
            }
        };
        add_with(RawTurn::IDLE, my.pos, 0.0, &mut mine);
        for d in 0..8u8 {
            if my.cd_move == 0 && !e.map.step_blocked(my.pos, d) {
                let dest = add(my.pos, DIR_VEC[d as usize]);
                if dest != en.pos {
                    add_with(RawTurn::step(d), dest, 0.0, &mut mine);
                }
            }
            if my.cd_dash == 0 {
                let dest = dash_end(e, s, me, d);
                if dest != my.pos {
                    add_with(RawTurn::dash(d), dest, pb.dash_cost, &mut mine);
                }
            }
        }
    }
    let mut theirs: Vec<(RawTurn, f64)> = vec![(RawTurn::IDLE, 0.0)];
    if en.state == FState::Idle {
        for d in 0..8u8 {
            if en.cd_move == 0 && !e.map.step_blocked(en.pos, d) {
                theirs.push((RawTurn::step(d), 0.0));
            }
            if en.cd_dash == 0 {
                theirs.push((RawTurn::dash(d), pb.their_dash_cost));
            }
        }
        let plain = theirs.len();
        if pb.model_shots {
            if let Some(a) = k.ranged.filter(|a| en.cd_attack[*a as usize] == 0) {
                for i in 0..plain {
                    let (mv, cost) = theirs[i];
                    theirs.push((mv.with_attack(a, my.pos), cost + pb.fire_cost));
                }
            }
        }
        if pb.model_slams && chebyshev(en.pos, my.pos) <= 2 + e.rules.dash_distance {
            if let Some(a) = k.repulsor.filter(|a| en.cd_attack[*a as usize] == 0) {
                for i in 0..plain {
                    let (mv, cost) = theirs[i];
                    theirs.push((mv.with_burst(a), cost + pb.repulsor_cost));
                }
            }
        }
    }
    let (rows, cols) = (mine.len(), theirs.len());
    let mut payoff = vec![0.0f64; rows * cols];
    for (m, (mt, cost)) in mine.iter().enumerate() {
        for (c, (tt, their_cost)) in theirs.iter().enumerate() {
            payoff[m * cols + c] = tick_value(e, s, me, &turns(*mt, *tt), pb, pb.deep_rollout, true, sims).0 - cost + their_cost;
        }
    }
    let (mix_r, mix_c, value) = solve_matrix(&payoff, rows, cols, 60);
    if depth <= 1 || deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
        return value;
    }
    // One tick further for the cells both sides are most likely to reach.
    let top = |mix: &[f64], n: usize| -> Vec<usize> {
        let mut order: Vec<usize> = (0..mix.len()).collect();
        order.sort_by(|a, b| mix[*b].partial_cmp(&mix[*a]).unwrap());
        order.truncate(n);
        order
    };
    for &m in &top(&mix_r, pb.inner_rows) {
        for &c in &top(&mix_c, pb.inner_cols) {
            let (v1, s1) = tick_value(e, s, me, &turns(mine[m].0, theirs[c].0), pb, 0, false, sims);
            let later = if s1.result.is_none() { subgame_value(e, &s1, me, pb, k, depth - 1, deadline, sims) } else { 0.0 };
            payoff[m * cols + c] = v1 + later - mine[m].1 + theirs[c].1;
        }
    }
    solve_matrix(&payoff, rows, cols, 60).2
}

/// Solves a zero-sum matrix game by regret matching. `a[m * cols + e]` is the row
/// player's payoff. Returns both players' average strategies and the game's value.
fn solve_matrix(a: &[f64], rows: usize, cols: usize, iters: usize) -> (Vec<f64>, Vec<f64>, f64) {
    let mut rr = vec![0.0f64; rows];
    let mut rc = vec![0.0f64; cols];
    let mut avg_r = vec![0.0f64; rows];
    let mut avg_c = vec![0.0f64; cols];
    let mut sr = vec![0.0f64; rows];
    let mut sc = vec![0.0f64; cols];
    let mut ur = vec![0.0f64; rows];
    let mut uc = vec![0.0f64; cols];
    let strategy = |regret: &[f64], out: &mut [f64]| {
        let total: f64 = regret.iter().sum();
        if total > 0.0 {
            for (o, r) in out.iter_mut().zip(regret) {
                *o = r / total;
            }
        } else {
            out.fill(1.0 / regret.len() as f64);
        }
    };
    for it in 0..iters {
        strategy(&rr, &mut sr);
        strategy(&rc, &mut sc);
        ur.fill(0.0);
        uc.fill(0.0);
        for m in 0..rows {
            let row = &a[m * cols..(m + 1) * cols];
            let mut u = 0.0;
            for e in 0..cols {
                u += sc[e] * row[e];
                uc[e] += sr[m] * row[e];
            }
            ur[m] = u;
        }
        let v: f64 = sr.iter().zip(&ur).map(|(p, u)| p * u).sum();
        let w = (it + 1) as f64; // later iterations count for more
        for m in 0..rows {
            rr[m] = (rr[m] + ur[m] - v).max(0.0);
            avg_r[m] += w * sr[m];
        }
        for e in 0..cols {
            rc[e] = (rc[e] + v - uc[e]).max(0.0);
            avg_c[e] += w * sc[e];
        }
    }
    let tr: f64 = avg_r.iter().sum();
    let tc: f64 = avg_c.iter().sum();
    avg_r.iter_mut().for_each(|x| *x /= tr);
    avg_c.iter_mut().for_each(|x| *x /= tc);
    let mut value = 0.0;
    for m in 0..rows {
        for e in 0..cols {
            value += avg_r[m] * avg_c[e] * a[m * cols + e];
        }
    }
    (avg_r, avg_c, value)
}

/// ARENA_VISITS=file: for every tree decision with the opponent in view, one line with what
/// the search spent its visits on at the root (shares of hold / toward / away / sideways /
/// dash, then of the turns with a shot, a frostburst and a repulsor) and the 52 move features,
/// as targets for training the move model on the search's own visit counts (`movefit --visits`).
fn dump_visits(e: &victris_engine::Engine, s: &State, me: usize, acts: &[(RawTurn, u32, f64)]) {
    let Some(path) = std::env::var_os("ARENA_VISITS") else { return };
    let (f, o) = (&s.fighters[me], &s.fighters[1 - me]);
    if f.state != FState::Idle || !can_see(&e.rules, &e.map, f.pos, o.pos) {
        return;
    }
    let total: u32 = acts.iter().map(|a| a.1).sum();
    if total == 0 {
        return;
    }
    let mut mv = [0.0f64; 5];
    let mut atk = [0.0f64; 3];
    for (turn, n, _) in acts {
        let w = *n as f64 / total as f64;
        mv[victris_engine::movefeat::move_class(s, me, turn)] += w;
        if let victris_engine::turn::AttackIn::Named { attack, .. } = turn.attack {
            let k = match e.rules.attacks[attack as usize].kind {
                AttackKind::Projectile { .. } => 0,
                AttackKind::Area { .. } => 1,
                AttackKind::Burst { .. } => 2,
            };
            atk[k] += w;
        }
    }
    let feat = victris_engine::movefeat::features(e, s, me);
    let mut line = String::from("V");
    for v in mv.iter().chain(atk.iter()) {
        line.push_str(&format!(" {v:.4}"));
    }
    for v in feat.iter() {
        line.push_str(&format!(" {v:.3}"));
    }
    line.push('\n');
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = std::io::Write::write_all(&mut file, line.as_bytes());
    }
}

/// A search tree kept between ticks, per match (seed) and seat.
struct Kept {
    tick: i32,
    root: State,
    ours: RawTurn,
    tree: victris_engine::mcts::Tree,
}

thread_local! {
    static KEPT: std::cell::RefCell<std::collections::HashMap<(i64, usize), Kept>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// What the tree search found at the root of its latest decision on this thread: the position
/// it searched (the tracker's belief), each side's turns with their visits and average result
/// in hp for that side, the average result for fighter 0, and the iterations run and reused.
/// Only tools that show the search's thinking read it (`playground/`); the bot never does.
pub struct RootSeen {
    pub root: State,
    pub acts: [Vec<(RawTurn, u32, f64)>; 2],
    pub value: f64,
    pub sims: u32,
    pub reused: u32,
}

thread_local! {
    pub static LAST_ROOT: std::cell::RefCell<Option<RootSeen>> = const { std::cell::RefCell::new(None) };
}

pub fn decide(t: &Tracker, view: &View, pb: &Playbook) -> Decision {
    let started = Instant::now();
    let d = decide_inner(t, view, pb, started);
    t.charge_think(started.elapsed());
    d
}

fn decide_inner(t: &Tracker, view: &View, pb: &Playbook, started: Instant) -> Decision {
    let idle = Decision { turn: RawTurn::IDLE, sims: 0, score: 0.0, note: "cannot act" };
    let Some(s) = t.belief else { return idle };
    let e = &t.engine;
    let me = t.me;
    let my = s.fighters[me];
    let en = s.fighters[1 - me];
    // standing in the opponent's frostburst before it goes off
    let frosted = s.areas().iter().any(|b| b.owner as usize != me && t.engine.map.in_blast(b.pos, b.radius, my.pos));
    if my.state != FState::Idle {
        return idle;
    }
    // The opening book, while we are on its line and the opponent is out of sight (it only
    // places us; once the opponent is seen, the search decides).
    if let (Some(line), None) = (pb.book.get(&format!("{}/{}", e.map.name, me + 1)), view.enemy.as_ref()) {
        if let Some((_, _, turn)) = line.iter().find(|(tick, pos, _)| *tick == s.tick && *pos == my.pos) {
            return Decision { turn: RawTurn::from_json(Some(turn), &e.rules), sims: 0, score: 0.0, note: "book" };
        }
    }
    let k = kit(e);
    let visible = view.enemy.is_some();
    // The tree search as the whole brain: both sides search for their best move on the
    // exact engine for `tree_ms`, and we play our most visited turn. Only with `search` on:
    // the gateway's quick fallback calls us with `search` off and wants the plain heuristic.
    let guessed = pb.tree_fog > 0 && t.enemy_last_seen.map(|(tick, _)| s.tick - tick <= pb.tree_fog).unwrap_or(s.tick <= pb.tree_fog);
    if pb.tree_ms > 0 && pb.search && (visible || guessed) && t.think_cap_ms(pb.tree_ms) >= 20 {
        let mut cfg = victris_engine::mcts::Config { depth: pb.tree_depth, value_weight: pb.tree_value, value_gen: pb.tree_gen as usize, lead: pb.tree_lead, quiet_slam: pb.tree_slam, own_priors: pb.tree_own_priors, dash_burst: pb.tree_dash_burst, lead_mask: if pb.tree_lead_them { 1u8 << (1 - me) } else { 0 }, expand_at: pb.tree_expand_at, max_nodes: pb.tree_max_nodes, quiet: pb.tree_quiet, c: pb.tree_c, even: pb.tree_even as f32, ..Default::default() };
        if pb.tree_value != 0.0 || pb.tree_net != 0.0 || pb.tree_net_hp != 0.0 || pb.tree_net_value != 0.0 {
            cfg.buff_value = 0.0;
        }
        cfg.net_weight = pb.tree_net;
        cfg.net_hp = pb.tree_net_hp;
        cfg.net_value = pb.tree_net_value;
        cfg.net_cap = pb.tree_net_cap;
        cfg.plans = pb.tree_plans;
        cfg.plan_playout = pb.tree_plan_playout;
        cfg.win_shape = pb.tree_win_shape;
        cfg.slam_value = pb.tree_slam_value;
        cfg.loaded_value = pb.tree_loaded_value;
        cfg.heal_value = pb.tree_heal_value;
        cfg.strategy = pb.tree_strategy;
        cfg.slam_prior = pb.tree_slam_prior as f32;
        cfg.strategy_side = if pb.tree_strategy_own { me as u8 } else { 2 };
        for (k, g) in pb.tree_phase_gens.iter().take(victris_engine::valuefeat::PHASES).enumerate() {
            cfg.phase_gens[k] = *g;
        }
        if pb.tree_depth_buff > 0 && s.fighters.iter().any(|f| f.power_until != NO_BUFF && f.power_until > s.tick) {
            cfg.depth = pb.tree_depth_buff;
            if pb.tree_quiet_buff > 0 {
                cfg.quiet = pb.tree_quiet_buff;
            }
        }
        // The think time, cut down when the match budget could not carry it to the tick limit.
        let deadline = t.think_cap_ms(pb.tree_ms);
        // Out of sight, each search gets its own guess at where the opponent stands: the
        // first the tracker's ghost, the others a tile drawn from those it could be on. The
        // turns at the root are then pooled by turn, so a move good against most of the
        // guesses wins. (In sight every search starts from the same state.)
        let root_for = |k: u32| -> victris_engine::State {
            if visible || k == 0 {
                return s;
            }
            let w = e.map.w;
            let tiles: Vec<P> = t.possible.iter().enumerate().filter(|(_, b)| **b).map(|(i, _)| [(i as i32) % w, (i as i32) / w]).collect();
            if tiles.is_empty() {
                return s;
            }
            let mut rng = 0x9e3779b97f4a7c15u64.wrapping_mul(k as u64 + 7) ^ ((s.tick as u64) << 20) | 1;
            let pick = tiles[((victris_engine::clone::next(&mut rng) * tiles.len() as f32) as usize).min(tiles.len() - 1)];
            let mut st = s;
            st.fighters[1 - me].pos = pick;
            st
        };
        // Out of sight a thread can also split its time over several guesses
        // (`tree_fog_samples`), each searched on its own and pooled turn by turn.
        let per_thread = if visible { 1 } else { pb.tree_fog_samples.max(1) };
        let pool = |into: &mut victris_engine::mcts::RootResult, r: &victris_engine::mcts::RootResult| {
            for x in 0..2 {
                for b in r.acts[x].iter() {
                    match into.acts[x].iter_mut().find(|a| a.0 == b.0) {
                        Some(a) => {
                            let n = a.1 + b.1;
                            if n > 0 {
                                a.2 = (a.2 * a.1 as f64 + b.2 * b.1 as f64) / n as f64;
                            }
                            a.1 = n;
                        }
                        None => into.acts[x].push(*b),
                    }
                }
            }
        };
        // The starting states, made here: the tracker cannot be shared between threads.
        let starts: Vec<victris_engine::State> = (0..pb.tree_threads.max(1) * per_thread).map(root_for).collect();
        if pb.tree_reuse && pb.tree_threads <= 1 && per_thread == 1 && visible {
            // One search, seeded with the branch of last tick's tree that the game went into.
            let key = (t.seed, me);
            let kept = KEPT.with(|k| k.borrow_mut().remove(&key));
            let mut search = victris_engine::mcts::Search::new(e, cfg);
            search.start();
            let mut reused = 0u32;
            if let Some(k) = kept {
                if k.tick + 1 == s.tick {
                    search.start_from(k.tree);
                    match search.reroot(&k.root, me, &k.ours, &s) {
                        Some(sub) => {
                            reused = sub.visits();
                            search.start_from(sub);
                        }
                        None => search.start(),
                    }
                }
            }
            let mut done = 0u32;
            loop {
                search.iterate(&s, [None, None], 1000);
                done += 1000;
                if started.elapsed().as_millis() as u64 >= deadline {
                    break;
                }
            }
            let root = search.result();
            dump_visits(e, &s, me, &root.acts[me]);
            let Some((turn, _, q)) = root.acts[me].iter().max_by_key(|a| a.1).copied() else { return idle };
            LAST_ROOT.with(|l| *l.borrow_mut() = Some(RootSeen { root: s, acts: root.acts, value: root.value, sims: done, reused }));
            let tree = search.into_tree();
            KEPT.with(|k| {
                let mut k = k.borrow_mut();
                if k.len() > 16 {
                    k.clear();
                }
                k.insert(key, Kept { tick: s.tick, root: s, ours: turn, tree });
            });
            return Decision { turn, sims: done + reused, score: q, note: "tree" };
        }
        let run = |cfg: victris_engine::mcts::Config, thread: u32| {
            let mut solved: Option<RawTurn> = None;
            let mut out: Option<victris_engine::mcts::RootResult> = None;
            let mut done = 0u32;
            let begun = started.elapsed().as_millis() as u64;
            for j in 0..per_thread {
                let root = starts[(thread + j * pb.tree_threads.max(1)) as usize];
                // this guess's share of the time left
                let until = begun + (deadline.saturating_sub(begun)) * (j as u64 + 1) / per_thread as u64;
                let mut search = victris_engine::mcts::Search::new(e, cfg);
                search.start();
                loop {
                    search.iterate(&root, [None, None], 1000);
                    done += 1000;
                    if started.elapsed().as_millis() as u64 >= until {
                        break;
                    }
                }
                let r = search.result();
                if pb.tree_solve > 0 && per_thread == 1 && pb.tree_threads <= 1 {
                    solved = search.root_solve(me, pb.tree_solve as usize, 2000);
                }
                match out.as_mut() {
                    None => out = Some(r),
                    Some(o) => pool(o, &r),
                }
            }
            (out.expect("at least one search"), done, solved)
        };
        let (root, done, solved) = if pb.tree_threads <= 1 {
            run(cfg, 0)
        } else {
            // Several searches side by side, each with its own jitter on the root priors
            // (and out of sight its own guesses); visits are added turn by turn.
            let results: Vec<_> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..pb.tree_threads)
                    .map(|k| {
                        let mut c = cfg;
                        c.noise_seed = if k == 0 { 0 } else { 0x9e3779b97f4a7c15u64.wrapping_mul(k as u64 + 1) ^ (s.tick as u64) };
                        let run = &run;
                        scope.spawn(move || run(c, k))
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            let mut it = results.into_iter();
            let Some((mut root, mut done, _)) = it.next() else { return idle };
            for (r, d, _) in it {
                done += d;
                pool(&mut root, &r);
            }
            (root, done, None)
        };
        dump_visits(e, &s, me, &root.acts[me]);
        let mut best = root.acts[me].iter().max_by_key(|a| a.1).copied();
        if let Some(turn) = solved {
            if let Some(a) = root.acts[me].iter().find(|a| a.0 == turn) {
                best = Some(*a);
            }
        }
        LAST_ROOT.with(|l| *l.borrow_mut() = Some(RootSeen { root: s, acts: root.acts, value: root.value, sims: done, reused: 0 }));
        return match best {
            Some((turn, _, q)) => Decision { turn, sims: done, score: q, note: "tree" },
            None => idle,
        };
    }
    let dist_now = chebyshev(my.pos, en.pos);

    // ---- our options: a movement (or none), then an attack (or none)
    let mut moves: Vec<(RawTurn, P, u8)> = vec![(RawTurn::IDLE, my.pos, 0)]; // (turn, tile we expect to end on, 0 stay / 1 step / 2 dash)
    for (d, tile) in &view.moves {
        moves.push((RawTurn::step(*d), *tile, 1));
    }
    for d in &view.dashes {
        moves.push((RawTurn::dash(d.dir), d.tile, 2));
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Atk {
        None,
        Ranged,
        Frost,
        Repulsor,
    }
    let mut attacks = vec![Atk::None];
    if visible {
        if k.ranged.map(|a| my.cd_attack[a as usize] == 0).unwrap_or(false) {
            attacks.push(Atk::Ranged);
        }
        if k.repulsor.map(|a| my.cd_attack[a as usize] == 0).unwrap_or(false) && dist_now <= if pb.dash_slam { 5 } else { 2 } {
            attacks.push(Atk::Repulsor);
        }
        if let Some((a, range)) = k.frost {
            if my.cd_attack[a as usize] == 0 && dist_now <= range + 1 && dist_now >= 2 {
                attacks.push(Atk::Frost);
            }
        }
    }

    // ---- the opponent's possible movements this tick, by what we believe it has ready
    let mut replies: Vec<RawTurn> = vec![RawTurn::IDLE];
    if visible && en.state == FState::Idle {
        for d in 0..8u8 {
            if en.cd_move == 0 && !e.map.step_blocked(en.pos, d) {
                replies.push(RawTurn::step(d));
            }
            if en.cd_dash == 0 {
                replies.push(RawTurn::dash(d));
            }
        }
    }
    let en_ranged = visible && en.state == FState::Idle && k.ranged.map(|a| en.cd_attack[a as usize] == 0).unwrap_or(false);
    let en_repulsor = visible && en.state == FState::Idle && k.repulsor.map(|a| en.cd_attack[a as usize] == 0).unwrap_or(false);

    // ---- where we want to be
    let centre = e.map.center;
    let zone_now = s.zone_radius;
    let outside = |p: P, r: i32| chebyshev(p, centre) > r;
    let mut goal: Option<P> = None;
    let mut goal_note = "hold";
    if outside(my.pos, zone_now) || view.next_shrink.map(|(r, _)| outside(my.pos, r)).unwrap_or(false) {
        goal = Some(centre);
        goal_note = "zone";
    }
    if goal.is_none() {
        let missing = e.rules.max_hp - my.hp;
        let mut best: Option<(i32, P, &'static str)> = None;
        for (i, def) in e.map.pads.iter().enumerate() {
            let pad = s.pads[i];
            let d = chebyshev(my.pos, def.pos);
            // A step takes two ticks, so a pad d tiles away is 2d ticks off; with `pad_race`
            // we leave for a power pad in time to be on it when it appears.
            let racing = pb.pad_race && def.kind == PadKind::Power;
            let lead = if racing { 2 * d + 4 } else { 3 };
            let soon = pad.available || pad.respawn_tick - s.tick <= lead;
            let wanted = match def.kind {
                PadKind::Health => missing >= pb.health_pad_missing_hp,
                PadKind::Power => my.power_until == NO_BUFF,
            };
            let reach = if def.kind == PadKind::Health { 10 } else if racing { 9 } else { 7 };
            // Do not race the opponent to a pad it is clearly closer to. With `pad_bonus`
            // a power pad is only given up when the opponent is all but standing on it.
            let their_d = chebyshev(en.pos, def.pos);
            let contested = if pb.pad_bonus > 0.0 && def.kind == PadKind::Power {
                visible && their_d + 1 < d && their_d <= 1
            } else {
                visible && their_d + 1 < d
            };
            // Waiting on a pad that the next shrink leaves outside the zone costs more than it gives.
            let closing = racing && view.next_shrink.map(|(r, _)| outside(def.pos, r)).unwrap_or(false);
            if soon && wanted && d <= reach && !contested && !closing && !outside(def.pos, zone_now) && best.map(|(bd, _, _)| d < bd).unwrap_or(true) {
                best = Some((d, def.pos, if def.kind == PadKind::Health { "health pad" } else { "power pad" }));
            }
        }
        if let Some((_, p, note)) = best {
            goal = Some(p);
            goal_note = note;
        }
    }
    if goal.is_none() && !visible {
        // Head for where the opponent may be: the middle of the tiles it could be on.
        let (w, mut sx, mut sy, mut n) = (e.map.w, 0i64, 0i64, 0i64);
        for (i, b) in t.possible.iter().enumerate() {
            if *b {
                sx += (i as i32 % w) as i64;
                sy += (i as i32 / w) as i64;
                n += 1;
            }
        }
        let mid = if n > 0 { [(sx / n) as i32, (sy / n) as i32] } else { en.pos };
        let mut target = t.enemy_last_seen.map(|(_, p)| p).unwrap_or(mid);
        if n > 0 && !t.possible[(target[1] * w + target[0]) as usize] {
            target = mid;
        }
        if e.map.is_wall(target) || outside(target, zone_now) {
            target = centre;
        }
        goal = Some(target);
        goal_note = "seek";
    }
    let goal_dist = goal.map(|g| distances(e, g));
    let walk = |p: P| -> f64 {
        match &goal_dist {
            Some(d) => {
                let x = d[(p[1] * e.map.w + p[0]) as usize];
                if x == i32::MAX {
                    60.0
                } else {
                    x as f64
                }
            }
            None => 0.0,
        }
    };

    // ---- score every turn
    let mut sims = 0u32;
    let mut best: Option<(f64, RawTurn, &'static str)> = None;
    // A little seeded noise breaks ties differently each tick, so we do not pace in a loop.
    let mut noise_state = (t.seed as u64 ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(s.tick as u64 + 1) | 1;
    let mut noise = || {
        noise_state ^= noise_state << 13;
        noise_state ^= noise_state >> 7;
        noise_state ^= noise_state << 17;
        (noise_state % 1000) as f64 / 1000.0 * 0.05
    };
    let turns = |mine: RawTurn, theirs: RawTurn| if me == 0 { [mine, theirs] } else { [theirs, mine] };
    // With the search on and the opponent in view, turns are collected here and chosen
    // together as a matrix game: (turn, its own merits apart from the exchange, label).
    let matrix = pb.search && visible;
    let mut cands: Vec<(RawTurn, f64, &'static str)> = Vec::new();

    // The learned movement prior: the log probability that a strong bot makes this kind
    // of move here. It is not split among the moves of a kind: the search picks the
    // direction, the prior only weighs the kind (split, it favoured holding, the one
    // kind with a single move).
    let learned = if visible && (pb.move_prior_weight > 0.0 || pb.attack_prior_weight > 0.0) { Some(movemodel::feats(e, &s, me)) } else { None };
    let move_prior: Option<[f32; 5]> = match &learned {
        Some(f) if pb.move_prior_weight > 0.0 && my.cd_move == 0 => Some(movemodel::probs(f)),
        _ => None,
    };
    // The learned attack prior, as a bonus (or a price) on each kind of attack.
    let attack_prior = |which: usize| -> f64 {
        match &learned {
            Some(f) if pb.attack_prior_weight > 0.0 => pb.attack_prior_weight * movemodel::attack_log_odds(f, which),
            _ => 0.0,
        }
    };
    for (mv, dest, kind) in &moves {
        // Static terms for the tile we would end on.
        let mut place = 0.0;
        let can_step_next = *kind == 0 && my.cd_move <= 1;
        // What is already in the air: after this movement, how much of it can we still get
        // out of the way of? Play the next tick every way we could, then let it all land.
        {
            let mut after = s;
            e.step(&mut after, &turns(*mv, RawTurn::IDLE));
            sims += 1;
            let mut least = i32::MAX;
            if after.result.is_none() {
                // Escape plans: stand still, or keep walking one way whenever a step is
                // ready. Seven ticks covers a frostburst's whole fuse, slow included.
                for plan in 0..9u8 {
                    let mut f = after;
                    let mut lost = 0;
                    for _ in 0..7 {
                        if f.result.is_some() {
                            break;
                        }
                        let fm = &f.fighters[me];
                        let go = plan < 8 && fm.state == FState::Idle && fm.cd_move == 0 && !e.map.step_blocked(fm.pos, plan);
                        let t = if go { RawTurn::step(plan) } else { RawTurn::IDLE };
                        lost += e.step(&mut f, &turns(t, RawTurn::IDLE)).hp_lost[me];
                        sims += 1;
                    }
                    if f.result.is_some() && !f.fighters[me].alive {
                        lost += 1000;
                    }
                    least = least.min(lost);
                    if least == 0 {
                        break;
                    }
                }
            } else {
                least = 0;
            }
            place -= pb.incoming_weight * least as f64;
        }
        if outside(*dest, zone_now) {
            place -= 12.0;
        }
        if let Some((r, in_ticks)) = view.next_shrink {
            if outside(*dest, r) {
                place -= 6.0 * (1.0 - in_ticks as f64 / 25.0).max(0.2);
            }
        }
        place -= pb.goal_weight * walk(*dest);
        if pb.pad_bonus > 0.0 && my.power_until == NO_BUFF {
            if goal_note == "power pad" {
                place -= pb.pad_bonus / 8.0 * walk(*dest);
            }
            let on_pad = e.map.pads.iter().enumerate().any(|(i, def)| {
                def.kind == PadKind::Power && def.pos == *dest && (s.pads[i].available || s.pads[i].respawn_tick - s.tick <= 1)
            });
            if on_pad {
                place += pb.pad_bonus;
            }
        }
        if let Some(p) = &move_prior {
            let c = movemodel::class(*kind, my.pos, *dest, en.pos);
            place += pb.move_prior_weight * (p[c].max(1e-3) as f64).ln();
        }
        if *kind == 2 {
            place -= if frosted && pb.dash_cost_frosted >= 0.0 { pb.dash_cost_frosted } else { pb.dash_cost };
        }
        if visible {
            let d = chebyshev(*dest, en.pos);
            if goal.is_none() {
                place -= pb.distance_weight * (d as f64 - pb.preferred_distance).abs().min(6.0);
            }
            // Inside four tiles a shot arrives before a spent step comes back, so while the
            // opponent's shot is ready the step is our dodge: spend it only for a reason.
            if *kind == 1 && d <= 4 {
                let shot_ready = k.ranged.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false);
                place -= if shot_ready { pb.step_cost_under_fire } else { pb.step_cost_near };
            }
            // Next to the opponent with a wall behind us is where a repulsor hurts most.
            if d <= 1 && k.repulsor.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false) {
                let mut p = *dest;
                let mut slam = false;
                if let Some(dir) = octant(en.pos, *dest) {
                    for _ in 0..3 {
                        if e.map.step_blocked(p, dir) {
                            slam = true;
                            break;
                        }
                        p = add(p, DIR_VEC[dir as usize]);
                    }
                }
                place -= if slam { 8.0 } else { 1.5 };
            }
            // A step now leaves us unable to move next tick. If the opponent's shot is ready
            // and it can be within two tiles of us by then, that is a free hit for it.
            {
                let stuck_next = *kind != 0 || my.cd_move > 1;
                let shot_ready = k.ranged.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false);
                if stuck_next && shot_ready && d <= pb.stuck_range {
                    place -= pb.stuck_penalty;
                }
            }
            // Their shot is spent for a few ticks and ours is not: this is when to close in,
            // so that we are within two tiles when they next cannot step.
            if pb.press_bonus > 0.0 {
                let their_shot_away = k.ranged.map(|a| en.cd_attack[a as usize] >= 3).unwrap_or(false);
                let ours_soon = k.ranged.map(|a| my.cd_attack[a as usize] <= 2).unwrap_or(false);
                if their_shot_away && ours_soon && d >= 2 && d < dist_now {
                    place += pb.press_bonus * (dist_now - d) as f64;
                }
            }
            // Within two tiles a shot lands the tick it is fired: be there only with the tempo.
            if d <= 2 && k.ranged.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false) {
                place -= if can_step_next { 2.0 } else { 6.0 };
            }
            let en_stuck = en.cd_move >= 2 && en.cd_dash >= 2;
            if d <= 2 && en_stuck && k.ranged.map(|a| my.cd_attack[a as usize] <= 1).unwrap_or(false) {
                place += 5.0;
            }
        }

        // The ladder's staple: a step or a dash to our side and a repulsor in the same tick,
        // shoving us into a wall. If any movement the opponent has would do that to us on
        // this tile, the tile is exposed.
        if visible && pb.slam_risk > 0.0 && en_repulsor && *kind != 2 {
            let burst = k.repulsor.unwrap();
            for r in &replies {
                let mut after = s;
                let sum = e.step(&mut after, &turns(*mv, r.with_burst(burst)));
                sims += 1;
                if sum.slammed[me] {
                    place -= pb.slam_risk;
                    break;
                }
            }
        }

        for atk in &attacks {
            if *atk == Atk::Repulsor && chebyshev(*dest, en.pos) > 2 {
                continue; // a repulsor only reaches the tiles around us
            }
            if *atk == Atk::Frost {
                // An area is judged from the tile we end on: in range and in sight from there.
                let (_, range) = k.frost.unwrap();
                if chebyshev(*dest, en.pos) > range || !can_see(&e.rules, &e.map, *dest, en.pos) {
                    continue;
                }
            }
            let (mine, cost, bonus, note) = match atk {
                Atk::None => (*mv, 0.0, 0.0, goal_note),
                Atk::Ranged => (mv.with_attack(k.ranged.unwrap(), en.pos), pb.fire_cost, attack_prior(0), "sure shot"),
                Atk::Repulsor => (mv.with_burst(k.repulsor.unwrap()), pb.repulsor_cost, attack_prior(2), "repulsor"),
                Atk::Frost => {
                    let a = k.frost.unwrap().0;
                    let fuse = match e.rules.attacks[a as usize].kind {
                        victris_engine::rules::AttackKind::Area { fuse, .. } => fuse,
                        _ => 0,
                    };
                    // The bonus is for a burst thrown from where we stand or step. Paid on a
                    // dash as well, it had us dash after every opponent that dashed away
                    // (20 dashes a game in self-play against 7).
                    let dasher = pb.frost_hold_grace <= 0 || t.enemy_dashes > 0 || view.tick < pb.frost_hold_grace;
                    let timing = if en.cd_dash <= fuse {
                        if dasher { -pb.frost_wait_cost } else { 0.0 }
                    } else if *kind == 2 {
                        0.0
                    } else {
                        pb.frost_combo
                    };
                    (mv.with_attack(a, en.pos), pb.frost_cost, pb.frost_value + timing + attack_prior(1), "frostburst")
                }
            };
            if matrix {
                cands.push((mine, place - cost + bonus, note));
                // Standing still, a shot can also lead the opponent: aim where it could step.
                if pb.lead_shots && *atk == Atk::Ranged && *kind == 0 && en.state == FState::Idle && en.cd_move == 0 {
                    for d in 0..8u8 {
                        if !e.map.step_blocked(en.pos, d) {
                            let aim = add(en.pos, DIR_VEC[d as usize]);
                            if aim != *dest {
                                cands.push((mv.with_attack(k.ranged.unwrap(), aim), place - cost, "lead shot"));
                            }
                        }
                    }
                }
                continue;
            }
            // Worst and average over the opponent's movements, with it not attacking.
            let (mut worst, mut sum) = (f64::MAX, 0.0);
            for r in &replies {
                let v = outcome(e, &s, me, &turns(mine, *r), pb, &mut sims);
                worst = worst.min(v);
                sum += v;
            }
            let avg = sum / replies.len() as f64;
            // What it could do to us this tick if it guessed the tile we end on.
            let mut threat: f64 = 0.0;
            if *kind != 2 {
                for r in &replies {
                    if en_ranged {
                        let v = outcome(e, &s, me, &turns(mine, r.with_attack(k.ranged.unwrap(), *dest)), pb, &mut sims);
                        threat = threat.min(v - worst);
                    }
                    if en_repulsor && chebyshev(*dest, en.pos) <= 2 {
                        let v = outcome(e, &s, me, &turns(mine, r.with_burst(k.repulsor.unwrap())), pb, &mut sims);
                        threat = threat.min(v - worst);
                    }
                }
            }
            let score = worst + pb.average_weight * (avg - worst) + pb.threat_weight * threat + place - cost + bonus + noise();
            if best.map(|(b, _, _)| score > b).unwrap_or(true) {
                best = Some((score, mine, note));
            }
        }
    }
    if matrix && !cands.is_empty() {
        // The opponent's options: every movement we believe it has, alone or with an
        // attack; and, standing still, a shot at each tile we could end on.
        let mut theirs: Vec<(RawTurn, f64)> = Vec::new(); // (turn, what it costs them)
        let en_frost = k.frost.filter(|(a, range)| en.state == FState::Idle && en.cd_attack[*a as usize] == 0 && dist_now <= *range);
        for r in &replies {
            let moved = if matches!(r.dash, victris_engine::turn::DirIn::Dir(_)) { pb.their_dash_cost } else { 0.0 };
            theirs.push((*r, moved));
            if en_ranged && (pb.model_attacks || pb.model_shots) {
                theirs.push((r.with_attack(k.ranged.unwrap(), my.pos), moved + pb.fire_cost));
            }
            if en_repulsor && (pb.model_attacks || pb.model_repulsor) && dist_now <= 3 {
                theirs.push((r.with_burst(k.repulsor.unwrap()), moved + pb.repulsor_cost));
            }
        }
        if en_ranged && pb.model_attacks {
            for (_, dest, kind) in &moves {
                if *kind == 1 {
                    theirs.push((RawTurn::IDLE.with_attack(k.ranged.unwrap(), *dest), pb.fire_cost));
                }
            }
        }
        if let Some((a, _)) = en_frost.filter(|_| pb.model_attacks) {
            theirs.push((RawTurn::IDLE.with_attack(a, my.pos), pb.frost_cost - pb.frost_value));
        }
        let (rows, cols) = (cands.len(), theirs.len());
        let mut payoff = vec![0.0f64; rows * cols];
        for (m, (mine, merit, _)) in cands.iter().enumerate() {
            for (c, (reply, their_cost)) in theirs.iter().enumerate() {
                payoff[m * cols + c] = cell_value(e, &s, me, &turns(*mine, *reply), pb, &mut sims) + merit + their_cost;
            }
        }
        let (mut mix, mut their_mix, mut value) = solve_matrix(&payoff, rows, cols, 300);
        let mut final_rows: Vec<usize> = (0..rows).collect();
        let mut final_payoff: Vec<f64> = payoff.clone();
        // Deepening: the turns the first solve likes best are played out one tick further,
        // with the next tick itself solved as a game, and the choice is made among them.
        if pb.deepen > 0 && dist_now <= pb.deep_range && rows > 1 {
            let mut order: Vec<usize> = (0..rows).collect();
            let row_mean = |m: usize| payoff[m * cols..(m + 1) * cols].iter().sum::<f64>() / cols as f64;
            order.sort_by(|a, b| (mix[*b], row_mean(*b)).partial_cmp(&(mix[*a], row_mean(*a))).unwrap());
            order.truncate(pb.deepen);
            // One of our turns played a tick further against every reply, the replies
            // shared out over the search threads. `depth` counts ticks after the first.
            // The hard stop is checked before every cell: past it, a cell keeps the value the
            // one-tick solve gave it. A busy machine costs lookahead, never a missed tick.
            let hard_stop = started + std::time::Duration::from_millis(pb.hard_ms.max(1));
            let payoff = &payoff;
            let deepen_row = |m: usize, depth: u32, deadline: Option<Instant>, out: &mut [f64]| -> u32 {
                let (mine, merit, _) = cands[m];
                let threads = pb.threads.clamp(1, 16).min(cols.max(1));
                let chunk = cols.div_ceil(threads);
                let mut total = 0u32;
                std::thread::scope(|scope| {
                    let handles: Vec<_> = out
                        .chunks_mut(chunk)
                        .enumerate()
                        .map(|(i, part)| {
                            let theirs = &theirs;
                            let k = &k;
                            scope.spawn(move || {
                                let mut n = 0u32;
                                for (j, slot) in part.iter_mut().enumerate() {
                                    if Instant::now() >= hard_stop {
                                        *slot = payoff[m * cols + i * chunk + j];
                                        continue;
                                    }
                                    let (reply, their_cost) = theirs[i * chunk + j];
                                    let (v1, s1) = tick_value(e, &s, me, &turns(mine, reply), pb, 0, false, &mut n);
                                    let later = if s1.result.is_none() { subgame_value(e, &s1, me, pb, k, depth, deadline, &mut n) } else { 0.0 };
                                    *slot = v1 + later + merit + their_cost;
                                }
                                n
                            })
                        })
                        .collect();
                    for h in handles {
                        total += h.join().unwrap_or(0);
                    }
                });
                total
            };
            let mut deep = vec![0.0f64; order.len() * cols];
            // Rows not started before the hard stop are dropped from the choice.
            let mut reached = 0;
            for (i, &m) in order.iter().enumerate() {
                if i > 0 && Instant::now() >= hard_stop {
                    break;
                }
                sims += deepen_row(m, 1, None, &mut deep[i * cols..(i + 1) * cols]);
                reached = i + 1;
            }
            order.truncate(reached);
            deep.truncate(reached * cols);
            let (mut deep_mix, mut deep_their, mut deep_value) = solve_matrix(&deep, order.len(), cols, 300);
            // With time left, the turns still in contention get a third tick, best first,
            // until the clock runs out. Rows are replaced whole, so a row is never half deep.
            if pb.depth >= 3 {
                let deadline = if pb.think_ms > 0 { Some(started + std::time::Duration::from_millis(t.think_cap_ms(pb.think_ms))) } else { None };
                let mut by_weight: Vec<usize> = (0..order.len()).collect();
                by_weight.sort_by(|a, b| deep_mix[*b].partial_cmp(&deep_mix[*a]).unwrap());
                let mut row = vec![0.0f64; cols];
                let mut done = 0;
                for &i in by_weight.iter().take(pb.deep3_rows) {
                    if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                        break;
                    }
                    sims += deepen_row(order[i], pb.depth - 1, deadline, &mut row);
                    if deadline.map(|d| Instant::now() >= d).unwrap_or(false) {
                        break; // a row cut short by the clock is not trusted
                    }
                    deep[i * cols..(i + 1) * cols].copy_from_slice(&row);
                    done += 1;
                }
                if done > 0 {
                    let solved = solve_matrix(&deep, order.len(), cols, 300);
                    deep_mix = solved.0;
                    deep_their = solved.1;
                    deep_value = solved.2;
                }
            }
            mix = vec![0.0; rows];
            for (i, &m) in order.iter().enumerate() {
                mix[m] = deep_mix[i];
            }
            value = deep_value;
            their_mix = deep_their;
            final_rows = order.clone();
            final_payoff = deep.clone();
        }
        // Real opponents are not perfect dodgers. With `prior_weight` on, each of our turns
        // is valued partly against the solved opponent and partly against a plain one that
        // stands still half the time and otherwise steps at random, and the best is played.
        let mut prior_pick: Option<usize> = None;
        if pb.prior_weight > 0.0 && !final_rows.is_empty() {
            let plain = |t: &RawTurn| t.attack == victris_engine::turn::AttackIn::None && t.dash == victris_engine::turn::DirIn::None;
            let steps = theirs.iter().filter(|(t, _)| plain(t) && t.mv != victris_engine::turn::DirIn::None).count();
            let prior: Vec<f64> = if pb.prior_model {
                let columns: Vec<RawTurn> = theirs.iter().map(|(t, _)| *t).collect();
                oppmodel::prior(e, &columns, &en, &my, &k)
            } else {
                theirs
                    .iter()
                    .map(|(t, _)| {
                        if !plain(t) {
                            0.0
                        } else if t.mv == victris_engine::turn::DirIn::None {
                            if steps == 0 { 1.0 } else { pb.prior_hold }
                        } else {
                            (1.0 - pb.prior_hold) / steps as f64
                        }
                    })
                    .collect()
            };
            let mut best = f64::MIN;
            for (i, &m) in final_rows.iter().enumerate() {
                let row = &final_payoff[i * cols..(i + 1) * cols];
                let solved: f64 = row.iter().zip(&their_mix).map(|(v, p)| v * p).sum();
                let naive: f64 = row.iter().zip(&prior).map(|(v, p)| v * p).sum();
                let blended = (1.0 - pb.prior_weight) * solved + pb.prior_weight * naive;
                if blended > best {
                    best = blended;
                    prior_pick = Some(m);
                }
            }
            value = best;
        }
        // Play from the mixed strategy, ignoring the tail regret matching leaves behind.
        let top = mix.iter().cloned().fold(0.0, f64::max);
        mix.iter_mut().for_each(|p| {
            if *p < 0.05_f64.min(top) {
                *p = 0.0
            }
        });
        let total: f64 = mix.iter().sum();
        let mut roll = noise() / 0.05 * total;
        let mut pick = rows - 1;
        if pb.sample {
            for (m, p) in mix.iter().enumerate() {
                if roll < *p {
                    pick = m;
                    break;
                }
                roll -= p;
            }
        } else {
            pick = (0..rows).max_by(|a, b| mix[*a].partial_cmp(&mix[*b]).unwrap()).unwrap_or(0);
        }
        if let Some(m) = prior_pick {
            pick = m;
        }
        // The plan search chooses among the turns the game solve rates within
        // `plan_margin` hp of its best: safe by the solve, then best by the plan.
        if pb.plan_depth >= 2 && !final_rows.is_empty() && started.elapsed().as_millis() < pb.hard_ms as u128 {
            let solved_value = |i: usize| -> f64 {
                final_payoff[i * cols..(i + 1) * cols].iter().zip(&their_mix).map(|(v, p)| v * p).sum()
            };
            let values: Vec<f64> = (0..final_rows.len()).map(solved_value).collect();
            let top = values.iter().cloned().fold(f64::MIN, f64::max);
            let mut best_plan = f64::MIN;
            for (i, &m) in final_rows.iter().enumerate() {
                if values[i] < top - pb.plan_margin {
                    continue;
                }
                let (mine, merit, _) = cands[m];
                let v = plan_value(e, &s, me, mine, pb, &k, &mut sims) + merit + pb.plan_solve_weight * values[i];
                if v > best_plan {
                    best_plan = v;
                    pick = m;
                }
            }
        }
        return Decision { turn: cands[pick].0, sims, score: value, note: cands[pick].2 };
    }
    match best {
        Some((score, turn, note)) => Decision { turn, sims, score, note },
        None => idle,
    }
}

