# field-marshal

The main bot: a Rust workspace built around an exact native port of the Victris Arena game
engine. The same engine code will track the live match, simulate candidate ticks for the
search, and stand behind the offline tests.

Status (17 November 2026): steps 1 and 2 of the build order are built and tested locally. The
engine is ported and proven against the official one; the client, state tracker and a safe
heuristic play full games over a real WebSocket against local opponents. Nothing here has
connected to the ladder yet: that needs a registered bot and Mike's go-ahead.

## Layout

```
field-marshal/
  crates/engine/        victris-engine: the engine port and the observation rules (library, no I/O)
  crates/conformance/   conformance: replay verifier and benchmark (binary + tests)
  crates/bot/           marshal: state tracker, policy, session and the ladder client;
                        arena: two playbooks against each other, Rust only
  tools/fuzz-oracle.mjs random games played by the OFFICIAL engine, for the verifier
  tools/gauntlet.ts     local ladder: plays marshal over a WebSocket against local opponents
  tools/grow-corpus.py  downloads public ladder replays into ../replays/corpus
  tools/fetch-engine-src.sh  re-download the official TypeScript source for reference
  playbooks/default.json     which playbook to use against which opponent
  versions/             the version each bot queues as (versions/<bot>, else versions/default)
  run.sh                run one bot on the ladder: ./run.sh <bot-name>
```

Planned next: `crates/search` (simultaneous-move search, shared by every bot).

## The engine port

`victris-engine` reproduces official engine 0.23.0 (`packages/engine/src/*.ts`, shipped in the
simulator's source map; a copy lives in `../engine-src/`, which is not in git).

- `Engine` holds the immutable part of a match: the rules table and the map, with lookup
  tables for blocked steps and a fill-on-first-use line-of-sight cache. Build it from the
  `rules` and `map` in `hello`, or from a replay header. Nothing is hard-coded to rules 0.22.1.
- `State` is flat and `Copy` (1,448 bytes): two fighters, fixed arrays for projectiles, areas
  and pads. A search clones it with a memcpy.
- `engine.step(&mut state, &[turn1, turn2])` resolves one tick in the official eight phases.
  It returns a small `TickSummary` (hp lost, wall slams) for scoring.
- `RawTurn` models what a bot sent, including malformed turns, so validation matches the
  server's for any input.
- `engine.state_hash(&state)` is the ladder's per-tick replay hash (cyrb53 over canonical JSON).

`victris_engine::observe` ports what a fighter is told each tick: fog of war (sight radius and
line of sight), the legal steps and dashes, pickups in view, the zone warning and the danger
tiles. `View::from_state` builds it from a state and `View::from_json` reads it from the
server's message; comparing the two is how a bot checks its predictions.

`victris_engine::events` ports the event list a tick produces and the rule for which events
each fighter is told about. `Engine::step_events` records them; `Engine::step` (the search's
path) does not. They are checked against the official engine together with the observations.

Limits of the fixed arrays: 16 projectiles, 6 areas, 8 pads, 4 attacks. The shipped rules on
21x21 maps peak at about 8 projectiles and 2 areas. Exceeding a limit panics with a clear
message instead of silently dropping anything.

## Proof of exactness

Every check compares the full state after every tick, through the official state hash.

| Check | Games | Ticks | Mismatches |
| --- | --- | --- | --- |
| Ladder replays in `../replays/corpus` (hashes written by the ladder server) | 1,287 | 222,521 | 0 |
| Fuzz against the official engine (`tools/fuzz-oracle.mjs`, seeds 11 to 14) | 200,000 | 39,012,244 | 0 |
| Observations and their event lists against the official engine (`--observations`) | 6,600 | 2,562,205 observations | 0 |

The fuzz oracle plays games through the official engine with rough turns: legal play in
several styles plus every kind of malformed turn the validator has a rule for. 30% of games
use a random map (11 to 25 tiles a side, random walls, spawns and pads) and 20% use randomly
changed rules numbers, so the port is checked beyond what the ladder currently runs. With
`--states` the oracle also writes the full state, and the verifier shows the first differing
field.

The harness was itself tested by injecting twelve deliberate one-line bugs (rounding, stun
length, buff expiry, swap rule, bounce rule, zone timing, shout cooldown, dead-fighter timers,
repulsor ordering and others). Eleven were caught. The twelfth changes a condition that can
never be true during a tick, so it is not a behaviour change.

```sh
cargo test --release                      # 60 official-engine games + the replay corpus if present
cargo build --release
target/release/conformance verify ../replays/corpus
node tools/fuzz-oracle.mjs --games 20000 --seed 1 | target/release/conformance verify -
node tools/fuzz-oracle.mjs --games 20000 --seed 1 --only 137 --states | target/release/conformance verify -   # debug one game
target/release/conformance bench
```

The fuzz oracle needs Node and `../sim/arena-sim.mjs` (the official simulator).

## Speed

Measured on the Mac mini (Apple M4, one performance core), playing exactly the same random
games as `analysis/bench.mjs` does on the official engine:

| | Official engine (Node 26) | This port | Ratio |
| --- | --- | --- | --- |
| Ticks per second | 393,000 | 3,700,000 | 9.5x |
| Ticks per second, copying the state every tick as a search does | 185,000 | 3,400,000 | 18x |

This is the straightforward port with no tuning. The projectile sweep still tests a small
bounding box of tiles per shot per tick, as the original does; precomputing each shot's path
is the obvious next gain if the search needs it.

## The bot

`marshal` is one process per bot. `Session` (crates/bot/src/session.rs) handles the protocol
independently of the transport, so the path tested locally is the path used on the ladder.

- State tracker (`tracker.rs`). `hello` carries the seed, so the bot knows the whole starting
  position. Each tick it tries every turn the opponent could have played, runs the exact engine,
  and keeps the one whose predicted observation equals the real one. A tick where nothing fits
  while the opponent is in view is a prediction mismatch: it is counted and logged with the
  match id. What cannot be predicted is allowed for and taken from the observation: an
  opponent's shot flying into view, and pads that were out of sight. Hidden even in view are the
  opponent's step and weapon cooldowns; the tracker infers them from what it saw. Out of view,
  it keeps the set of tiles the opponent could have reached.
- Policy (`policy.rs`, default playbook matrix-1). A heuristic gives every turn its own merits:
  staying in the zone, getting out of what is already in the air (planned over a frostburst's
  whole fuse), keeping off walls a repulsor could reach, taking cheap pickups. With the opponent
  in view the exchange is then solved as a matrix game: each of our turns against each movement
  the opponent could make, played on the exact engine and four ticks on, solved by regret
  matching. Out of view it takes the turn with the best worst case (the heuristic alone is
  playbook safe-2). About 0.2 ms a tick, so nearly the whole clock is still unused.
- Playbooks. Every number in the heuristic is a field of `Playbook`. `playbooks/default.json`
  names a default and per-opponent overrides; the playbook is chosen from the opponent's name in
  `hello` and written to the log with every match.
- Safety. A panic in the tracker or policy costs one idle tick, never the match. Rules or a map
  the engine cannot load put the match in safe mode (idle turns) with a loud log line. SIGTERM
  finishes the current match and exits. The connection reconnects with backoff and resumes a
  match in progress.
- Logs. `../logs/<bot>/<bot>-<date>.jsonl`: one line per tick (turn, why, tracker status,
  simulations, time, budget left) and one per match (opponent, playbook, result, damage by
  source, think-time percentiles, tracking counts).

### Local results (`tools/gauntlet.ts`)

The gauntlet runs the real binary over a real WebSocket, with the official simulator's match
core as the server, and can attach the true state to every observation (`--truth`).

| Opponent | safe-1 (first heuristic), 30 games each | safe-2 (tuned), 20 each | matrix-1 (search), 20 each |
| --- | --- | --- | --- |
| random (house) | 30 won, 0 lost | 20 won, 0 lost | 20 won, 0 lost |
| ts-starter (house) | 28 won, 2 lost | 20 won, 0 lost | 20 won, 0 lost |
| foot-soldier v1 | 28 won, 2 lost | 19 won, 1 lost | 20 won, 0 lost |
| foot-soldier v2 (ranked 67th on the ladder) | 16 won, 13 lost, 1 drawn | 14 won, 5 lost, 1 drawn | 17 won, 3 lost |

No timeouts, no refused frames, no rejected turn parts, no panics in any run. Decision time:
median 0.1 to 0.2 ms, worst 6 ms.

Tracking over the 120 safe-1 games: 37,082 ticks, zero prediction mismatches. The tracker's
own fighter always equalled the truth. With the opponent in view the whole state equalled the
truth except for what is hidden by design: the opponent's step and weapon cooldowns (wrong on
about one tick in ten, after it comes back into view or fires out of sight), pads out of sight,
and shots still out of sight.

### The arena

`arena` plays two playbooks against each other in Rust alone. Each side sees only what the
ladder would send it (its observation and its visible events) and runs the same tracker and
policy as the live bot. It plays about 200 games a second on 8 threads at the first settings,
with zero tracker mismatches in 1,800 games (about a million observations).

It already paid for itself: a sweep of 22 single-setting changes (1,200 games each) and two
rounds of combining the winners produced safe-2, which beats safe-1 92% to 8% in the arena and
carried over to the local opponents above. The biggest gains were dashing far less and not
discounting a turn for what the opponent might do if it guessed our tile. Self-play can
mislead, so every arena gain is confirmed against other bots before it ships.

The first search, matrix-1, beats safe-2 92% to 8% over 600 arena games. Two results from
building it need revisiting against stronger opponents: giving the opponent its attacks as
options made the search far too timid (39% against safe-2 instead of 90%), and sampling from
the mixed strategy did a little worse than always playing its most likely turn (84% against
90%). Both are playbook switches (`model_attacks`, `sample`).

```sh
target/release/arena --pairs 300 --seed 1 --a candidate.json        # 1,800 games against the default
```

```sh
../foot-soldier/node_modules/.bin/tsx tools/gauntlet.ts --games 120 --seed 11 --truth --log-dir /tmp/glog
cargo test --release -p marshal     # tracker regression on 8 recorded games
```

## Design: five bots, one library

The ladder allows five bots per owner. All five run from this workspace:

- One library, one binary. The engine (and later the search) is shared. `marshal` is started
  once per bot name with its own token (`../bots/<name>`), version (`versions/<name>`) and log.
  A crash or a bad experiment in one process cannot touch the others.
- Playbooks pick the behaviour per opponent, as above. The flagship plays the proven playbook;
  the other four take the experiments.
- Coordination (not built yet). A small shared store on disk holds the test plan and results.
  Whichever bot draws the target rival claims the next arm, balanced across maps and seats, and
  writes the outcome back. The per-match log line already records opponent, playbook and result.
- Cores. The M4 has four performance cores for up to five concurrent searches. The engine and
  maps are immutable and shareable, and the state is a plain value; how to split the cores
  between five processes is to be measured once the search exists.

Registering, enrolling or retiring bots changes the ladder account and needs Mike's go-ahead.

## Build order

1. Engine port and conformance. Done. The replay corpus stands at 1,287: the ladder host began
   refusing bulk downloads (HTTP 403) after about 700 files in one sitting, so growing it to
   5,000 has to be a slow trickle (`tools/grow-corpus.py`), not a sweep.
2. Client, state tracker and a safe heuristic. Built and tested locally. Left: 50 ladder games
   with no timeouts and no prediction mismatches, which needs a registered bot.
3. Matrix search, time manager and self-play arena. The arena and a first one-tick search exist.
   Next: use the clock (the search takes 0.2 ms of a 600 ms allowance), which means deeper
   lookahead and a time manager, and an opponent model that makes modelling attacks pay.
4. Simultaneous-move MCTS, parallel search and tuned evaluation.
5. Opponent models and the five-bot matchup-testing loop.
