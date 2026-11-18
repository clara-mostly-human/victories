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
- `data/status.html`: the same as a page, rewritten every five minutes on the Mac.
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
