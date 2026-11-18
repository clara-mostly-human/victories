//! The gateway: one process that owns every connection to the ladder.
//!
//! Each bot's decision engine stays its own process (`marshal --stdio`, from whatever
//! release that bot is deployed on), but it no longer talks to the ladder. The gateway
//! holds the bot's WebSocket and passes frames both ways over the engine's pipes. That puts
//! everything that leaves this machine for the ladder behind one piece of code:
//!
//! - connects and `queue` requests are paced across the whole fleet, so five bots that
//!   lose their sockets together do not all knock at once;
//! - a refused `queue` is retried with a backoff, a fatal refusal halts that bot only;
//! - an engine that has not answered an observation in time gets an idle turn sent for it,
//!   so a stalled or crashed engine costs turns, never a disqualification, and it is
//!   restarted and told about the match it is in;
//! - a new release is swapped in between two matches by replacing the engine, while the
//!   connection to the ladder stays up;
//! - every frame, connect and reset is counted in one place.
//!
//! A bot is run this way when its entry in `fleet/bots.json` has `"gateway": true`.

use crate::config::{BotConfig, Config};
use crate::online::log_line;
use crate::store::{data_dir, default_db, now_ms, Store};
use crate::supervise::bot_command;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const DEFAULT_URL: &str = "wss://victrisarena.com/bot";

/// Errors that reconnecting cannot fix (the same list the engine's own client uses).
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

#[derive(Clone)]
pub struct Options {
    /// least time between two connection attempts, fleet-wide
    pub connect_gap: Duration,
    /// least time between two `queue` requests, fleet-wide
    pub queue_gap: Duration,
    /// an engine that has not answered an observation after this long gets an idle turn
    pub answer_deadline: Duration,
}

impl Default for Options {
    fn default() -> Self {
        // The ladder allows one second a turn; our reply takes about 100 ms to get there.
        Options { connect_gap: Duration::from_millis(300), queue_gap: Duration::from_millis(300), answer_deadline: Duration::from_millis(800) }
    }
}

/// A budget: at most `limit` of something in any stretch of `window`. Whoever asks when it
/// is spent waits for the oldest entry to age out.
pub struct Budget {
    pub name: &'static str,
    pub limit: usize,
    pub window: Duration,
    times: Mutex<std::collections::VecDeque<Instant>>,
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
    fn room_at(&self) -> Option<Instant> {
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

    /// Waits for room, then books one use.
    pub fn take(&self, stop: &dyn Fn() -> bool) -> bool {
        if !self.wait_for_room(stop) {
            return false;
        }
        self.record();
        true
    }
}

/// What all bot threads share: the pacing of connects and queue requests, and the counts.
pub struct Shared {
    opt: Options,
    next_connect: Mutex<Instant>,
    next_queue: Mutex<Instant>,
    pub connects: AtomicU64,
    pub drops: AtomicU64,
    pub frames_out: AtomicU64,
    pub frames_in: AtomicU64,
    pub idle_turns: AtomicU64,
    pub refusals: AtomicU64,
    /// ladder addresses on which a match was slow, most recent last
    pub slow: Mutex<Vec<std::net::IpAddr>>,
    /// the comms budget: new connections, queue requests and matches started
    pub connect_budget: Budget,
    pub queue_budget: Budget,
    pub match_budget: Budget,
}

impl Shared {
    pub fn new(opt: Options, cfg: &Config) -> Shared {
        let now = Instant::now();
        Shared {
            connect_budget: Budget::new("connection", cfg.comms_connects_per_minute, Duration::from_secs(60)),
            queue_budget: Budget::new("queue request", cfg.comms_queues_per_minute, Duration::from_secs(60)),
            match_budget: Budget::new("match", cfg.comms_matches_per_day, Duration::from_secs(86_400)),
            opt,
            next_connect: Mutex::new(now),
            next_queue: Mutex::new(now),
            connects: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            frames_in: AtomicU64::new(0),
            idle_turns: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            slow: Mutex::new(Vec::new()),
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
}

/// One engine process and its pipes.
struct Engine {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    buf: Vec<u8>,
    release: String,
    version: String,
}

impl Engine {
    fn spawn(root: &Path, cfg: &Config, bot: &BotConfig, release: &str, release_dir: &Path, version: &str) -> Result<Engine, String> {
        let log_dir = root.join("logs").join(&bot.name);
        let _ = std::fs::create_dir_all(&log_dir);
        let err = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("run.log")).map_err(|e| e.to_string())?;
        let mut cmd = bot_command(root, cfg, bot, release, release_dir, version);
        cmd.arg("--stdio");
        if crate::supervise::client_level(release_dir) >= 3 {
            cmd.arg("--provisional");
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(err));
        let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", release_dir.join("marshal").display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        // Reads from the engine never block the gateway.
        unsafe {
            let fd = stdout.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Ok(Engine { child, stdin, stdout, buf: Vec::new(), release: release.to_string(), version: version.to_string() })
    }

    fn send(&mut self, frame: &str) -> bool {
        self.stdin.write_all(frame.as_bytes()).and_then(|_| self.stdin.write_all(b"\n")).and_then(|_| self.stdin.flush()).is_ok()
    }

    /// Whole lines the engine has written since the last call. `Err` when it has gone.
    fn lines(&mut self) -> Result<Vec<String>, ()> {
        let mut chunk = [0u8; 8192];
        loop {
            match self.stdout.read(&mut chunk) {
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

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// Waits until the socket or the engine has something to read, at most `timeout`.
fn wait_readable(fds: &[i32], timeout: Duration) {
    let mut set: Vec<libc::pollfd> = fds.iter().map(|fd| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }).collect();
    unsafe {
        libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, timeout.as_millis() as libc::c_int);
    }
}

struct Deployed {
    release: String,
    version: String,
    dir: PathBuf,
}

/// What the store says this bot should run, or why it should not run at all.
fn deployed(root: &Path, name: &str) -> Result<(Config, BotConfig, Deployed), String> {
    let cfg = Config::load(root)?;
    let bot = cfg.bot(name).cloned().ok_or_else(|| format!("{name} is not in fleet/bots.json"))?;
    if !bot.managed || !bot.gateway {
        return Err("not set to run through the gateway in fleet/bots.json".into());
    }
    let store = Store::open(&default_db(root), Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let st = store.bot_state(name).map_err(|e| e.to_string())?.ok_or("no release deployed")?;
    if let Some(h) = st.halted {
        return Err(format!("halted ({h})"));
    }
    let (release, version) = (st.release.ok_or("no release deployed")?, st.version.ok_or("no version")?);
    let dir = store.release(&release).ok().flatten().map(|r| PathBuf::from(r.path)).unwrap_or_else(|| data_dir(root).join("releases").join(&release));
    if !dir.join("marshal").is_file() {
        return Err(format!("release {release} has no binary"));
    }
    Ok((cfg, bot, Deployed { release, version, dir }))
}

fn event(root: &Path, bot: &str, kind: &str, detail: Value) {
    eprintln!("[{bot}] gateway: {kind} {detail}");
    if let Ok(store) = Store::open(&default_db(root), Duration::from_millis(500)) {
        store.event(Some(bot), &format!("gateway_{kind}"), &detail);
    }
}

fn queue_frame(version: &str) -> String {
    json!({ "type": "queue", "version": version }).to_string()
}

/// Runs one bot until asked to stop: its connection, its engine, and everything between.
pub fn run_bot(root: &Path, name: &str, shared: &Shared, stop: &dyn Fn() -> bool) {
    // The same lock the engine's own client takes: one connection per token, whoever holds it.
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
        note("another process is running this bot; waiting".into());
        if !nap(Duration::from_secs(5), stop) {
            return;
        }
    }

    let mut engine: Option<Engine> = None;
    let mut backoff = Duration::ZERO;
    // the match in progress survives a reconnect and an engine restart
    let mut last_hello: Option<String> = None;
    let mut in_match = false;
    let mut current_match: Option<String> = None;

    'outer: while !stop() {
        let (cfg, bot, dep) = match deployed(root, name) {
            Ok(x) => x,
            Err(why) => {
                note(format!("not running: {why}"));
                if let Some(e) = engine.take() {
                    e.stop();
                }
                nap(Duration::from_secs(5), stop);
                continue;
            }
        };
        let token = match std::fs::read_to_string(root.join("bots").join(name)) {
            Ok(t) => t.split_whitespace().collect::<String>(),
            Err(_) => {
                note(format!("not running: no token file bots/{name}"));
                nap(Duration::from_secs(15), stop);
                continue;
            }
        };
        let url = bot.url.clone().unwrap_or_else(|| DEFAULT_URL.to_string());

        // The engine for the deployed release. Between matches a different release or
        // version label replaces it; during a match the one that is playing stays.
        let stale = engine.as_ref().map(|e| e.release != dep.release || e.version != dep.version).unwrap_or(true);
        if stale && !in_match {
            if let Some(e) = engine.take() {
                e.stop();
            }
            match Engine::spawn(root, &cfg, &bot, &dep.release, &dep.dir, &dep.version) {
                Ok(e) => {
                    let _ = std::fs::write(crate::supervise::pid_file(root, name), e.child.id().to_string());
                    event(root, name, "engine_started", json!({ "release": dep.release, "version": dep.version, "pid": e.child.id() }));
                    engine = Some(e);
                }
                Err(why) => {
                    note(format!("not running: {why}"));
                    nap(Duration::from_secs(15), stop);
                    continue;
                }
            }
        }

        // Connect, in the fleet's turn.
        if !backoff.is_zero() && !nap(backoff, stop) {
            break;
        }
        if !shared.connect_budget.take(stop) {
            break;
        }
        Shared::pace(&shared.next_connect, shared.opt.connect_gap);
        shared.connects.fetch_add(1, Ordering::Relaxed);
        let prefer: Vec<std::net::IpAddr> = cfg.prefer_addresses.iter().filter_map(|a| a.parse().ok()).collect();
        let (mut ws, remote) = match connect(&url, &token, &prefer, shared) {
            Ok(x) => x,
            Err(why) => {
                backoff = (backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                event(root, name, "connect_failed", json!({ "why": why, "retry_in_s": backoff.as_secs() }));
                continue;
            }
        };
        let connected = Instant::now();
        let version = engine.as_ref().map(|e| e.version.clone()).unwrap_or(dep.version.clone());
        if !shared.match_budget.wait_for_room(stop) || !shared.queue_budget.take(stop) {
            break;
        }
        Shared::pace(&shared.next_queue, shared.opt.queue_gap);
        if send(&mut ws, shared, queue_frame(&version)).is_err() {
            continue;
        }
        // the engine's quick answer for the tick it is working on
        let mut fallback: Option<(i64, String)> = None;
        // ticks of this match on which the override test sent an empty turn first
        let mut test_ticks: Vec<i64> = Vec::new();
        let mut requeue_at: Option<Instant> = None;
        // a match just ended and the engine has not yet said what it would do next
        let mut after_end: Option<Instant> = None;
        let mut refusals = 0u32;
        // the observation the engine owes an answer to
        let mut pending: Option<(i64, Instant)> = None;
        // a restarted engine was replayed the hello; its `ready` must not go to the ladder twice
        let mut swallow_ready = false;
        let mut last_heard = Instant::now();
        let mut last_ping = Instant::now();
        let mut last_check = Instant::now();
        let why_closed: String;

        'conn: loop {
            // -- what the ladder sent --------------------------------------------------------
            loop {
                match ws.read() {
                    Ok(Message::Text(text)) => {
                        last_heard = Instant::now();
                        shared.frames_in.fetch_add(1, Ordering::Relaxed);
                        let msg: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
                        match kind {
                            "hello" => {
                                let id = msg.get("match_id").and_then(Value::as_str).unwrap_or("").to_string();
                                if current_match.as_deref() != Some(id.as_str()) {
                                    shared.match_budget.record();
                                    current_match = Some(id);
                                }
                                in_match = true;
                                refusals = 0;
                                requeue_at = None;
                                after_end = None;
                            }
                            "observation" => {
                                pending = msg.get("tick").and_then(Value::as_i64).map(|t| (t, Instant::now()));
                                // The override test: on every Nth tick an empty turn goes out at
                                // once, ahead of the engine's real one, to learn which of two
                                // answers for one tick the ladder applies.
                                if let (Some((tick, _)), true) = (pending, bot.provisional_test_every > 0) {
                                    if tick > 0 && tick % bot.provisional_test_every as i64 == 0 {
                                        if let Err(why) = send(&mut ws, shared, json!({ "tick": tick }).to_string()) {
                                            why_closed = why;
                                            break 'conn;
                                        }
                                        test_ticks.push(tick);
                                    }
                                }
                            }
                            "end" => {
                                in_match = false;
                                last_hello = None;
                                pending = None;
                                if !test_ticks.is_empty() {
                                    event(root, name, "provisional_test", json!({ "match": current_match, "ticks": test_ticks }));
                                    test_ticks.clear();
                                }
                            }
                            "error" => {
                                let code = msg.get("code").and_then(Value::as_str).unwrap_or("?").to_string();
                                if FATAL_CODES.contains(&code.as_str()) {
                                    if let Some(e) = engine.as_mut() {
                                        e.send(&text);
                                    }
                                    if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
                                        let _ = store.set_halted(name, Some(&format!("the ladder refused the bot: {code}")));
                                    }
                                    log_line(root, "ALERT", &format!("{name} is halted: the ladder refused it ({code}). `fleet resume {name}` starts it again."));
                                    let _ = ws.close(None);
                                    in_match = false;
                                    continue 'outer;
                                }
                                if !in_match {
                                    // Refused while waiting for a match: ask again, later each time.
                                    refusals += 1;
                                    shared.refusals.fetch_add(1, Ordering::Relaxed);
                                    let wait = msg.get("retry_after_s").and_then(Value::as_u64).unwrap_or((2 + refusals as u64).min(15)).clamp(1, 120);
                                    requeue_at = Some(Instant::now() + Duration::from_secs(wait));
                                }
                            }
                            _ => {}
                        }
                        // The engine sees every frame, so its own log and tracker stay whole.
                        // It cannot see the socket, so the hello tells it which address it is on.
                        let forward = if kind == "hello" {
                            let mut m = msg.clone();
                            m["_remote"] = json!(remote.to_string());
                            let t = m.to_string();
                            last_hello = Some(t.clone());
                            t
                        } else {
                            text.clone()
                        };
                        if let Some(e) = engine.as_mut() {
                            e.send(&forward);
                        }
                        if kind == "end" {
                            // The engine answers `end` with what it would do next: queue, or
                            // reconnect because this connection was slow. Wait for that.
                            after_end = Some(Instant::now());
                        }
                    }
                    Ok(Message::Close(_)) => {
                        why_closed = "the ladder closed the connection".into();
                        break 'conn;
                    }
                    Ok(_) => last_heard = Instant::now(),
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
                    Err(e) => {
                        why_closed = format!("read failed: {e}");
                        break 'conn;
                    }
                }
            }

            // -- what the engine wrote -------------------------------------------------------
            let lines = match engine.as_mut().map(|e| e.lines()) {
                Some(Ok(lines)) => lines,
                Some(Err(())) | None => {
                    // The engine has gone. Start another and, mid-match, tell it where it is.
                    if let Some(e) = engine.take() {
                        e.stop();
                    }
                    event(root, name, "engine_died", json!({ "in_match": in_match }));
                    if let Ok((cfg, bot, dep)) = deployed(root, name) {
                        if let Ok(mut e) = Engine::spawn(root, &cfg, &bot, &dep.release, &dep.dir, &dep.version) {
                            let _ = std::fs::write(crate::supervise::pid_file(root, name), e.child.id().to_string());
                            if let (true, Some(h)) = (in_match, last_hello.as_ref()) {
                                e.send(h);
                                swallow_ready = true;
                            }
                            engine = Some(e);
                        }
                    }
                    if engine.is_none() {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Vec::new()
                }
            };
            let mut engine_said: Option<&'static str> = None;
            for line in lines {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match v.get("type").and_then(Value::as_str) {
                    // The gateway decides when to queue and when to reconnect; the engine advises.
                    Some("queue") => {
                        engine_said = Some("queue");
                        continue;
                    }
                    Some("reconnect") => {
                        engine_said = Some("reconnect");
                        continue;
                    }
                    Some("leave") => continue,
                    Some("ready") if swallow_ready => {
                        swallow_ready = false;
                        continue;
                    }
                    _ => {}
                }
                // The engine's quick fallback for this tick: kept, not sent. The ladder takes
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
                if let Some(tick) = v.get("tick").and_then(Value::as_i64) {
                    match pending {
                        Some((want, _)) if want == tick => pending = None,
                        // An answer to a tick we already answered for it, or an old one.
                        _ => continue,
                    }
                }
                if let Err(why) = send(&mut ws, shared, line) {
                    why_closed = why;
                    break 'conn;
                }
            }

            // -- between two matches -------------------------------------------------------------
            if let Some(since) = after_end {
                if engine_said.is_some() || since.elapsed() > Duration::from_millis(500) {
                    after_end = None;
                    if stop() {
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        let _ = ws.close(None);
                        break 'outer;
                    }
                    // The moment to change engines: the old one has written its match.
                    let gap = match deployed(root, name) {
                        Ok((cfg, bot, dep)) => {
                            if engine.as_ref().map(|e| e.release != dep.release || e.version != dep.version).unwrap_or(true) {
                                if let Some(e) = engine.take() {
                                    e.stop();
                                }
                                match Engine::spawn(root, &cfg, &bot, &dep.release, &dep.dir, &dep.version) {
                                    Ok(e) => {
                                        let _ = std::fs::write(crate::supervise::pid_file(root, name), e.child.id().to_string());
                                        event(root, name, "engine_swapped", json!({ "release": dep.release, "version": dep.version, "pid": e.child.id() }));
                                        engine = Some(e);
                                    }
                                    Err(why) => event(root, name, "engine_failed", json!({ "why": why })),
                                }
                            }
                            // Every bot but the flagship leaves the freed match slot alone for a moment.
                            if bot.role == "flagship" { 0 } else { cfg.queue_gap_ms }
                        }
                        Err(why) => {
                            // Halted or undeployed: leave the ladder until that changes.
                            note(format!("not running: {why}"));
                            let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                            let _ = ws.close(None);
                            continue 'outer;
                        }
                    };
                    if engine_said == Some("reconnect") {
                        // The engine measured a slow connection. Every bot now tries this
                        // address last, and this bot connects afresh before it queues.
                        {
                            let mut slow = shared.slow.lock().unwrap_or_else(|p| p.into_inner());
                            slow.retain(|a| *a != remote);
                            slow.push(remote);
                            if slow.len() >= 4 {
                                slow.remove(0);
                            }
                        }
                        event(root, name, "slow_connection", json!({ "remote": remote.to_string() }));
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        let _ = ws.close(None);
                        backoff = Duration::ZERO;
                        continue 'outer;
                    }
                    requeue_at = Some(Instant::now() + Duration::from_millis(gap));
                }
            }

            // -- the clock ---------------------------------------------------------------------
            if let Some((tick, since)) = pending {
                if since.elapsed() >= shared.opt.answer_deadline {
                    // The engine is late. Its quick fallback for this tick goes out if it
                    // gave one; an empty turn if not. Either keeps the bot in the match.
                    pending = None;
                    shared.idle_turns.fetch_add(1, Ordering::Relaxed);
                    let (frame, kind) = match fallback.take() {
                        Some((t, f)) if t == tick => (f, "fallback_turn"),
                        _ => (json!({ "tick": tick }).to_string(), "idle_turn"),
                    };
                    if let Err(why) = send(&mut ws, shared, frame) {
                        why_closed = why;
                        break 'conn;
                    }
                    event(root, name, kind, json!({ "tick": tick, "waited_ms": since.elapsed().as_millis() as u64 }));
                }
            }
            if let Some(at) = requeue_at {
                if in_match {
                    requeue_at = None;
                } else if Instant::now() >= at {
                    requeue_at = None;
                    let version = engine.as_ref().map(|e| e.version.clone()).unwrap_or_default();
                    if !shared.match_budget.wait_for_room(stop) || !shared.queue_budget.take(stop) {
                        let _ = ws.close(None);
                        break 'outer;
                    }
                    Shared::pace(&shared.next_queue, shared.opt.queue_gap);
                    if let Err(why) = send(&mut ws, shared, queue_frame(&version)) {
                        why_closed = why;
                        break 'conn;
                    }
                }
            }
            if last_heard.elapsed() > Duration::from_secs(60) {
                why_closed = "no traffic for 60 s".into();
                break 'conn;
            }
            if last_ping.elapsed() > Duration::from_secs(20) {
                last_ping = Instant::now();
                if ws.send(Message::Ping(Vec::new())).is_err() {
                    why_closed = "ping failed".into();
                    break 'conn;
                }
            }
            if !in_match && last_check.elapsed() > Duration::from_secs(5) {
                last_check = Instant::now();
                if stop() {
                    let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                    let _ = ws.close(None);
                    break 'outer;
                }
                // A halt, an undeploy or a new release while waiting in the queue.
                match deployed(root, name) {
                    Ok((_, _, dep)) if engine.as_ref().map(|e| e.release == dep.release && e.version == dep.version).unwrap_or(false) => {}
                    _ => {
                        let _ = send(&mut ws, shared, json!({ "type": "leave" }).to_string());
                        let _ = ws.close(None);
                        continue 'outer;
                    }
                }
            }

            // Sleep until the ladder or the engine has something, or the next clock check.
            let mut fds = Vec::new();
            fds.extend(socket_fd(&ws));
            fds.extend(engine.as_ref().map(|e| e.stdout.as_raw_fd()));
            let nap = match pending {
                Some((_, since)) => shared.opt.answer_deadline.saturating_sub(since.elapsed()).min(Duration::from_millis(100)),
                None => Duration::from_millis(100),
            };
            wait_readable(&fds, nap.max(Duration::from_millis(1)));
        }

        // The connection is gone. A match in progress goes on at the ladder: reconnect at
        // once if this one had been working, and the ladder sends hello and the observation.
        shared.drops.fetch_add(1, Ordering::Relaxed);
        backoff = if connected.elapsed() > Duration::from_secs(30) { Duration::ZERO } else { (backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30)) };
        // The bot's own log keeps the record of drops, in the form the engine's client wrote
        // them when it held the socket, so everything that counts resets still sees them.
        let version = engine.as_ref().map(|e| e.version.clone()).unwrap_or_default();
        let line = json!({ "t": "disconnected", "detail": { "why": why_closed, "in_match": in_match, "retry_in_s": backoff.as_secs(), "by": "gateway" }, "ts": now_ms(), "bot": name, "version": version });
        let day = crate::online::utc_stamp()[..10].to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("logs").join(name).join(format!("{name}-{day}.jsonl"))) {
            // One write call: the engine appends to the same file, and a line written in
            // pieces could end up inside one of its lines.
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
        event(root, name, "disconnected", json!({ "why": why_closed, "in_match": in_match, "retry_in_s": backoff.as_secs() }));
    }

    if let Some(e) = engine.take() {
        e.stop();
    }
    let _ = std::fs::remove_file(crate::supervise::pid_file(root, name));
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

/// Runs every bot that is set to go through the gateway, each on its own thread, and
/// starts a thread for any bot that is switched over while it runs.
pub fn run(root: &Path, stop: Arc<dyn Fn() -> bool + Send + Sync>) {
    let cfg = Config::load(root).unwrap_or_default();
    let shared = Arc::new(Shared::new(Options::default(), &cfg));
    let mut last_state = Instant::now() - Duration::from_secs(60);
    let mut running: Vec<(String, std::thread::JoinHandle<()>)> = Vec::new();
    let mut last_report = Instant::now();
    eprintln!("gateway started");
    while !stop() {
        if let Ok(cfg) = Config::load(root) {
            for b in cfg.bots.iter().filter(|b| b.managed && b.gateway) {
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
        if last_state.elapsed() >= Duration::from_secs(10) {
            // What the budget looks like right now, for `fleet status`.
            last_state = Instant::now();
            let b = |x: &Budget| json!({ "used": x.used(), "limit": x.limit, "window_s": x.window.as_secs(), "times_waited": x.waits.load(Ordering::Relaxed) });
            let state = json!({
                "ts_ms": now_ms(),
                "bots": running.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                "connections": b(&shared.connect_budget), "queue_requests": b(&shared.queue_budget), "matches": b(&shared.match_budget),
                "since_start": {
                    "connects": shared.connects.load(Ordering::Relaxed), "drops": shared.drops.load(Ordering::Relaxed),
                    "frames_out": shared.frames_out.load(Ordering::Relaxed), "frames_in": shared.frames_in.load(Ordering::Relaxed),
                    "idle_turns": shared.idle_turns.load(Ordering::Relaxed), "refused_queues": shared.refusals.load(Ordering::Relaxed),
                },
                "slow_addresses": shared.slow.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            });
            let _ = std::fs::create_dir_all(data_dir(root).join("run"));
            let _ = std::fs::write(data_dir(root).join("run").join("gateway.json"), state.to_string());
        }
        if last_report.elapsed() >= Duration::from_secs(300) {
            last_report = Instant::now();
            eprintln!(
                "gateway: {} bots; since start {} connects, {} drops, {} frames out, {} in, {} idle turns sent for late engines, {} refused queues",
                running.len(),
                shared.connects.load(Ordering::Relaxed),
                shared.drops.load(Ordering::Relaxed),
                shared.frames_out.load(Ordering::Relaxed),
                shared.frames_in.load(Ordering::Relaxed),
                shared.idle_turns.load(Ordering::Relaxed),
                shared.refusals.load(Ordering::Relaxed)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    for (_, h) in running {
        let _ = h.join();
    }
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
}
