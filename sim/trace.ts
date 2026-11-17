// Prints one self-play game tick by tick: tsx sim/trace.ts <seed> [my seat] [opponent module]
import { Game } from "./arena-sim.mjs";
import { decide } from "../foot-soldier/strategy.ts";
const seed = Number(process.argv[2] ?? 0), mySeat = Number(process.argv[3] ?? 1);
const opp = (await import(process.argv[4] ?? "./opponents/v1.ts")).decide as typeof decide;
const game: any = new Game({ seed });
const mk = (h: any) => ({ id: h.match_id, rules: h.rules, map: h.map, pads: h.pickups });
const m: any = { 1: mk(game.hello(1)), 2: mk(game.hello(2)) };
console.log("map", game.hello(1).map_name);
while (!game.over) {
  const r: any = {};
  for (const id of [1, 2]) {
    const o = game.next(id);
    r[id] = (id === mySeat ? decide : opp)(o, m[id]);
    if (id === mySeat) {
      const ev = (o.events ?? []).filter((e: any) => !["projectile_spawn", "attack"].includes(e.kind)).map((e: any) => e.kind + (e.damage ? ":" + e.damage : "") + (e.target ?? e.fighter ?? "")).join(",");
      console.log(o.tick, "me", o.self.pos.join(","), o.self.hp, "en", o.enemies[0] ? o.enemies[0].pos.join(",") + " " + o.enemies[0].hp : "-", JSON.stringify(r[id]), ev);
    }
  }
  game.step(r);
}
console.log(JSON.stringify(game.result));
