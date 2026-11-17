// Local evaluation: plays foot-soldier/strategy.ts against an earlier strategy, both seats per seed.
//   foot-soldier/node_modules/.bin/tsx sim/eval.ts [seeds] [opponent module, default ./opponents/v1.ts]
// Also plays the simulator's built-in Random as a sanity check.
import { Game } from "./arena-sim.mjs";
import { decide } from "../foot-soldier/strategy.ts";

const N = Number(process.argv[2] ?? 100);
const oppPath = process.argv[3] ?? "./opponents/v1.ts";
const opp = (await import(oppPath)).decide as typeof decide;

const newMatch = (hello: any) => ({ id: hello.match_id, rules: hello.rules, map: hello.map, pads: hello.pickups });
type Tally = { win: number; loss: number; draw: number; rejected: number; failures: number; ticks: number };
const reasons: Record<string, number> = {};

function play(seed: number, mySeat: 1 | 2, builtin: string | null, t: Tally): void {
  const game: any = builtin ? new Game({ seed, opponent: builtin }) : new Game({ seed });
  const seats: number[] = game.players;
  const brain: Record<number, { fn: typeof decide; match: any }> = {};
  for (const id of seats)
    brain[id] = { fn: builtin || id === mySeat ? decide : opp, match: newMatch(game.hello(id)) };
  const me = builtin ? 1 : mySeat;
  while (!game.over) {
    const replies: Record<number, unknown> = {};
    for (const id of seats) {
      const obs = game.next(id);
      if (id === me)
        for (const e of obs.events ?? [])
          if (e.kind === "rejected") {
            t.rejected++;
            reasons[`${e.part}:${e.reason}`] = (reasons[`${e.part}:${e.reason}`] ?? 0) + 1;
          }
      try {
        replies[id] = brain[id]!.fn(obs, brain[id]!.match);
      } catch (err) {
        if (id === me) { t.failures++; if (t.failures < 3) console.error(err); }
        replies[id] = {};
      }
    }
    game.step(replies);
  }
  const end = game.next(me);
  t.ticks += game.result.final_tick;
  if (end.result === "win") t.win++; else if (end.result === "loss") t.loss++; else t.draw++;
}

const fresh = (): Tally => ({ win: 0, loss: 0, draw: 0, rejected: 0, failures: 0, ticks: 0 });
const show = (name: string, t: Tally) => {
  const n = t.win + t.loss + t.draw;
  console.log(`vs ${name}: ${t.win}W ${t.loss}L ${t.draw}D over ${n} (${((100 * t.win) / n).toFixed(1)}%) | avg ticks ${Math.round(t.ticks / n)} | rejected ${t.rejected} | decide errors ${t.failures}`);
};

const rnd = fresh();
for (let seed = 0; seed < Math.min(N, 30); seed++) play(seed, 1, "random", rnd);
show("random", rnd);
const perMap: Record<string, Tally> = {};
const all = fresh();
for (let seed = 0; seed < N; seed++)
  for (const seat of [1, 2] as const) {
    const before = { ...all };
    play(seed, seat, null, all);
    const m = (perMap[["arena", "columns", "pillars"][seed % 3] ?? "?"] ??= fresh());
    m.win += all.win - before.win; m.loss += all.loss - before.loss; m.draw += all.draw - before.draw; m.ticks += all.ticks - before.ticks;
  }
show(oppPath, all);
for (const [k, t] of Object.entries(perMap)) show(`  map#${k}`, t);
console.log("rejected by reason", reasons);
