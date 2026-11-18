# The fleet

Everything around the bots that is not playing the game: running them, planning experiments,
keeping results, shipping releases, collecting replays, and noticing when a bot is down.

One tool does all of it: `fleet` (source in `field-marshal/crates/fleet`, build with
`cargo build --release -p victris-fleet` in `field-marshal/`). The agents under launchd run a
copy at `data/bin/fleet`, which `fleet install ...` refreshes, so a rebuild never changes a
running agent. `fleet help` lists every command.

State lives in `data/` (not in git): `data/fleet.db` is the store, `data/releases/` the frozen
builds, `data/run/` pid files. Per-bot settings are in `fleet/bots.json` (in git, no secrets).

## Where to look

- `fleet status` and `fleet online`: every bot, the live release, experiments, load and think times.
- `http://Claras-Mac-mini.local:8787/` (or the Mac's address on the home network, port 8787):
  the status page, live. It lists every game as it ends (bot, result, opponent with rating
  and rank, map, seat, length, our rating after, damage, experiment arm, what the ladder
  charged per turn; `/?bot=<name>` shows one bot), the bots, the experiment arms also split
  by opponent strength, the releases and the alert log. It replaces looking at the Victris
  site from home. `com.victris.fleet-status` (`fleet serve`) builds it from the store
  on every request, so it is never older than the last finished game, and it reloads itself
  every 20 seconds. It only reads the store and talks to nothing outside the Mac.
- `data/status.html`: the same page as a file, rewritten every five minutes.
- The hosted page (a claude.ai artifact) shows the same numbers from one document, written
  by `fleet report` to `data/status/state.json` and pushed by a Claude session every ten
  minutes. It deliberately does not ask the ladder for anything: a browser on the home
  network loading ladder data is the leading suspect for the bots' dropped connections.
- `fleet digest`: the last 24 hours in a few lines. A scheduled task in the Claude app
  (`victris-fleet-daily-digest`, every morning at about 08:10 local) runs it and pushes the
  first line to the phone.
- `logs/fleet-alerts.log` (`fleet alerts`): `ALERT` and `RECOVERED` from the watchdog, `LOAD` from
  the load guard, `PIPELINE` from the autopilot. If `fleet/alert-hook.sh` exists and is
  executable, every `ALERT` and `RECOVERED` also runs it with the kind and the text as its two
  arguments; that is the place to send alerts to a phone without any Claude session running.

## Who runs what

| Bot | Run by | The fleet |
| --- | --- | --- |
| field-marshal | the search thread. Its launchd wrapper runs `data/releases/live/marshal`, the release `fleet release promote` last set. | builds its releases, keeps the live pointer, reads its results, watches that it is online. Never starts, stops or restarts it, and never uses its token. |
| foot-soldier | `fleet run foot-soldier` under launchd | the canary: always carries the newest release allowed on the ladder and takes no experiment arms, so its record is the record of one release. (The TypeScript bot in `foot-soldier/` is retired and only kept as an arena opponent.) |
| shock-trooper, quarter-master, spy-master | `fleet run <bot>` under launchd | scouts: run the live release and take arms from the experiment plan. |

A bot is `managed` when the fleet supervises it. Adding a scout later is: register the bot,
put its token in `bots/<name>`, add it to `fleet/bots.json` with `"managed": true`, then
`fleet deploy <name> <release>` and `fleet install <name>`.

## Agents on the Mac

| Agent | What it does |
| --- | --- |
| `com.victris.fleet-online` | Offline watchdog and load guard. Asks the ladder once a minute whether each bot with `expect_online` is online. After two minutes offline it writes an `ALERT` line to `logs/fleet-alerts.log`, shows a Mac notification, and repeats every 15 minutes until a `RECOVERED` line. Read-only. |
| `com.victris.fleet-collector` | Replay collector, see below. |
| `com.victris.fleet-watch` | Every 5 minutes: loads new games from bot logs into the store, and kills a managed bot that has been silent for 20 minutes so its supervisor restarts it. Once a day it writes a snapshot of the store to `data/backup/` (the newest seven are kept) and compresses tick logs of managed bots that are more than two days old. |
| `com.victris.<bot>` | Supervisor of one managed bot (`fleet run <bot>`). |

**Live bots come first.** The M4 has four fast cores and each live bot needs one, so everything
else here is batch work and stays out of the way: duels, the arena gate, release builds, the
collector and the health pass all run under `nice -n 15`; a gate uses one worker (two search
processes) unless told two, never more. On top of that the watchdog agent is a load guard:
every 15 seconds it looks at the load average and at each live bot's think time over the
last 90 seconds, and while either is too high (`"guard": {"load": 8, "think_p99_ms": 300}`
in `fleet/bots.json`) it keeps `data/run/batch.pause` in place. No duel starts a game while
that file exists. `fleet online` shows the load, the think times and whether batch work is
paused. Anything else long-running on this machine should check the same file.

`fleet status` shows all of it on one screen. `fleet alerts` prints the alert log.

## The network: what we learned and what the client does about it

- **One address for everything.** The bots share this network's single public address with
  every browser and script on it, and the ladder's edge judges them together. A Victris page
  open in a browser at home, or replays downloaded from this Mac, came with bursts in which
  every bot's connection was dropped at once (2026-11-18). So: nothing on the home network
  loads the Victris site or its replays while the bots play; the fleet itself sends the
  ladder nothing but the bots' own frames.
- **Connections differ in speed, for life.** Some connections are charged about 100 ms a
  turn and others 230 ms and more, with turns lost to the one-second limit, and a connection
  keeps its speed until it is replaced. It first looked like a property of the edge address,
  but the same address has since been both fast and slow, so no address is preferred
  (`prefer_addresses` in `fleet/bots.json` is empty). What works is measuring: the client
  records the address and what the ladder charged per turn in every match row
  (`fleet stats --by remote`, `--by connection`, `--by arm-remote`), and after a match on a
  slow connection (charge p50 over 180 ms with a fast engine, or two or more turns charged a
  full second) it reconnects before it queues again, up to four times in a row.
- **The standings, twice an hour.** Opponent ratings and ranks on the status page come from
  the `ladder` table, filled by one request for the ladder's standings every
  `ladder_refresh_minutes` (30; `fleet ladder` does it by hand). It is the only request the
  fleet makes to the ladder apart from the bots' own connections. A bot learns only its
  rating change from the ladder, so its rating after each game is carried forward from the
  standings, within one version (the ladder rates every version from scratch, which is why
  scouts keep one version label across deploys).
- **The first answer for a tick is final.** Tested live on 2026-11-18: an empty turn sent
  ahead of the real one was the one the ladder applied, 18 times of 18, and the second was
  ignored without complaint. So a bot cannot answer at once and correct itself later.
  Behind the gateway the engine instead writes a quick heuristic turn first, which the
  gateway holds and sends only if the real decision is not there after 800 ms.
- **Five bots, five match slots.** The account may play five matches at once. Every bot but
  the flagship waits `queue_gap_ms` (one second) after a match before queueing, so the
  flagship gets a freed slot first, and a refused queue is retried after a few seconds.

## The gateway

The design is Mike's (2026-11-18): **one process talks to the ladder, and nothing else on
this machine does.** `fleet gateway` runs under launchd (`com.victris.fleet-gateway`,
restarted within a second if it dies) and holds one WebSocket per bot whose entry has
`"gateway": true`. The programs that decide turns are separate local processes ("bot
processes": `marshal --stdio`). Each attaches to the gateway over a Unix socket,
`data/run/gateway.sock`, which becomes its standard input and output. A bot process can be
stopped, replaced or crash and the bot's connection to the ladder stays up.

    fleet health                  # the one verdict, and a line per bot
    fleet health --json           # everything the gateway knows (data/run/gateway.json)
    fleet install gateway         # load or reload the gateway's agent
    fleet install <bot>           # the bot's supervisor: starts its bot process, attached
    fleet attach <bot> -- <cmd>   # attach a bot process the fleet does not start (the flagship)

What the gateway owns:

- **The sockets.** It connects one at a time for the whole fleet (2 s apart, inside the
  comms budget), with backoff (1 s doubling to 30 s after a failed or short-lived
  connection). It tests each socket: three pings when it opens (the round trip is logged;
  with `"ping_reject_ms"` set, a slow one is dropped at once), and in play the wire time
  of every turn, which is what the ladder charged us less what the bot process took. A
  socket that loses two turns on the wire in a match is replaced in play (the match
  resumes on the new one). Otherwise a slow socket is only given up when a reconnect is
  cheap: at connect (ping over `"ping_reject_ms"`, 120 now: up to three tries, at 0, 2 and
  5 s), with one quick ping when a match is being resumed (two tries), and between
  matches after two slow pings. All of it sits under one cap, `"replacements_per_10min"`
  (8), across the whole fleet: when the cap is spent a bot keeps the socket it has. About
  half of all new sockets are slow whatever we do (ping 150 to 260 ms and 300 to 450 ms
  on the wire per turn, against 40 to 65 and 70), and the ladder resets a socket every
  few minutes anyway; an eager version of this test reconnected 33 times in four minutes
  on 2026-11-18 and left most bots on slow sockets. Replacing in play for slowness alone
  is off unless `"slow_wire_ms"` is set.
- **The ladder's protocol**, as the ladder's guide says. After a match it queues again. On
  `quota_exceeded` the bot stays queued and waits: no second `queue`, no reconnect. After
  a reconnect it queues as usual and the ladder resumes a match in progress (the match
  start, then the current observation); turns held for the old socket are never replayed.
  A refused version label (`version_limit`) is bumped in the store and queued under the
  next label on the same socket. A fatal refusal halts that bot only.
- **A queue per bot process.** Frames wait in the process's own queue; a newer
  observation replaces stale ones; a process that has not read for three seconds is
  detached. If the process is late (800 ms) the gateway sends its quick fallback turn, or
  an empty turn; if no process is attached it says `ready` and plays idle. Silence never
  forfeits a match. Thirty late answers in a row and the process is let go, so its
  supervisor starts a fresh one.
- **The truth, and resync.** The gateway knows the connection, the match, the tick and
  the label. A bot process that attaches during a match is sent the match start and the
  latest observation. A build of client level 4 also gets `status` frames (on change and
  once a second) and a heartbeat (`ping`, answered with `pong` and the match and tick the
  process believes); a process that disagrees twice is set right with `sync` and the
  same replay.
- **Releases and labels.** `fleet deploy <bot> <release>` only writes the store. After
  the bot's current match the gateway lets the old process go, the supervisor starts the
  new release, it attaches, and the gateway queues. The socket never closes.
- **Health.** Each bot has one verdict, from turns flowing: `playing`, `waiting for a
  match, N s`, `stalled N s: <cause>`, `no bot process attached`, `socket down (...)`. A
  bot queued two minutes without a match is logged as `STALL` with the cause (the quota
  the ladder named, or "queued on a live socket"), and `STALL over` when it plays. The
  fleet's verdict (`HEALTH ok` / `HEALTH degraded: ...`) goes to `logs/fleet-alerts.log`
  on every change and is the first line of the status page.

Settings, in `fleet/bots.json`: `"comms"` (connections a minute, queue requests a minute,
matches a day) and `"gateway"` (`connect_gap_ms`, `answer_deadline_ms`, `slow_wire_ms`,
`ping_reject_ms`, `replace_gap_ms`, `max_concurrent`, `stall_after_ms`).

Standings (opponents' ratings, our ranks) are fetched on GitHub every five minutes
(`.github/workflows/standings.yml`) and read from the `standings` branch; replays come the
same way (`corpus.yml`, `fleet collect import`). The machine makes no HTTP request to the
ladder.

To test a change to the gateway without touching the ladder: `fleet mock-ladder` is a
stand-in ladder (slow sockets, dropped connections, quota and label refusals on request)
and `fleet mock-engine` a stand-in bot process; run them in a throwaway root with
`VICTRIS_ROOT`.

## The autopilot

`com.victris.fleet-autopilot` runs the pipeline by itself, one step a minute, and writes a
`PIPELINE` line for everything it does:

1. A commit on `main` that changes `field-marshal/crates/bot`, `crates/engine` or `playbooks`
   is built into a release about two minutes after it lands.
2. The newest build plays the live release in the arena gate, as a low-priority child under
   the load guard. A build with the same search as the live one (only `session.rs`,
   `main.rs`, `fleet_hook.rs` or `lib.rs` differ) skips the gate: the arena could not tell
   them apart.
3. The newest build that passed, or was promoted, goes on the canary.
4. Ladder evidence for what is on the canary is worked out every ten minutes.
5. Scouts are moved to a fleet build of the live release's commit, but never while an
   experiment is running.

It never promotes and never touches the flagship. Promotion is a person's or the search
thread's step: `fleet release promote <id>` after the evidence passed, or with
`--force "<reason>"` as the owner's own call. Promote moves `data/releases/live` and
`data/releases/live.env` (`RELEASE=<id>`) by rename, so a restart never lands between two
releases.

## The store

`data/fleet.db` (SQLite, WAL). Every match of every bot is one row in `matches`: bot, version,
release, opponent and its version and rating, map, seat, playbook, experiment arm, result,
rating change, think times. Rows come from three places: a managed bot writes its own at
`hello` and at `end`; `fleet ingest` reads `marshal` logs; the collector fills in what the
ladder's public list says (which is how foot-soldier's games and all opponent ratings arrive).

    fleet stats --by version                        # every bot and version
    fleet stats --by opponent --bot field-marshal --version v4
    fleet stats --by map --min-opp-rating 2400 --since 24h
    fleet compare field-marshal:v4 field-marshal:v3 # opponent by opponent, with a confidence

Games against our own bots are left out unless `--all` is given.

## Experiments

An experiment names target opponents and two or more arms. Each arm is a set of playbook
settings laid over the bot's own playbook for that opponent (`{}` is the control). When a bot
with `"experiments": true` draws a target, it gets the arm with the fewest games in that
opponent / map / seat cell, so arms stay balanced across maps and seats. The choice and the
match row are one transaction, so several bots can draw at once.

    # slam.json
    { "name": "slam-risk-vs-top", "opponents": ["StuartSutcliffe", "Shelbot*"], "games_per_arm": 60,
      "arms": { "control": {}, "risk8": { "slam_risk": 8 } } }

    fleet plan add slam.json
    fleet plan show slam-risk-vs-top     # record per arm and per opponent, balance per cell
    fleet plan add-arm slam-risk-vs-top risk2 '{"slam_risk": 2}'   # joins a running experiment
    fleet plan pause|resume|stop <name>

Two experiments can run at once. One against named opponents (no bare `"*"` in its
`opponents`) gets priority 1 and takes its opponents' games; every other game stays with the
wide experiment. `"priority"` in the file overrides this. Its baseline counts only games
against its own opponents.

With `"autopilot_moves_bots": false` in `fleet/bots.json` (the setting since 2026-11-18)
the autopilot still builds and gates each new bot commit, but it never moves a scout or
the canary by itself: a bot changes release only when a commit is named and someone runs
`fleet release canary <id>` and `fleet deploy <bot> <id>`, one bot at a time. Every move
is a restart and a new connection, and new connections close together came with resets.

The autopilot stops an arm early in two cases: two standard errors behind the control arm
after 100 games (control's games over the arm's own time), or three standard errors behind
the baseline that `plan show` prints after 30 games, provided it is also behind control. A
stopped arm keeps its games, and the remaining arms share evenly from then on.

A game in which our bot lost five or more turns to the connection (the server charged the
full 1000 ms) is left out of `plan show`, of `fleet stats --experiment`, and of the early
stop: it tests the connection, not the playbook. `plan show` prints how many were left out
per arm; `--with-lost` puts them back. The quota still counts every game.

`plan show` reads each arm against a baseline over the arm's own time: the control arm, plus
games of our bots outside any experiment that played the default playbook. The default file
can play more than one named playbook (matrix-5, and matrix-6 against bots rated 2250 and
up), so the names are a list: `"baseline_playbooks"` in `fleet/bots.json`, or
`--baseline-playbook a,b` for one reading. `--control-only` keeps to the control arm. For
the closed `tempo-on-off`, whose default was matrix-5 alone, pass `--baseline-playbook matrix-5`.

In the client this is the `Strategist` trait in `crates/bot/src/session.rs`: asked once at
`hello`, told the result at `end`. `marshal --fleet-db <file> [--experiments]` plugs the fleet
in; without the flag a bot plays its playbooks file exactly as before. The search code never
sees any of this, only the playbook.

## Releases

A release is a frozen build of one commit (`marshal`, `arena`, the playbooks file) in
`data/releases/<id>/`. Uncommitted work is never part of one.

    fleet release build [--ref HEAD]          # export the commit, build, run the tests
    fleet release gate <id> [--against <id>]  # arena: build against build until decided
    fleet release canary <id>                 # put it on the ladder with the canary and scout bots
    fleet release evidence <id>               # its ladder games against the live release's
    fleet release promote <id>                # make it live; deploys to managed flagships
    fleet release list

- The arena gate plays the candidate against the live release with `fleet/duel.mjs`: two
  `marshal --stdio` processes, the official simulator as referee, the ladder's time limits,
  every seed played from both seats. A sequential test stops it as soon as the candidate is
  shown to score 56% or shown not to beat 50% (`--s0`, `--s1`, `--games`, `--workers`).
- Arena wins overstate ladder gains, so a gated release then has to earn ladder evidence on
  the canary: at least 60 games, at least 5 opponents in common with the baseline, and 90%
  confidence that it scores better opponent by opponent.
- `deploy` and `promote` refuse a release that has not come that far. `--force "<reason>"`
  and `fleet release mark <id> <status> --reason ...` override, and are logged in `events`.
- A deploy is a hot swap: the store is changed and the bot gets SIGTERM. It finishes its
  match and exits; the supervisor starts the new release under the next version label.

## Supervisor

`fleet run <bot>` keeps one bot alive: clean exit means start again at once, a crash restarts
with a growing delay, a fatal ladder error (bad token, replaced) is retried after two minutes
and halts the bot after three in an hour, `fleet stop <bot>` halts it until `fleet resume`.
`marshal` holds a lock per bot name (`~/.victris/run/<bot>.lock`), so a second process for
the same token is refused instead of knocking the first one off the ladder.

## Replay corpus

The collector watches the ladder's public match list and downloads replays one at a time, at
most one request every five seconds: our own games first, then games of the top 15 bots, then
games between bots rated 2000 and up, then the rest. A 403 or 429 pauses it for ten minutes,
doubling up to two hours. The ladder plays far more games than that pace can fetch, so queued
games that were not reached are let go: other bots' after a day, top bots' after three, ours never. Every replay is re-simulated on the exact engine before it is
accepted; one that does not match goes to `replays/rejected/` and raises an event. Accepted
replays are `replays/corpus/<match id>.jsonl.gz`, indexed in the `replays` table with both
bots' names, versions and ratings.

    fleet corpus --count
    fleet corpus --min-rating 2400            # paths, one per line, newest first
    fleet corpus --bot StuartSutcliffe --limit 500
    fleet corpus --ours --since 24h
    field-marshal/target/release/conformance fit-eval $(fleet corpus --min-rating 2400 --limit 2000)

`fleet collect index` verifies and indexes replays that were put on disk some other way.
