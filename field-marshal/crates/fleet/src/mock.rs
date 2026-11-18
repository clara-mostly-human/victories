//! A stand-in for the ladder and a stand-in bot process, for testing the gateway on this
//! machine without a single frame going to the real ladder.
//!
//! `fleet mock-ladder --port 9911 ...` speaks the bot socket's protocol as the ladder's
//! guide describes it: `queue`, `hello`, `ready`, an observation per tick with
//! `self.budget_ms_left`, a turn that echoes the tick within a second, `end`; refusals
//! (`quota_exceeded` keeps the bot queued, `version_limit`), a dropped connection, and a
//! match that resumes with `hello` and the current observation. It can make its first
//! connections slow, to see the gateway replace them.
//!
//! `fleet mock-engine` is a bot process that answers every observation with an empty turn.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::Message;

#[derive(Clone, Debug)]
pub struct LadderOptions {
    pub port: u16,
    /// ticks in a match
    pub ticks: i64,
    /// pause between an answer and the next observation
    pub tick_ms: u64,
    /// pause between `queue` and the match
    pub match_gap_ms: u64,
    /// this many of the first connections are slow ...
    pub slow_first: usize,
    /// ... by this much per turn
    pub slow_ms: u64,
    /// this many of the first queue requests are answered `quota_exceeded` ...
    pub quota_first: usize,
    /// ... and the bot is then held in the queue this long
    pub quota_hold_ms: u64,
    /// the first match of each token loses its connection at this tick
    pub drop_at_tick: Option<i64>,
    /// the first queue request is refused with `version_limit`
    pub version_limit_first: bool,
}

impl Default for LadderOptions {
    fn default() -> Self {
        LadderOptions { port: 9911, ticks: 60, tick_ms: 20, match_gap_ms: 300, slow_first: 0, slow_ms: 400, quota_first: 0, quota_hold_ms: 3000, drop_at_tick: None, version_limit_first: false }
    }
}

struct Saved {
    id: String,
    tick: i64,
    budget: i64,
    at: Instant,
}

#[derive(Default)]
struct Ladder {
    connections: AtomicUsize,
    queues: AtomicUsize,
    matches: AtomicUsize,
    version_limit_done: AtomicBool,
    /// matches whose connection dropped, by token, waiting to be resumed
    saved: Mutex<HashMap<String, Saved>>,
    /// tokens whose first match has already been dropped once
    dropped: Mutex<Vec<String>>,
}

fn say(text: &str) {
    println!("mock-ladder: {text}");
    let _ = std::io::stdout().flush();
}

fn serve(stream: TcpStream, opt: LadderOptions, ladder: Arc<Ladder>) {
    let mut token = String::new();
    let ws = tungstenite::accept_hdr(stream, |req: &Request, resp: Response| {
        token = req.headers().get("Authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim_start_matches("Bearer ").to_string();
        Ok(resp)
    });
    let Ok(mut ws) = ws else { return };
    let n = ladder.connections.fetch_add(1, Ordering::SeqCst);
    let slow = n < opt.slow_first;
    let who = token.chars().take(12).collect::<String>();
    say(&format!("connection {n} from {who}{}", if slow { " (slow)" } else { "" }));
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(10)));
    let send = |ws: &mut tungstenite::WebSocket<TcpStream>, v: Value| ws.send(Message::Text(v.to_string())).is_ok();

    let mut start_at: Option<Instant> = None;
    let mut version = String::new();
    // the match
    let mut id: Option<String> = None;
    let mut tick = 0i64;
    let mut budget = 300_000i64;
    let mut ready = false;
    let mut hello_at = Instant::now();
    let mut awaiting: Option<Instant> = None;
    let mut next_obs = Instant::now();
    let mut idle_run = 0;

    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                match v.get("type").and_then(Value::as_str) {
                    Some("queue") => {
                        version = v.get("version").and_then(Value::as_str).unwrap_or("?").to_string();
                        let k = ladder.queues.fetch_add(1, Ordering::SeqCst);
                        if id.is_some() {
                            send(&mut ws, json!({ "type": "error", "code": "already_queued" }));
                        } else if opt.version_limit_first && !ladder.version_limit_done.swap(true, Ordering::SeqCst) {
                            say(&format!("{who}: version label {version} refused"));
                            send(&mut ws, json!({ "type": "error", "code": "version_limit", "message": "versions can't be edited, register a new label" }));
                        } else if let Some(s) = ladder.saved.lock().unwrap().remove(&token).filter(|s| s.at.elapsed() < Duration::from_secs(10)) {
                            // A match to resume: its hello again, then the current observation.
                            say(&format!("{who}: resumes {} at tick {}", s.id, s.tick));
                            id = Some(s.id.clone());
                            tick = s.tick;
                            budget = s.budget - s.at.elapsed().as_millis() as i64;
                            ready = false;
                            hello_at = Instant::now();
                            awaiting = None;
                            next_obs = Instant::now();
                            send(&mut ws, json!({ "type": "hello", "match_id": s.id, "bot_id": 1, "map_name": "mock-21", "opponent": { "name": "mock-opponent", "version": "v1" }, "resumed": true }));
                        } else if start_at.is_some() {
                            send(&mut ws, json!({ "type": "error", "code": "already_queued" }));
                        } else if k < opt.quota_first {
                            say(&format!("{who}: queue request {k} held back by the quota"));
                            send(&mut ws, json!({ "type": "error", "code": "quota_exceeded", "quota": "concurrent_matches_per_owner", "limit": 5, "used": 5, "retry_after_s": null }));
                            start_at = Some(Instant::now() + Duration::from_millis(opt.quota_hold_ms));
                        } else {
                            start_at = Some(Instant::now() + Duration::from_millis(opt.match_gap_ms));
                        }
                    }
                    Some("ready") => ready = true,
                    Some("leave") => start_at = None,
                    _ => {
                        if let (Some(t), Some(since)) = (v.get("tick").and_then(Value::as_i64), awaiting) {
                            if id.is_none() {
                                send(&mut ws, json!({ "type": "error", "code": "not_in_match" }));
                            } else if t == tick {
                                let charge = since.elapsed().as_millis() as i64 + if slow { opt.slow_ms as i64 } else { 0 };
                                budget -= charge.min(1000);
                                awaiting = None;
                                idle_run = 0;
                                tick += 1;
                                next_obs = Instant::now() + Duration::from_millis(opt.tick_ms);
                            }
                        } else if v.get("tick").is_some() && id.is_none() {
                            send(&mut ws, json!({ "type": "error", "code": "not_in_match" }));
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }

        // The clock.
        if id.is_none() {
            if start_at.map(|t| Instant::now() >= t).unwrap_or(false) {
                start_at = None;
                let m = format!("mock{}_{}", ladder.matches.fetch_add(1, Ordering::SeqCst), version);
                say(&format!("{who}: match {m} starts"));
                id = Some(m.clone());
                tick = 0;
                budget = 300_000;
                ready = false;
                hello_at = Instant::now();
                awaiting = None;
                idle_run = 0;
                next_obs = Instant::now();
                if !send(&mut ws, json!({ "type": "hello", "match_id": m, "bot_id": 1, "map_name": "mock-21", "opponent": { "name": "mock-opponent", "version": "v1" }, "deadlines": { "turn_ms": 1000 } })) {
                    break;
                }
            }
            continue;
        }
        if let Some(since) = awaiting {
            if since.elapsed() >= Duration::from_millis(1000) {
                // No answer in time: the turn is idle.
                budget -= 1000;
                awaiting = None;
                idle_run += 1;
                tick += 1;
                next_obs = Instant::now();
                if idle_run >= 10 {
                    say(&format!("{who}: disqualified after ten idle turns"));
                    send(&mut ws, json!({ "type": "end", "match_id": id, "result": "loss", "reason": "disqualified", "final_tick": tick, "rating_change": -2.0 }));
                    id = None;
                    continue;
                }
            }
        }
        if awaiting.is_none() && (ready || hello_at.elapsed() > Duration::from_secs(3)) && Instant::now() >= next_obs {
            if tick >= opt.ticks {
                say(&format!("{who}: match {} ends, budget left {budget} ms", id.clone().unwrap_or_default()));
                send(&mut ws, json!({ "type": "end", "match_id": id, "result": "win", "reason": "last_standing", "final_tick": tick, "rating_change": 1.25, "replay_url": "mock://replay", "stats": {} }));
                id = None;
                continue;
            }
            if opt.drop_at_tick == Some(tick) {
                let mut dropped = ladder.dropped.lock().unwrap();
                if !dropped.contains(&token) {
                    dropped.push(token.clone());
                    say(&format!("{who}: connection dropped at tick {tick}"));
                    break;
                }
            }
            if slow {
                std::thread::sleep(Duration::from_millis(opt.slow_ms / 2));
            }
            if !send(&mut ws, json!({ "type": "observation", "tick": tick, "self": { "budget_ms_left": budget, "hp": 100 } })) {
                break;
            }
            awaiting = Some(Instant::now());
        }
    }
    // The connection is gone; a match in progress can be resumed for ten seconds.
    if let Some(m) = id {
        ladder.saved.lock().unwrap().insert(token.clone(), Saved { id: m, tick, budget, at: Instant::now() });
    }
    say(&format!("connection {n} from {who} closed"));
}

pub fn run_ladder(opt: LadderOptions) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", opt.port)).map_err(|e| format!("cannot listen on port {}: {e}", opt.port))?;
    say(&format!("listening on ws://127.0.0.1:{}/bot with {opt:?}", opt.port));
    let ladder = Arc::new(Ladder::default());
    for stream in listener.incoming().flatten() {
        let (opt, ladder) = (opt.clone(), ladder.clone());
        std::thread::spawn(move || serve(stream, opt, ladder));
    }
    Ok(())
}

/// A bot process for tests: says ready, answers every observation with an empty turn after
/// `think_ms`, answers heartbeats, and can exit at a given tick (once per match).
pub fn run_engine(think_ms: u64, die_at: Option<i64>, provisional: bool) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut current: Option<String> = None;
    let mut last_tick: Option<i64> = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        match v.get("type").and_then(Value::as_str) {
            Some("hello") => {
                current = v.get("match_id").and_then(Value::as_str).map(str::to_string);
                let _ = writeln!(out, "{}", json!({ "type": "ready" }));
            }
            Some("observation") => {
                let tick = v.get("tick").and_then(Value::as_i64).unwrap_or(0);
                if die_at == Some(tick) {
                    eprintln!("mock-engine: exiting at tick {tick}");
                    std::process::exit(0);
                }
                if provisional {
                    let _ = writeln!(out, "{}", json!({ "tick": tick, "provisional": true }));
                    let _ = out.flush();
                }
                std::thread::sleep(Duration::from_millis(think_ms));
                last_tick = Some(tick);
                let _ = writeln!(out, "{}", json!({ "tick": tick }));
            }
            Some("end") => {
                current = None;
                last_tick = None;
            }
            Some("ping") => {
                let _ = writeln!(out, "{}", json!({ "type": "pong", "id": v.get("id"), "match": current, "tick": last_tick }));
            }
            Some("sync") => {
                eprintln!("mock-engine: set back in step: {line}");
                if v.get("match").map(|m| m.is_null()).unwrap_or(true) {
                    current = None;
                }
            }
            _ => {}
        }
        let _ = out.flush();
    }
}
