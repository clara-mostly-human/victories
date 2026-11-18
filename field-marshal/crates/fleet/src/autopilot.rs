//! The autopilot: the release pipeline run by itself, up to the point where a person decides.
//!
//! Once a minute it looks at the repository and the store and does the next thing due:
//!
//! 1. A new commit that changes how the bot plays (`BOT_PATHS`) is built into a release.
//! 2. That candidate plays the live release in the arena gate (a child process, low
//!    priority, under the load guard). A candidate with the same bot code as the live
//!    release skips the gate.
//! 3. A candidate that passes goes on the ladder with the canary bot.
//! 4. Its ladder evidence against the live release is worked out every ten minutes.
//! 5. The scouts are kept on a fleet build of the commit the flagship runs, so experiments
//!    always stand on the proven base. They are not moved while an experiment is running.
//!
//! It never promotes and never touches the flagship: "live" is what `fleet release promote`
//! last set, and the flagship's own thread decides when to restart onto it. Everything it does is one line in
//! `logs/fleet-alerts.log` (kind PIPELINE) and an event in the store.

use crate::config::Config;
use crate::online::log_line;
use crate::release::{self, release_commit, same_bot, EvidenceOptions};
use crate::store::{data_dir, default_db, Release, Store};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Autopilot {
    gate: Option<(String, Child)>,
    /// commits whose build failed, so a broken commit is tried once
    failed_builds: HashSet<String>,
    said: HashMap<String, String>,
    last_evidence: Option<Instant>,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Releases the fleet built itself (`r<N>-<sha>`), newest first.
fn fleet_builds(store: &Store) -> Vec<Release> {
    let mut v: Vec<Release> = store.releases().unwrap_or_default().into_iter().filter(|r| r.id.starts_with('r') && r.git_sha.len() == 40).collect();
    v.sort_by_key(|r| -r.seq);
    v
}

impl Autopilot {
    /// Says a thing once per subject, so a standing condition is one line, not one a minute.
    fn say(&mut self, root: &Path, subject: &str, text: String) {
        if self.said.get(subject) != Some(&text) {
            log_line(root, "PIPELINE", &text);
            self.said.insert(subject.to_string(), text);
        }
    }

    fn build(&mut self, root: &Path, store: &Store, sha: &str, note: &str) -> Option<Release> {
        if self.failed_builds.contains(sha) {
            return None;
        }
        match release::build(root, store, sha, Some(note), true) {
            Ok(r) => {
                log_line(root, "PIPELINE", &format!("built {} from {} ({note})", r.id, &sha[..7]));
                Some(r)
            }
            Err(e) => {
                self.failed_builds.insert(sha.to_string());
                let last = e.lines().last().unwrap_or("").to_string();
                log_line(root, "PIPELINE", &format!("build of {} FAILED and will not be retried: {last}", &sha[..7]));
                store.event(None, "build_failed", &json!({ "sha": sha, "error": e }));
                None
            }
        }
    }

    pub fn step(&mut self, root: &Path) -> Result<(), String> {
        let cfg = Config::load(root)?;
        let store = Store::open(&default_db(root), Duration::from_secs(10)).map_err(|e| e.to_string())?;
        let live_id = release::sync_flagship(root, &store, &cfg)?;
        let live = match &live_id {
            Some(id) => store.release(id).map_err(|e| e.to_string())?,
            None => None,
        };
        let live_sha = live.as_ref().and_then(release_commit);

        // 1. The gate child, if one is running.
        if let Some((id, child)) = self.gate.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                let id = id.clone();
                self.gate = None;
                let r = store.release(&id).map_err(|e| e.to_string())?;
                let text = match r.as_ref().and_then(|r| r.gate.clone()) {
                    Some(g) => format!(
                        "arena gate {}: {id} scored {:.0}% over {} games against {} ({}W {}L {}D)",
                        g["verdict"].as_str().unwrap_or("?"),
                        100.0 * g["score"].as_f64().unwrap_or(0.0),
                        g["games"],
                        g["against"].as_str().unwrap_or("?"),
                        g["wins"],
                        g["losses"],
                        g["draws"]
                    ),
                    None => {
                        // No verdict was written: the gate itself broke. Do not loop on it.
                        let _ = store.set_release_json(&id, "gate", &json!({ "verdict": "error", "note": "the gate ended without a result; see data/gate log" }));
                        format!("arena gate of {id} ended without a result; see {}", data_dir(root).join(format!("gate-{id}.log")).display())
                    }
                };
                log_line(root, "PIPELINE", &text);
            }
        }

        // 2. A commit that changes the bot becomes a candidate, once it has sat for two minutes.
        let head = git(root, &["rev-parse", "HEAD"]).ok_or("git rev-parse failed")?;
        let builds = fleet_builds(&store);
        let changed = match builds.first() {
            None => true,
            Some(newest) => same_bot(root, &newest.git_sha, &head) == Some(false),
        };
        if changed {
            let age = git(root, &["log", "-1", "--format=%ct", &head]).and_then(|t| t.parse::<i64>().ok()).map(|t| crate::store::now_ms() / 1000 - t).unwrap_or(0);
            if age >= 120 {
                self.build(root, &store, &head, "a new commit that changes the bot");
            }
        }

        // 3. The scouts' base: a fleet build of the commit the flagship runs.
        if let Some(sha) = &live_sha {
            let builds = fleet_builds(&store);
            let mirror = match builds.iter().find(|r| same_bot(root, sha, &r.git_sha) == Some(true)) {
                Some(r) => Some(r.clone()),
                None => match git(root, &["rev-parse", "--verify", &format!("{sha}^{{commit}}")]) {
                    Some(full) => self.build(root, &store, &full, "the commit the flagship runs, for the scouts"),
                    None => None,
                },
            };
            if let Some(mirror) = mirror {
                let running = !store.experiments(true).map_err(|e| e.to_string())?.is_empty();
                for b in cfg.bots.iter().filter(|b| b.managed && b.role == "scout") {
                    let current = store.bot_state(&b.name).map_err(|e| e.to_string())?.and_then(|s| s.release);
                    if current.as_deref() == Some(&mirror.id) {
                        continue;
                    }
                    if running && current.is_some() {
                        self.say(root, &format!("scout-{}", b.name), format!("{} stays on {} until the running experiment ends; the flagship is on {}", b.name, current.unwrap_or_default(), mirror.id));
                        continue;
                    }
                    let why = format!("same bot code as the live release {}", live_id.clone().unwrap_or_default());
                    match release::deploy(root, &store, &cfg, &b.name, &mirror.id, None, Some(&why)) {
                        Ok(v) => log_line(root, "PIPELINE", &format!("scout {} now runs {} as {v} ({why})", b.name, mirror.id)),
                        Err(e) => self.say(root, &format!("scout-{}", b.name), format!("cannot deploy {} to {}: {e}", mirror.id, b.name)),
                    }
                }
            }
        }

        // 4. The newest candidate meets the live release in the arena.
        let builds = fleet_builds(&store);
        if let Some(cand) = builds.first().filter(|c| c.status == "built" && c.gate.is_none()) {
            match (&live, &live_sha) {
                (None, _) => self.say(root, "gate", format!("{} is waiting: no live release to gate against", cand.id)),
                (Some(l), Some(sha)) if release::same_search(root, sha, &cand.git_sha) == Some(true) => {
                    // Same engine, tracker, policy and playbooks: the arena has no network and
                    // no ladder, so it could not tell the two apart.
                    let _ = store.set_release_json(&cand.id, "gate", &json!({ "verdict": "same", "against": l.id, "note": "same search as the live release; no gate needed" }));
                    let _ = store.set_release_status(&cand.id, "gated");
                    log_line(root, "PIPELINE", &format!("{} has the same search as the live release {} (client changes only): no gate needed", cand.id, l.id));
                }
                (Some(_), _) => {
                    // A newer candidate replaces one still in the arena.
                    if let Some((old, child)) = self.gate.as_mut().filter(|(id, _)| *id != cand.id) {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = store.set_release_json(old, "gate", &json!({ "verdict": "superseded", "note": format!("{} replaced it before the gate ended", cand.id) }));
                        log_line(root, "PIPELINE", &format!("arena gate of {old} stopped: {} is newer", cand.id));
                        self.gate = None;
                    }
                    if self.gate.is_none() {
                        let log = std::fs::File::create(data_dir(root).join(format!("gate-{}.log", cand.id))).map_err(|e| e.to_string())?;
                        let err = log.try_clone().map_err(|e| e.to_string())?;
                        let child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
                            .current_dir(root)
                            .args(["release", "gate", &cand.id])
                            .stdout(Stdio::from(log))
                            .stderr(Stdio::from(err))
                            .spawn()
                            .map_err(|e| format!("cannot start the gate: {e}"))?;
                        log_line(root, "PIPELINE", &format!("arena gate started: {} against the live release", cand.id));
                        self.gate = Some((cand.id.clone(), child));
                    }
                }
            }
        }

        // 5. The newest release that passed goes to the canary.
        let builds = fleet_builds(&store);
        // (A release promoted straight to live by the flagship's thread counts too: the
        // canary always carries the newest search that is allowed on the ladder.)
        if let Some(pass) = builds.iter().find(|r| r.status == "gated" || r.status == "live") {
            let on_canary: Vec<i64> = cfg
                .bots
                .iter()
                .filter(|b| b.managed && b.role == "canary")
                .map(|b| store.bot_state(&b.name).ok().flatten().and_then(|s| s.release).and_then(|id| builds.iter().find(|r| r.id == id).map(|r| r.seq)).unwrap_or(0))
                .collect();
            if on_canary.iter().any(|seq| *seq < pass.seq) {
                match release::canary(root, &store, &cfg, &pass.id, None) {
                    Ok(bots) => {
                        for (bot, v) in bots {
                            log_line(root, "PIPELINE", &format!("canary {bot} now runs {} as {v}", pass.id));
                        }
                    }
                    Err(e) => self.say(root, "canary", format!("cannot put {} on the canary: {e}", pass.id)),
                }
            }
        }

        // 6. Ladder evidence for what is on the canary, every ten minutes.
        if self.last_evidence.map(|t| t.elapsed() >= Duration::from_secs(600)).unwrap_or(true) && live_id.is_some() {
            self.last_evidence = Some(Instant::now());
            for r in fleet_builds(&store).iter().filter(|r| r.status == "canary") {
                if let Ok(v) = release::evidence(&store, &r.id, &EvidenceOptions::default()) {
                    let verdict = v["verdict"].as_str().unwrap_or("?").to_string();
                    if verdict == "pass" || verdict == "fail" {
                        self.say(
                            root,
                            &format!("evidence-{}", r.id),
                            format!(
                                "ladder evidence {verdict}: {} has {} games, {:+.1} points against {} on shared opponents, {:.0}% sure it is better{}",
                                r.id,
                                v["candidate"]["games"],
                                100.0 * v["score_difference"].as_f64().unwrap_or(0.0),
                                v["baseline"].as_str().unwrap_or("?"),
                                100.0 * v["confidence_better"].as_f64().unwrap_or(0.0),
                                if verdict == "pass" { ". It has earned promotion; that step is a person's." } else { "" }
                            ),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn run(&mut self, root: &Path, stop: &dyn Fn() -> bool) {
        log_line(root, "PIPELINE", "autopilot started");
        while !stop() {
            if let Err(e) = self.step(root) {
                self.say(root, "error", format!("autopilot step failed: {e}"));
            }
            let until = Instant::now() + Duration::from_secs(60);
            while Instant::now() < until && !stop() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        if let Some((_, child)) = self.gate.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
