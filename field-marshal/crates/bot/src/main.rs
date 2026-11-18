//! marshal: one ladder bot process.
//!
//!   marshal --bot <name> [--version v1] [--url wss://victrisarena.com/bot] [--matches N]
//!           [--playbooks file.json] [--log-dir dir] [--token-file path] [--ladder id]
//!           [--fleet-db file [--release id] [--experiments] [--ours name,name]]
//!           [--prefer-address ip,ip] [--requeue-delay-ms N]
//!   --stdio --provisional: before each decision, also write a quick fallback turn marked
//!           "provisional": true (the gateway holds it in case the real one is late)
//!   marshal --bot <name> --stdio      read server frames on stdin, write replies on stdout (tests)
//!
//! The token comes from ARENA_TOKEN, else from --token-file, else from <repo>/bots/<name>.
//! It is never printed or logged. SIGTERM or Ctrl-C finishes the current match, then exits.
//!
//! With --fleet-db every match is written to the fleet's shared results store, and with
//! --experiments the playbook for a target opponent comes from the fleet's experiment plan.
//!
//! Exit codes: 0 done or stopped, 1 a ladder error that reconnecting cannot fix, 2 bad
//! arguments or no token, 3 another process is already running this bot.

use marshal::session::{Config, Out, Playbooks, Session};
use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

struct Args {
    bot: String,
    version: String,
    url: String,
    stdio: bool,
    matches: Option<u32>,
    playbooks: Option<PathBuf>,
    log_dir: Option<PathBuf>,
    token_file: Option<PathBuf>,
    ladder: Option<String>,
    fleet_db: Option<PathBuf>,
    release: Option<String>,
    experiments: bool,
    ours: Vec<String>,
    prefer: Vec<std::net::IpAddr>,
    requeue_delay_ms: u64,
    provisional: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        bot: String::new(),
        version: std::env::var("ARENA_VERSION").unwrap_or_else(|_| "v1".into()),
        url: std::env::var("ARENA_URL").unwrap_or_else(|_| "wss://victrisarena.com/bot".into()),
        stdio: false,
        matches: std::env::var("ARENA_MATCHES").ok().and_then(|s| s.parse().ok()),
        playbooks: None,
        log_dir: None,
        token_file: None,
        ladder: std::env::var("ARENA_LADDER").ok(),
        fleet_db: None,
        release: None,
        experiments: false,
        ours: Vec::new(),
        prefer: std::env::var("ARENA_PREFER_ADDRESS").map(|t| fleet::net::parse_addresses(&t)).unwrap_or_default(),
        requeue_delay_ms: 0,
        provisional: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--bot" => a.bot = val()?,
            "--version" => a.version = val()?,
            "--url" => a.url = val()?,
            "--matches" => a.matches = Some(val()?.parse().map_err(|_| "--matches needs a number")?),
            "--playbooks" => a.playbooks = Some(val()?.into()),
            "--log-dir" => a.log_dir = Some(val()?.into()),
            "--token-file" => a.token_file = Some(val()?.into()),
            "--ladder" => a.ladder = Some(val()?),
            "--stdio" => a.stdio = true,
            "--fleet-db" => a.fleet_db = Some(val()?.into()),
            "--release" => a.release = Some(val()?),
            "--experiments" => a.experiments = true,
            "--provisional" => a.provisional = true,
            "--prefer-address" => a.prefer = fleet::net::parse_addresses(&val()?),
            "--requeue-delay-ms" => a.requeue_delay_ms = val()?.parse().map_err(|_| "--requeue-delay-ms needs a number")?,
            "--ours" => a.ours = val()?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if a.bot.is_empty() {
        return Err("--bot <name> is required".into());
    }
    Ok(a)
}

/// The repository root: two levels above this workspace's `crates/`, found from the executable.
fn repo_root() -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_default();
    // <repo>/field-marshal/target/<profile>/marshal
    for _ in 0..4 {
        p.pop();
    }
    p
}

fn read_token(a: &Args) -> Result<String, String> {
    if let Ok(t) = std::env::var("ARENA_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    let path = a.token_file.clone().unwrap_or_else(|| repo_root().join("bots").join(&a.bot));
    std::fs::read_to_string(&path)
        .map(|s| s.split_whitespace().collect::<String>())
        .map_err(|e| format!("no token: set ARENA_TOKEN or put it in {} ({e})", path.display()))
}

/// One process per bot: a second connection with the same token throws the first one off
/// the ladder ("replaced"), so a bot name is held with a file lock for as long as we run.
fn lock_bot(bot: &str) -> Result<std::fs::File, String> {
    let dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(".victris").join("run");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{bot}.lock"));
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(_) => Err(format!("{bot} is already running (lock {})", path.display())),
    }
}

fn run_stdio(mut session: Session, provisional: bool) {
    let stdin = std::io::stdin();
    if provisional {
        session.early = Some(Box::new(|frame: String| {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{frame}");
            let _ = out.flush();
        }));
    }
    // Not locked for the whole run: the fallback above writes in the middle of a decision.
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        for o in session.handle(&line) {
            match o {
                Out::Send(s) => {
                    let _ = writeln!(out, "{s}");
                    let _ = out.flush();
                }
                Out::RequeueAfter(_) => {
                    // Behind a pipe the other side owns the connection: say what we would do.
                    let _ = writeln!(out, "{}", session.queue_frame());
                    let _ = out.flush();
                }
                Out::Reconnect => {
                    let _ = writeln!(out, r#"{{"type":"reconnect"}}"#);
                    let _ = out.flush();
                }
                Out::Done | Out::Fatal(_) => return,
            }
        }
    }
}

fn set_timeouts(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, read: Duration) {
    let tcp: Option<&TcpStream> = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::NativeTls(s) => Some(s.get_ref()),
        _ => None,
    };
    if let Some(s) = tcp {
        let _ = s.set_read_timeout(Some(read));
        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_nodelay(true);
    }
}

enum Closed {
    Done,
    Fatal(String),
    Dropped(String),
    /// closed by us after a slow match, to try another address
    Slow(Option<std::net::IpAddr>),
}

/// One connection: queue, then answer frames until the socket closes or we are done.
fn run_connection(session: &mut Session, url: &str, token: &str, stop: &AtomicBool, prefer: &[std::net::IpAddr], avoid: &[std::net::IpAddr]) -> Closed {
    // The address is ours to choose: the ladder's edge addresses are not equally fast.
    let (mut ws, remote) = match fleet::net::connect(url, token, prefer, avoid) {
        Ok(x) => x,
        Err(e) if e.starts_with("bad url") || e.starts_with("token has") => return Closed::Fatal(e),
        Err(e) => return Closed::Dropped(e),
    };
    session.remote = Some(remote.to_string());
    set_timeouts(&mut ws, Duration::from_secs(1));
    if let Err(e) = ws.send(Message::Text(session.queue_frame())) {
        return Closed::Dropped(format!("send failed: {e}"));
    }
    let mut last_heard = Instant::now();
    let mut last_ping = Instant::now();
    // when to send `queue` again after the server refused it
    let mut requeue_at: Option<Instant> = None;
    loop {
        if let Some(at) = requeue_at {
            if session.in_match() {
                requeue_at = None;
            } else if Instant::now() >= at && !stop.load(Ordering::Relaxed) {
                requeue_at = None;
                if let Err(e) = ws.send(Message::Text(session.queue_frame())) {
                    return Closed::Dropped(format!("send failed: {e}"));
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            session.stop_after_match = true;
            if !session.in_match() {
                let _ = ws.send(Message::Text(r#"{"type":"leave"}"#.into()));
                let _ = ws.close(None);
                return Closed::Done;
            }
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                last_heard = Instant::now();
                for o in session.handle(&text) {
                    match o {
                        Out::Send(s) => {
                            if let Err(e) = ws.send(Message::Text(s)) {
                                return Closed::Dropped(format!("send failed: {e}"));
                            }
                        }
                        Out::RequeueAfter(wait) => requeue_at = Some(Instant::now() + wait),
                        Out::Reconnect => {
                            let _ = ws.send(Message::Text(r#"{"type":"leave"}"#.into()));
                            let _ = ws.close(None);
                            return Closed::Slow(Some(remote));
                        }
                        Out::Done => {
                            let _ = ws.close(None);
                            return Closed::Done;
                        }
                        Out::Fatal(code) => {
                            let _ = ws.close(None);
                            return Closed::Fatal(code);
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => return Closed::Dropped("server closed the connection".into()),
            Ok(_) => last_heard = Instant::now(), // ping, pong or binary
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                // Quiet is normal while queued. Probe the connection, and give up on a dead one.
                if last_heard.elapsed() > Duration::from_secs(60) {
                    return Closed::Dropped("no traffic for 60 s".into());
                }
                if last_ping.elapsed() > Duration::from_secs(20) {
                    last_ping = Instant::now();
                    if let Err(e) = ws.send(Message::Ping(Vec::new())) {
                        return Closed::Dropped(format!("ping failed: {e}"));
                    }
                }
            }
            Err(e) => return Closed::Dropped(format!("read failed: {e}")),
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("marshal: {e}");
            std::process::exit(2);
        }
    };
    let playbooks = match &args.playbooks {
        None => Playbooks::default(),
        Some(p) => match std::fs::read_to_string(p).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
            Ok(v) => Playbooks::from_json(&v),
            Err(e) => {
                eprintln!("marshal: cannot read playbooks {}: {e}", p.display());
                std::process::exit(2);
            }
        },
    };
    let cfg = Config {
        bot: args.bot.clone(),
        version: args.version.clone(),
        ladder_id: args.ladder.clone(),
        playbooks,
        log_dir: args.log_dir.clone(),
        max_matches: args.matches,
        requeue_delay: Duration::from_millis(args.requeue_delay_ms),
        strategist: args.fleet_db.as_ref().map(|db| {
            Box::new(marshal::fleet_hook::FleetHook::new(db.clone(), &args.bot, &args.version, args.release.clone(), args.experiments, args.ours.clone()))
                as Box<dyn marshal::session::Strategist>
        }),
    };
    let mut session = Session::new(cfg);
    if args.stdio {
        run_stdio(session, args.provisional);
        return;
    }
    let token = match read_token(&args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("marshal: {e}");
            std::process::exit(2);
        }
    };
    let _lock = match lock_bot(&args.bot) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("marshal: {e}");
            std::process::exit(3);
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed));
    }
    eprintln!("[{}] starting as {} on {}", args.bot, args.version, args.url);
    // Reconnect and queue again; the ladder resumes a match in progress by sending hello and
    // the current observation. A connection that had been working is replaced at once: a
    // match keeps running at about 13 ticks a second while we are away, so every second of
    // waiting is a dozen turns not played. Repeated failures back off (1 s, doubling to 30 s).
    let mut delay = Duration::ZERO;
    // addresses that were slow for us, tried last from now on
    let mut avoid: Vec<std::net::IpAddr> = Vec::new();
    loop {
        let started = Instant::now();
        match run_connection(&mut session, &args.url, &token, &stop, &args.prefer, &avoid) {
            Closed::Slow(remote) => {
                if let Some(ip) = remote {
                    if !avoid.contains(&ip) {
                        avoid.push(ip);
                    }
                    // Every address has been slow once: start over and judge them afresh.
                    if avoid.len() >= 4 {
                        avoid.clear();
                    }
                }
                continue;
            }
            Closed::Done => {
                eprintln!("[{}] done: {} played, {}W {}L {}D", args.bot, session.played, session.wins, session.losses, session.draws);
                return;
            }
            Closed::Fatal(code) => {
                session.note("fatal", serde_json::json!(code));
                std::process::exit(1);
            }
            Closed::Dropped(why) => {
                if stop.load(Ordering::Relaxed) && !session.in_match() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(30) {
                    delay = Duration::ZERO; // it was a working connection
                }
                session.note("disconnected", serde_json::json!({ "why": why, "retry_in_s": delay.as_secs(), "in_match": session.in_match() }));
                std::thread::sleep(delay);
                delay = (delay * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
            }
        }
    }
}
