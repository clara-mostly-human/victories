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
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bots: Vec<BotConfig>,
    /// the load guard pauses batch work above this 1-minute load average
    pub guard_load: f64,
    /// ... or when a live bot's think time (99th percentile over the last 90 s) is above this
    pub guard_think_p99_ms: f64,
    /// the strong bots each new build is compared with by the habits yardstick
    pub habits_bots: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { bots: Vec::new(), guard_load: 8.0, guard_think_p99_ms: 300.0, habits_bots: vec!["StuartSutcliffe".into(), "Shelbot4".into()] }
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
                });
            }
        }
        let d = Config::default();
        let guard = |k: &str, dflt: f64| v.get("guard").and_then(|g| g.get(k)).and_then(Value::as_f64).unwrap_or(dflt);
        let habits_bots = v.get("habits_bots").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or(d.habits_bots);
        Config { bots, guard_load: guard("load", d.guard_load), guard_think_p99_ms: guard("think_p99_ms", d.guard_think_p99_ms), habits_bots }
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
