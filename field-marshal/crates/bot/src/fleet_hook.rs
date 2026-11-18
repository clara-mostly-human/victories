//! The bridge from the client's strategy hook to the fleet's shared store.
//!
//! At `hello` the match is written to the store and, when the opponent is the target of a
//! running experiment and this bot takes part, the next arm of that experiment is returned
//! as the playbook settings to play. At `end` the result is written back. A failure here
//! (store locked, disk full, anything) never reaches the match: the bot plays the playbook
//! from its file and says so in its log.

use crate::session::{MatchInfo, Strategist};
use fleet::store::{now_ms, MatchStart, Store};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub struct FleetHook {
    pub db: PathBuf,
    pub bot: String,
    pub version: String,
    pub release: Option<String>,
    /// take arms from the experiment plan (false: only report results)
    pub experiments: bool,
    /// names of our own bots; games against them are marked and never used for experiments
    pub ours: Vec<String>,
    store: Option<Store>,
}

impl FleetHook {
    pub fn new(db: PathBuf, bot: &str, version: &str, release: Option<String>, experiments: bool, ours: Vec<String>) -> FleetHook {
        FleetHook { db, bot: bot.to_string(), version: version.to_string(), release, experiments, ours, store: None }
    }

    /// Opens the store on first use and again after any failure. Waits are short: `hello`
    /// must be answered within the server's startup limit.
    fn store(&mut self) -> Option<&mut Store> {
        if self.store.is_none() {
            match Store::open(&self.db, Duration::from_millis(1500)) {
                Ok(s) => self.store = Some(s),
                Err(e) => eprintln!("[{}] fleet store unavailable: {e}", self.bot),
            }
        }
        self.store.as_mut()
    }

    fn start(&self, m: &MatchInfo) -> MatchStart {
        MatchStart {
            match_id: m.match_id.clone(),
            bot: self.bot.clone(),
            version: self.version.clone(),
            release: self.release.clone(),
            opponent: m.opponent.clone(),
            map: m.map.clone(),
            seat: m.seat,
            seed: m.seed,
        }
    }
}

impl Strategist for FleetHook {
    fn assign(&mut self, m: &MatchInfo, base: &str) -> Option<(String, Value)> {
        let start = self.start(m);
        let ours = self.ours.iter().any(|o| *o == m.opponent);
        let experiments = self.experiments;
        let bot = self.bot.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let store = self.store()?;
            match store.assign(&start, base, experiments, ours) {
                Ok(a) => a.map(|a| (a.playbook_name(), a.settings)),
                Err(e) => {
                    eprintln!("[{bot}] fleet assign failed: {e}");
                    None
                }
            }
        }));
        match result {
            Ok(Some(x)) => Some(x),
            Ok(None) => None,
            Err(_) => {
                self.store = None;
                None
            }
        }
    }

    fn finish(&mut self, _m: &MatchInfo, line: &Value) {
        let bot = self.bot.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Some(store) = self.store() else { return false };
            match store.match_finished(&bot, line, now_ms()) {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("[{bot}] fleet result not stored: {e}");
                    false
                }
            }
        }));
        if !matches!(result, Ok(true)) {
            self.store = None; // open afresh next time; `fleet ingest` recovers the row from the log
        }
    }
}
