# Victris Arena bots

One directory per bot, named after the bot. Tokens live in `bots/<bot-name>` (gitignored, never commit).

- `foot-soldier/` — the first, exploratory bot (TypeScript, from the official starter). `strategy.ts` is the
  decision code, `bot.ts` the connection. It exists to learn the game and the ladder; the main bot will be
  written in a fast compiled language, because search depth inside the 1 s turn deadline is what wins.
- `field-marshal/` — the main bot: a Rust workspace around an exact native port of the game engine, proven
  against ladder replay hashes and the official simulator. See `field-marshal/README.md`. Not on the ladder yet.
- `sim/` — the official simulator (0.82.2); `eval.ts` plays the current strategy against an earlier one
  (`sim/opponents/`) on both seats: `foot-soldier/node_modules/.bin/tsx sim/eval.ts 100`; `trace.ts` prints one
  game tick by tick; `ladder_stats.py <bot>` summarises recent ladder games. Note the simulator has only two
  spawn layouts per map, so against a deterministic opponent there are only about a dozen distinct games.
- `run.sh <bot-name>` — runs `<bot-name>/` on the ladder with the token from `bots/<bot-name>`, queueing as the
  version in `<bot-name>/VERSION`. To ship a change: bump VERSION and restart the process.
- `fleet/` — the tooling around the bots: shared results store, experiment plan, supervisor, release pipeline,
  replay collector and offline watchdog. See `fleet/README.md`; the tool is `fleet` (`field-marshal/crates/fleet`).
- `deploy/` — launchd agent that keeps foot-soldier running on the Mac mini (`deploy/install.sh`).

Every strategy change gets a new version number (`v2`, `v3`, …) so its rating is tracked separately.
