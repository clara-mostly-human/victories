//! `fleet run <bot>`: the process launchd keeps alive for one bot.
//!
//! It starts the bot from the release the store says this bot should run, keeps the Mac
//! awake while it plays, restarts it when it exits, and writes every start and exit to the
//! store. Deploying a new release is: change the store, send the bot SIGTERM. The bot
//! finishes its match and exits, and the loop here starts the new release.
//!
//! What an exit means (see `marshal`):
//! - 0: stopped on request or done. Start again at once (this is the hot swap).
//! - 1: the ladder refused us for good (bad token, replaced, banned). Wait two minutes and
//!   try again; three of these within an hour halt the bot until `fleet resume`.
//! - 2: bad arguments or no token. Halt.
//! - 3: another process holds this bot's name. Wait a minute.
//! - anything else: a crash. Restart with a growing delay.

use crate::config::{BotConfig, Config};
use crate::store::{data_dir, default_db, now_ms, Store};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn run_dir(root: &Path) -> PathBuf {
    data_dir(root).join("run")
}

pub fn pid_file(root: &Path, bot: &str) -> PathBuf {
    run_dir(root).join(format!("{bot}.pid"))
}

/// The pid of the bot process, when one is running.
pub fn bot_pid(root: &Path, bot: &str) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_file(root, bot)).ok()?.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub fn signal(pid: i32, sig: libc::c_int) -> bool {
    pid > 0 && unsafe { libc::kill(pid, sig) } == 0
}

/// Sleeps in short steps so a stop request is noticed. Returns false when asked to stop.
fn nap(d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if STOP.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !STOP.load(Ordering::Relaxed)
}

fn say(bot: &str, text: &str) {
    eprintln!("[{bot}] supervisor: {text}");
}

/// Does this release's `marshal` know the fleet flags? Releases built before the hook do not.
fn release_has_hook(dir: &Path) -> bool {
    // The search thread's own releases mark it with a `fleet` file.
    if dir.join("fleet").is_file() {
        return true;
    }
    std::fs::read_to_string(dir.join("release.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("fleet_hook").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// What the client in this release understands (see `release::build`); 1 for older builds.
fn client_level(dir: &Path) -> i64 {
    std::fs::read_to_string(dir.join("release.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("client").and_then(|c| c.as_i64()))
        .unwrap_or(1)
}

/// The command line for one bot, from its configuration and deployed release.
pub fn bot_command(root: &Path, cfg: &Config, bot: &BotConfig, release: &str, release_dir: &Path, version: &str) -> Command {
    let mut c = Command::new(release_dir.join("marshal"));
    c.arg("--bot").arg(&bot.name).arg("--version").arg(version);
    c.arg("--playbooks").arg(release_dir.join(&bot.playbooks));
    c.arg("--log-dir").arg(root.join("logs").join(&bot.name));
    c.arg("--token-file").arg(root.join("bots").join(&bot.name));
    if let Some(url) = &bot.url {
        c.arg("--url").arg(url);
    }
    if client_level(release_dir) >= 2 {
        if !cfg.prefer_addresses.is_empty() {
            c.arg("--prefer-address").arg(cfg.prefer_addresses.join(","));
        }
        if bot.role != "flagship" && cfg.queue_gap_ms > 0 {
            c.arg("--requeue-delay-ms").arg(cfg.queue_gap_ms.to_string());
        }
    }
    if release_has_hook(release_dir) {
        c.arg("--fleet-db").arg(default_db(root)).arg("--release").arg(release);
        c.arg("--ours").arg(cfg.names().join(","));
        if bot.experiments {
            c.arg("--experiments");
        }
    }
    c.current_dir(root);
    c
}

fn wait_child(child: &mut Child, bot: &str) -> Option<i32> {
    let mut told = false;
    let mut told_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {}
            Err(_) => return None,
        }
        if STOP.load(Ordering::Relaxed) && !told {
            // Pass the request on: the bot finishes its match first.
            signal(child.id() as i32, libc::SIGTERM);
            told = true;
            told_at = Instant::now();
            say(bot, "stop requested; waiting for the bot to finish its match");
        }
        if told && told_at.elapsed() > Duration::from_secs(900) {
            say(bot, "the bot did not stop within 15 minutes; killing it");
            let _ = child.kill();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn run(root: &Path, name: &str) -> i32 {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let _ = std::fs::create_dir_all(run_dir(root));
    let _ = std::fs::create_dir_all(root.join("logs").join(name));
    // One supervisor per bot.
    let lock_path = run_dir(root).join(format!("{name}.supervisor.lock"));
    let lock = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            say(name, &format!("cannot open {}: {e}", lock_path.display()));
            return 2;
        }
    };
    if lock.try_lock().is_err() {
        say(name, "another supervisor is already running this bot");
        return 3;
    }

    let mut crashes = 0u32;
    let mut reported_wait = String::new();
    while !STOP.load(Ordering::Relaxed) {
        // Everything is read afresh on each start, so a deploy or a config edit takes effect
        // on the next restart without touching launchd.
        let wait = |why: String, reported: &mut String| {
            if *reported != why {
                say(name, &format!("not running: {why}"));
                *reported = why;
            }
        };
        let cfg = match Config::load(root) {
            Ok(c) => c,
            Err(e) => {
                wait(e, &mut reported_wait);
                nap(Duration::from_secs(30));
                continue;
            }
        };
        let Some(bot) = cfg.bot(name).cloned() else {
            wait(format!("{name} is not in fleet/bots.json"), &mut reported_wait);
            nap(Duration::from_secs(30));
            continue;
        };
        let store = match Store::open(&default_db(root), Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => {
                wait(format!("store unavailable: {e}"), &mut reported_wait);
                nap(Duration::from_secs(30));
                continue;
            }
        };
        let state = store.bot_state(name).ok().flatten();
        let halted = state.as_ref().and_then(|s| s.halted.clone());
        let deployed = state.as_ref().and_then(|s| Some((s.release.clone()?, s.version.clone()?)));
        let problem = if !bot.managed {
            Some("managed is false in fleet/bots.json".to_string())
        } else if bot.gateway {
            Some("run by the gateway (gateway is true in fleet/bots.json)".to_string())
        } else if let Some(h) = halted {
            Some(format!("halted ({h}); `fleet resume {name}` starts it again"))
        } else if deployed.is_none() {
            Some(format!("no release deployed; `fleet deploy {name} <release>`"))
        } else if !root.join("bots").join(name).is_file() {
            Some(format!("no token file bots/{name}: the bot must be registered first"))
        } else {
            None
        };
        if let Some(why) = problem {
            wait(why, &mut reported_wait);
            drop(store);
            nap(Duration::from_secs(15));
            continue;
        }
        reported_wait.clear();
        let (release, version) = deployed.unwrap();
        let release_dir = store.release(&release).ok().flatten().map(|r| PathBuf::from(r.path)).unwrap_or_else(|| data_dir(root).join("releases").join(&release));
        if !release_dir.join("marshal").is_file() {
            let _ = store.set_halted(name, Some(&format!("release {release} has no binary at {}", release_dir.display())));
            continue;
        }

        let mut cmd = bot_command(root, &cfg, &bot, &release, &release_dir, &version);
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = store.set_halted(name, Some(&format!("cannot start {}: {e}", release_dir.join("marshal").display())));
                continue;
            }
        };
        let pid = child.id() as i32;
        let _ = std::fs::write(pid_file(root, name), pid.to_string());
        // Keep the Mac from idling into sleep for as long as the bot lives.
        let mut awake = Command::new("/usr/bin/caffeinate").arg("-i").arg("-w").arg(pid.to_string()).spawn().ok();
        say(name, &format!("started {release} as {version}, pid {pid}"));
        let _ = store.conn.execute(
            "INSERT INTO runs (bot, release, version, pid, started_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![name, release, version, pid, now_ms()],
        );
        let run_id = store.conn.last_insert_rowid();
        // The store is closed while the bot plays; the bot has its own connection.
        drop(store);

        let code = wait_child(&mut child, name);
        let _ = std::fs::remove_file(pid_file(root, name));
        if let Some(a) = awake.as_mut() {
            let _ = a.kill();
            let _ = a.wait();
        }
        let ran = started.elapsed();
        let exit = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        say(name, &format!("bot exited ({exit}) after {} s", ran.as_secs()));
        let store = Store::open(&default_db(root), Duration::from_secs(5)).ok();
        if let Some(s) = &store {
            let _ = s.conn.execute("UPDATE runs SET ended_ms = ?2, exit = ?3 WHERE id = ?1", rusqlite::params![run_id, now_ms(), exit]);
        }
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        match code {
            Some(0) => {
                crashes = 0;
                // A bot that exits cleanly straight away (nothing to do) must not spin.
                if ran < Duration::from_secs(5) {
                    nap(Duration::from_secs(5));
                }
            }
            Some(1) => {
                let recent: i64 = store
                    .as_ref()
                    .and_then(|s| s.conn.query_row("SELECT COUNT(*) FROM runs WHERE bot = ?1 AND exit = '1' AND ended_ms > ?2", rusqlite::params![name, now_ms() - 3_600_000], |r| r.get(0)).ok())
                    .unwrap_or(0);
                if let Some(s) = &store {
                    s.event(Some(name), "fatal_exit", &json!({ "recent": recent, "release": release }));
                    if recent >= 3 {
                        let _ = s.set_halted(name, Some("3 fatal ladder errors within an hour; see the bot's log"));
                    }
                }
                if recent < 3 {
                    say(name, "the ladder refused the bot; trying again in 2 minutes");
                    nap(Duration::from_secs(120));
                }
            }
            Some(2) => {
                if let Some(s) = &store {
                    let _ = s.set_halted(name, Some("the bot rejected its arguments or found no token"));
                }
            }
            Some(3) => {
                say(name, "another process is running this bot; trying again in a minute");
                nap(Duration::from_secs(60));
            }
            _ => {
                crashes = if ran > Duration::from_secs(600) { 1 } else { crashes + 1 };
                let delay = Duration::from_secs((1u64 << crashes.min(8)).min(300));
                if let Some(s) = &store {
                    s.event(Some(name), "crash", &json!({ "exit": exit, "ran_s": ran.as_secs(), "retry_in_s": delay.as_secs(), "release": release }));
                }
                say(name, &format!("crash; restarting in {} s", delay.as_secs()));
                nap(delay);
            }
        }
    }
    say(name, "stopped");
    0
}

// ---- launchd --------------------------------------------------------------------------------

pub fn label(bot: &str) -> String {
    format!("com.victris.{bot}")
}

fn agents_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library").join("LaunchAgents")
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A launchd agent. `interval` runs the program every so many seconds instead of keeping it alive.
pub fn plist(label: &str, root: &Path, args: &[String], log: &Path, interval: Option<u32>, batch: bool) -> String {
    let args: String = args.iter().map(|a| format!("    <string>{}</string>\n", xml(a))).collect();
    // Batch agents (collector, health pass) yield the processor and the disk to the bots.
    let low = "  <key>Nice</key><integer>15</integer>\n  <key>LowPriorityIO</key><true/>\n  <key>ProcessType</key><string>Background</string>\n";
    let mode = match interval {
        Some(s) => format!("  <key>StartInterval</key><integer>{s}</integer>\n  <key>RunAtLoad</key><true/>\n{low}"),
        None if batch => format!("  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>30</integer>\n{low}"),
        // ExitTimeOut: launchd waits this long after SIGTERM, so a stop lets the match finish.
        None => "  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>10</integer>\n  <key>ExitTimeOut</key><integer>960</integer>\n  <key>ProcessType</key><string>Interactive</string>\n".to_string(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>WorkingDirectory</key><string>{root}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>VICTRIS_ROOT</key><string>{root}</string>
    <key>HOME</key><string>{home}</string>
  </dict>
{mode}  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        label = xml(label),
        root = xml(&root.display().to_string()),
        home = xml(&std::env::var("HOME").unwrap_or_default()),
        log = xml(&log.display().to_string()),
    )
}

pub fn launchd_loaded(label: &str) -> bool {
    Command::new("launchctl").arg("print").arg(format!("gui/{}/{label}", uid())).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Copies this program to `data/bin/fleet`, so the agents do not run a binary that the next
/// `cargo build` replaces.
pub fn install_self(root: &Path) -> Result<PathBuf, String> {
    let dir = data_dir(root).join("bin");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join("fleet");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if me != dest {
        // Write beside it and rename: a running supervisor keeps the old file.
        let tmp = dir.join("fleet.new");
        std::fs::copy(&me, &tmp).map_err(|e| format!("cannot copy {}: {e}", me.display()))?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    Ok(dest)
}

pub fn install_agent(label: &str, text: &str) -> Result<(), String> {
    let dir = agents_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{label}.plist"));
    if launchd_loaded(label) {
        let _ = Command::new("launchctl").arg("bootout").arg(format!("gui/{}/{label}", uid())).status();
    }
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    // launchd needs a moment to let go of the old job; loading too early fails with
    // "Bootstrap failed: 5: Input/output error".
    let mut last = String::new();
    // An agent that lets a match finish before it exits can take minutes to go.
    for _ in 0..2000 {
        if !launchd_loaded(label) {
            let out = Command::new("launchctl").arg("bootstrap").arg(format!("gui/{}", uid())).arg(&path).output().map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok(());
            }
            last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(format!("launchctl bootstrap failed: {last}"))
}

pub fn remove_agent(label: &str) -> Result<(), String> {
    if launchd_loaded(label) {
        let _ = Command::new("launchctl").arg("bootout").arg(format!("gui/{}/{label}", uid())).status();
    }
    let path = agents_dir().join(format!("{label}.plist"));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Asks the running bot to finish its match and exit. The supervisor then starts whatever
/// the store says. Returns the pid that was signalled.
pub fn restart_bot(root: &Path, bot: &str) -> Option<i32> {
    let pid = bot_pid(root, bot)?;
    signal(pid, libc::SIGTERM).then_some(pid)
}
