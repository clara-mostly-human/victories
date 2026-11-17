# Victris Arena bots

One directory per bot, named after the bot. Tokens live in `bots/<bot-name>` (gitignored, never commit).

- `foot-soldier/` — the first, exploratory bot (TypeScript, from the official starter). `strategy.ts` is the
  decision code, `bot.ts` the connection. It exists to learn the game and the ladder; the main bot will be
  written in a fast compiled language, because search depth inside the 1 s turn deadline is what wins.
- `sim/` — the official simulator (0.82.2) and `eval.ts`, a local win-rate check:
  `foot-soldier/node_modules/.bin/tsx sim/eval.ts 100`
- `run.sh <bot-name> [version]` — runs `<bot-name>/` on the ladder with the token from `bots/<bot-name>`.
- `deploy/` — launchd agent that keeps foot-soldier running on the Mac mini (`deploy/install.sh`).

Every strategy change gets a new version number (`v2`, `v3`, …) so its rating is tracked separately.
