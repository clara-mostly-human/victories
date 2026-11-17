// Local evaluation: plays bot/strategy.ts decide() against simulator opponents.
//   npx tsx sim/eval.ts [games-per-opponent]
import { Game } from "./arena-sim.mjs";
import { decide, type Match } from "../foot-soldier/strategy.ts";

const N = Number(process.argv[2] ?? 100);
for (const opponent of ["random", "ts-starter"]) {
  let win = 0, loss = 0, draw = 0, rejected = 0, failures = 0;
  const reasons: Record<string, number> = {};
  for (let seed = 0; seed < N; seed++) {
    const game = new Game({ seed, opponent });
    const hello = game.hello(1);
    const match: Match = { id: String(seed), rules: hello.rules };
    while (!game.over) {
      const obs = game.next(1);
      for (const e of obs.events ?? []) {
        if (e.kind === "rejected" && e.fighter === undefined || e.kind === "rejected") {
          rejected++;
          reasons[`${e.part}:${e.reason}`] = (reasons[`${e.part}:${e.reason}`] ?? 0) + 1;
          if (e.part === "(message)") failures++;
        }
      }
      let turn = {};
      try { turn = decide(obs, match); } catch (err) { failures++; }
      game.step({ 1: turn });
    }
    const end = game.next(1);
    if (end.result === "win") win++; else if (end.result === "loss") loss++; else draw++;
  }
  console.log(`vs ${opponent}: ${win}W ${loss}L ${draw}D over ${N} | rejected parts ${rejected} | protocol failures ${failures}`, reasons);
}
