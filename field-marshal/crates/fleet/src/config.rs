//! Per-bot configuration, read from `<repo>/fleet/bots.json` (tracked in git, no secrets).
//!
//! ```json
//! { "bots": { "fleet-canary": { "role": "canary", "managed": true, "experiments": true } } }
//! ```
//!
//! `managed` bots are run by `fleet run` under launchd. `experiments` lets a bot take arms
//! from the experiment plan. `expect_online` puts a bot under the offline watchdog. An
//! optional top-level `"guard": { "load": 8, "think_p99_ms": 300 }` sets when the load guard
//! pauses batch work. Every name listed here counts as one of ours.

use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct BotConfig {
    pub name: String,
    /// flagship, scout, canary or legacy: only words for people, except that `promote`
    /// deploys to flagships and `canary` to scouts and canaries
    pub role: String,
    pub managed: bool,
    pub experiments: bool,
    /// playbooks file inside the release directory
    pub playbooks: String,
    /// ladder address, for pointing a bot at a local test server
    pub url: Option<String>,
    pub note: Option<String>,
    /// the offline watchdog raises an alert when this bot is off the ladder
    pub expect_online: bool,
    /// who brings it back, named in the alert
    pub owner: Option<String>,
    /// the gateway holds this bot's connection and runs its engine behind it
    pub gateway: bool,
    /// override test (gateway bots): on every Nth tick send an empty turn first, then the
    /// engine's; 0 is off
    pub provisional_test_every: u64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bots: Vec<BotConfig>,
    /// the load guard pauses batch work above this 1-minute load average
    pub guard_load: f64,
    /// ... or when a live bot's think time (99th percentile over the last 90 s) is above this
    pub guard_think_p99_ms: f64,
    /// ... or when the disk holding `data/` (and the swap files) has less free than this, in GB
    pub guard_min_disk_gb: f64,
    /// ... or when more swap than this is in use, in GB
    pub guard_max_swap_used_gb: f64,
    /// ... or when the kernel's memory level (`kern.memorystatus_level`, percent) is under this
    pub guard_min_memory_level: f64,
    /// ... or when a process had to wait longer than this for the CPU in the last 15 s, in ms
    pub guard_max_cpu_wait_ms: f64,
    /// the strong bots each new build is compared with by the habits yardstick
    pub habits_bots: Vec<String>,
    /// pull replays from the repository's `corpus` branch (filled by the GitHub workflow)
    pub corpus_branch: bool,
    /// ladder addresses to try first (`"prefer_addresses": ["104.21.89.84"]`)
    pub prefer_addresses: Vec<String>,
    /// how long a bot that is not the flagship waits after a match before queueing again,
    /// so the flagship gets the freed match slot first
    pub queue_gap_ms: u64,
    /// the gateway's comms budget (`"comms": {"connects_per_minute": 20, ...}`)
    pub comms_connects_per_minute: usize,
    pub comms_queues_per_minute: usize,
    pub comms_matches_per_day: usize,
    /// how often the health pass asks the ladder for its standings (0: never)
    pub ladder_refresh_minutes: u64,
    /// the playbook names the default playbook file plays; games outside an experiment on
    /// one of these count towards an experiment's baseline (`"baseline_playbooks": [...]`)
    pub baseline_playbooks: Vec<String>,
    /// may the autopilot move the scouts and the canary to new builds by itself? (false:
    /// it still builds and gates, but a bot changes release only when a commit is named)
    pub autopilot_moves_bots: bool,
    /// may the watchdog restart a bot of ours that sits without a match? (default false:
    /// it only reports the stall and its cause)
    pub stall_restarts: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { bots: Vec::new(), guard_load: 8.0, guard_think_p99_ms: 300.0, guard_min_disk_gb: 10.0, guard_max_swap_used_gb: 10.0, guard_min_memory_level: 20.0, guard_max_cpu_wait_ms: 100.0, habits_bots: vec!["StuartSutcliffe".into(), "Shelbot4".into()], corpus_branch: false, prefer_addresses: Vec::new(), queue_gap_ms: 1000,
            // The ladder's own quotas are 20,000 matches a day per owner and one match start
            // per bot every 300 ms; these sit well inside them.
            comms_connects_per_minute: 20, comms_queues_per_minute: 90, comms_matches_per_day: 15_000, ladder_refresh_minutes: 0, baseline_playbooks: vec!["matrix-5".into()], autopilot_moves_bots: true, stall_restarts: false }
    }
}

impl Config {
    pub fn load(root: &Path) -> Result<Config, String> {
        let path = root.join("fleet").join("bots.json");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Config::from_json(&v))
    }

    pub fn from_json(v: &Value) -> Config {
        let mut bots = Vec::new();
        if let Some(map) = v.get("bots").and_then(Value::as_object) {
            for (name, b) in map {
                let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
                let flag = |k: &str| b.get(k).and_then(Value::as_bool).unwrap_or(false);
                bots.push(BotConfig {
                    name: name.clone(),
                    role: text("role").unwrap_or_else(|| "scout".into()),
                    managed: flag("managed"),
                    experiments: flag("experiments"),
                    playbooks: text("playbooks").unwrap_or_else(|| "playbooks/default.json".into()),
                    url: text("url"),
                    note: text("note"),
                    expect_online: flag("expect_online"),
                    owner: text("owner"),
                    gateway: flag("gateway"),
                    provisional_test_every: b.get("provisional_test_every").and_then(Value::as_u64).unwrap_or(0),
                });
            }
        }
        let d = Config::default();
        let guard = |k: &str, dflt: f64| v.get("guard").and_then(|g| g.get(k)).and_then(Value::as_f64).unwrap_or(dflt);
        let comms = |k: &str, dflt: usize| v.get("comms").and_then(|c| c.get(k)).and_then(Value::as_u64).map(|n| n as usize).unwrap_or(dflt);
        let habits_bots = v.get("habits_bots").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or(d.habits_bots);
        Config { bots, guard_load: guard("load", d.guard_load), guard_think_p99_ms: guard("think_p99_ms", d.guard_think_p99_ms), guard_min_disk_gb: guard("min_disk_gb", d.guard_min_disk_gb), guard_max_swap_used_gb: guard("max_swap_used_gb", d.guard_max_swap_used_gb), guard_min_memory_level: guard("min_memory_level", d.guard_min_memory_level), guard_max_cpu_wait_ms: guard("max_cpu_wait_ms", d.guard_max_cpu_wait_ms), habits_bots, corpus_branch: v.get("corpus_branch").and_then(Value::as_bool).unwrap_or(false),
            prefer_addresses: v.get("prefer_addresses").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            queue_gap_ms: v.get("queue_gap_ms").and_then(Value::as_u64).unwrap_or(d.queue_gap_ms),
            comms_connects_per_minute: comms("connects_per_minute", d.comms_connects_per_minute),
            comms_queues_per_minute: comms("queues_per_minute", d.comms_queues_per_minute),
            comms_matches_per_day: comms("matches_per_day", d.comms_matches_per_day),
            ladder_refresh_minutes: v.get("ladder_refresh_minutes").and_then(Value::as_u64).unwrap_or(0),
            stall_restarts: v.get("stall_restarts").and_then(Value::as_bool).unwrap_or(false),
            autopilot_moves_bots: v.get("autopilot_moves_bots").and_then(Value::as_bool).unwrap_or(true),
            baseline_playbooks: v.get("baseline_playbooks").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).filter(|l: &Vec<String>| !l.is_empty()).unwrap_or(d.baseline_playbooks),
        }
    }

    pub fn bot(&self, name: &str) -> Option<&BotConfig> {
        self.bots.iter().find(|b| b.name == name)
    }

    pub fn is_ours(&self, name: &str) -> bool {
        self.bot(name).is_some()
    }

    pub fn names(&self) -> Vec<String> {
        self.bots.iter().map(|b| b.name.clone()).collect()
    }
}
