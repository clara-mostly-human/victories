#!/usr/bin/env node
// Fuzz oracle: plays random games through the OFFICIAL engine and writes them as replays,
// one after another, on stdout. Each tick line carries the official state hash (and, with
// --states, the full official state). `conformance verify -` re-simulates them with the
// Rust port and reports the first tick where the two engines disagree.
//
//   node tools/fuzz-oracle.mjs --games 2000 --seed 1 | target/release/conformance verify -
//
// Options:
//   --games N        how many games (default 200)
//   --seed S         generator seed (default 1); the same seed gives the same games
//   --states         also write the full state after every tick (big; for debugging)
//   --observations   also write what each fighter is told after every tick, to check the
//                    port's observation rules (fog of war, move options, danger tiles)
//   --random-maps P  share of games on a randomly generated map (default 0.3)
//   --vary-rules P   share of games with randomly changed rules numbers (default 0.2)
//   --only K         write only game number K of the run (to replay one failing game)
//   --stats          print how often each kind of engine event occurred, on stderr
//
// The turns are deliberately rough: legal play, plus every kind of malformed turn the
// validator has a rule for, so the port is checked in corners real bots rarely reach.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i < 0 ? dflt : Number(args[i + 1]);
};
const GAMES = opt("games", 200);
const SEED = opt("seed", 1);
const STATES = args.includes("--states");
const OBSERVATIONS = args.includes("--observations");
const P_RANDOM_MAP = opt("random-maps", 0.3);
const P_VARY_RULES = opt("vary-rules", 0.2);
const ONLY = opt("only", -1);

// The simulator bundle does not export the engine's internals, so make a copy that does.
const simPath = join(here, "../../sim/arena-sim.mjs");
const outDir = join(here, "../target/oracle");
mkdirSync(outDir, { recursive: true });
const xPath = join(outDir, "sim-x.mjs");
writeFileSync(
  xPath,
  readFileSync(simPath, "utf8") +
    "\nexport { step as __step, createMatch as __createMatch, stateHash as __stateHash, loadRules as __loadRules, loadMap as __loadMap, observe as __observe };\n",
);
const sim = await import(pathToFileURL(xPath).href);
const { __step: step, __createMatch: createMatch, __stateHash: stateHash, __loadRules: loadRules, __loadMap: loadMap, __observe: observe } = sim;
const { SHIPPED_RULES, shippedMap, rulesHash, ENGINE_VERSION } = sim;

// mulberry32
function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const DIRS = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
const VEC = { N: [0, -1], NE: [1, -1], E: [1, 0], SE: [1, 1], S: [0, 1], SW: [-1, 1], W: [-1, 0], NW: [-1, -1] };
const SHIPPED = ["arena-21", "columns-21", "pillars-21"];

function makeHelpers(rand) {
  const int = (n) => Math.floor(rand() * n);
  const range = (lo, hi) => lo + int(hi - lo + 1);
  const pick = (a) => a[int(a.length)];
  const chance = (p) => rand() < p;
  return { int, range, pick, chance };
}

function randomMap(h, id) {
  const { range, chance, int } = h;
  const w = chance(0.5) ? 21 : range(11, 25);
  const ht = chance(0.5) ? w : range(11, 25);
  const density = chance(0.2) ? 0 : rand01(h) * 0.25;
  const rows = [];
  for (let y = 0; y < ht; y++) {
    let row = "";
    for (let x = 0; x < w; x++) row += h.chance(density) ? "#" : ".";
    rows.push(row);
  }
  const floor = [];
  for (let y = 0; y < ht; y++) for (let x = 0; x < w; x++) if (rows[y][x] === ".") floor.push([x, y]);
  // Shuffle the floor tiles and deal out spawns and pads from the front.
  for (let i = floor.length - 1; i > 0; i--) {
    const j = int(i + 1);
    [floor[i], floor[j]] = [floor[j], floor[i]];
  }
  // Half the time put the spawns close together, so fights start at once.
  let spawns = [floor[0], floor[1]];
  if (chance(0.5)) {
    const near = floor.slice(1).find((t) => Math.max(Math.abs(t[0] - floor[0][0]), Math.abs(t[1] - floor[0][1])) <= 3);
    if (near) spawns = [floor[0], near];
  }
  const used = new Set(spawns.map((s) => s.join(",")));
  const pickups = [];
  const nPads = range(0, 6);
  for (const t of floor) {
    if (pickups.length >= nPads) break;
    if (used.has(t.join(","))) continue;
    used.add(t.join(","));
    pickups.push({ pos: t, kind: chance(0.5) ? "health" : "power" });
  }
  return { name: `fuzz-${id}`, tiles: rows, spawns, pickups };
}
const rand01 = (h) => h.int(1000) / 1000;

function variedRules(h) {
  const { range, chance } = h;
  const r = JSON.parse(JSON.stringify(SHIPPED_RULES));
  r.max_ticks = chance(0.5) ? 600 : range(60, 400);
  r.stats = { max_hp: range(20, 200), move_cooldown: range(1, 4), sight_radius: range(3, 12), armor: range(0, 3) };
  const speed = range(1, 4);
  // Keep the number of shots in flight within what the port's fixed arrays hold.
  const minCd = Math.ceil(52 / (speed * 6));
  r.attacks.ranged = { kind: "projectile", damage: range(1, 30), speed, cooldown: range(minCd, minCd + 6) };
  if (chance(0.6)) r.attacks.ranged.bounces = 1;
  if (chance(0.4)) r.attacks.ranged.range = range(2, 14);
  const fuse = range(3, 8);
  r.attacks.frostburst = { kind: "area", damage: range(1, 30), range: range(2, 10), cooldown: range(fuse, fuse + 10), fuse, radius: range(0, 3) };
  if (chance(0.7)) r.attacks.frostburst.slow = range(1, 3);
  r.attacks.repulsor = { kind: "burst", damage: range(0, 12), range: 1, cooldown: range(1, 14), push: range(1, 5) };
  r.dash = { distance: range(1, 5), cooldown: range(2, 14), recovery: range(0, 3) };
  r.knockback = { wall_slam_damage: range(0, 20), wall_slam_stun: range(0, 5), fighter_collision_damage: range(0, 10) };
  r.zone = {
    start_tick: range(5, 250), interval: range(5, 80), step: range(1, 3), min_radius: range(0, 3), warning_ticks: 20,
    damage_base: range(0, 8), damage_step: range(0, 8), damage_step_ticks: range(1, 30),
  };
  r.pickups = {
    first_spawn: range(0, 30),
    health: { amount: range(1, 60), respawn: range(1, 150) },
    power: { duration: range(1, 60), damage_multiplier: h.pick([[3, 2], [2, 1], [5, 3], [1, 2], [7, 4]]), respawn: range(1, 200) },
  };
  if (chance(0.25)) delete r.shout;
  else r.shout = { max_length: range(1, 40), cooldown: range(0, 12) };
  return r;
}

const sgn = (n) => (n > 0 ? 1 : n < 0 ? -1 : 0);
function dirTowards(from, to, away = false) {
  let dx = sgn(to[0] - from[0]);
  let dy = sgn(to[1] - from[1]);
  if (away) { dx = -dx; dy = -dy; }
  return DIRS.find((d) => VEC[d][0] === dx && VEC[d][1] === dy);
}

const BAD_DIRS = ["X", "n", "", 3, true, {}, ["N"], 0.5];
const BAD_TARGETS = [[1.5, 2], ["1", 2], [1, 2, 3], [1], "1,2", [1001, 0], [0, -1001], {}, 7, [null, 1], [1e21, 0], [true, false]];
const SHOUTS = [
  "hi", "gg", "", "   ", " ﻿", "\u0085", "\t\n", "x".repeat(40), "x".repeat(41), "😀".repeat(40), "😀".repeat(41),
  "é".repeat(12), 5, true, {}, ["a"],
];

/** One fighter's turn. `style` biases it; anything may be malformed. */
function genTurn(h, state, id, style, rules, map) {
  const { int, range, pick, chance } = h;
  const me = state.fighters.find((f) => f.id === id);
  const en = state.fighters.find((f) => f.id !== id);
  const garbage = style === "garbage" ? 0.35 : 0.03;
  if (chance(garbage * 0.3)) return pick([null, undefined, "idle", 5, [], true, {}]);
  const t = {};
  const dist = Math.max(Math.abs(me.pos[0] - en.pos[0]), Math.abs(me.pos[1] - en.pos[1]));
  const centre = [(map.tiles[0].length - 1) >> 1, (map.tiles.length - 1) >> 1];

  // movement
  const moveP = style === "pacifist" ? 0.5 : 0.6;
  if (chance(moveP)) {
    let d;
    const r = h.int(100);
    if (style === "pacifist") d = r < 70 ? dirTowards(me.pos, centre) : pick(DIRS);
    else if (style === "kiter") d = r < 60 ? dirTowards(me.pos, en.pos, dist <= 2) : pick(DIRS);
    else if (style === "random") d = pick(DIRS);
    else d = r < 65 ? dirTowards(me.pos, en.pos) : pick(DIRS);
    t.move = d ?? pick(DIRS);
  }
  if (chance(style === "brawler" ? 0.12 : 0.07)) {
    const d = chance(0.5) ? dirTowards(me.pos, en.pos, chance(0.5)) : pick(DIRS);
    if (t.move !== undefined && chance(0.9)) delete t.move; // usually one or the other
    t.dash = d ?? pick(DIRS);
  }
  if (chance(garbage)) t[chance(0.5) ? "move" : "dash"] = pick(BAD_DIRS);
  if (chance(garbage * 0.3)) t.move = null;

  // attack
  const attackP = style === "pacifist" ? 0.04 : style === "brawler" ? 0.6 : 0.4;
  if (chance(attackP)) {
    const near = (p, k) => [p[0] + range(-k, k), p[1] + range(-k, k)];
    const anyTile = () => [int(map.tiles[0].length), int(map.tiles.length)];
    const r = int(100);
    const names = Object.keys(rules.attacks);
    let name = dist <= 1 && chance(0.6) ? "repulsor" : r < 55 ? "ranged" : r < 80 ? "frostburst" : "repulsor";
    if (!names.includes(name)) name = pick(names);
    const a = { name };
    if (name !== "repulsor" || chance(0.1)) {
      const k = int(100);
      a.target =
        k < 45 ? [en.pos[0], en.pos[1]]
        : k < 70 ? near(en.pos, 2)
        : k < 85 ? anyTile()
        : k < 90 ? near(me.pos, 1) // sometimes the fighter's own tile
        : k < 95 ? [range(-1000, 1000), range(-1000, 1000)]
        : near(me.pos, 9);
    }
    t.attack = a;
  }
  if (chance(garbage)) {
    t.attack = pick([
      "ranged", 7, {}, { name: "laser", target: [1, 1] }, { name: 3 }, { name: "ranged" }, { name: "ranged", target: null },
      { name: "frostburst" }, { name: "__proto__", target: [1, 1] }, { name: "toString", target: [1, 1] }, [], true,
      { name: pick(["ranged", "frostburst"]), target: pick(BAD_TARGETS) },
      { name: "repulsor", target: pick(BAD_TARGETS) },
    ]);
  }
  if (chance(garbage * 0.3)) t.attack = null;

  // shout
  if (chance(style === "garbage" ? 0.2 : 0.03)) t.shout = pick(SHOUTS);
  if (chance(garbage * 0.2)) t.reason = "timeout";
  return t;
}

const seen = {};
const STYLES = ["brawler", "brawler", "kiter", "random", "pacifist", "garbage"];
const out = [];
// Wait for the reader when the pipe is full, so a long run streams instead of piling up in memory.
const flush = async () => {
  if (!out.length) return;
  const chunk = out.join("\n") + "\n";
  out.length = 0;
  if (!process.stdout.write(chunk)) await new Promise((resolve) => process.stdout.once("drain", resolve));
};

for (let g = 0; g < GAMES; g++) {
  const rand = rng((SEED * 1000003 + g * 7919) >>> 0);
  const h = makeHelpers(rand);
  const rules = loadRules(h.chance(P_VARY_RULES) ? variedRules(h) : JSON.parse(JSON.stringify(SHIPPED_RULES)));
  const usesShipped = !h.chance(P_RANDOM_MAP);
  const rawMap = usesShipped ? JSON.parse(JSON.stringify(shippedMap(h.pick(SHIPPED)))) : randomMap(h, g);
  const mapJson = { name: rawMap.name, tiles: rawMap.tiles, spawns: rawMap.spawns, pickups: rawMap.pickups ?? [] };
  const map = loadMap(JSON.parse(JSON.stringify(mapJson)));
  const seed = h.chance(0.05) ? h.pick([0, 1, -1, -2147483648, 2 ** 31, 2 ** 53 - 1, 4294967295]) : h.int(2 ** 31);
  const styles = [h.pick(STYLES), h.pick(STYLES)];
  // Draw everything above even for skipped games, so --only replays the same game.
  if (ONLY >= 0 && g !== ONLY) continue;

  const state = createMatch(rules, map, seed);
  out.push(JSON.stringify({
    type: "header", engine: ENGINE_VERSION, replay_format: 1, rules_hash: rulesHash(rules), map: map.name, seed,
    bots: [{ id: 1, name: `fuzz-${styles[0]}`, version: "0" }, { id: 2, name: `fuzz-${styles[1]}`, version: "0" }],
    rules, map_data: mapJson, fuzz: { run_seed: SEED, game: g },
  }));
  while (!state.result) {
    const turns = {};
    for (const id of [1, 2]) {
      const t = genTurn(h, state, id, styles[id - 1], rules, mapJson);
      if (t !== undefined) turns[id] = t;
    }
    // Step with exactly what the file will say, so the two engines read the same turns.
    const line = { type: "tick", tick: state.tick, turns: JSON.parse(JSON.stringify(turns)) };
    for (const e of step(state, line.turns)) {
      const k = e.kind === "rejected" ? `rejected:${e.reason}` : e.kind === "projectile_end" ? `projectile_end:${e.reason}` : e.kind === "hit" && e.ricochet ? "hit:ricochet" : e.kind;
      seen[k] = (seen[k] ?? 0) + 1;
    }
    line.hash = stateHash(state);
    if (STATES) {
      const { rules: _r, map: _m, events: _e, ...rest } = state;
      line.state = rest;
    }
    if (OBSERVATIONS) {
      line.obs = {};
      for (const f of state.fighters) if (f.alive) line.obs[f.id] = observe(state, f.id);
    }
    out.push(JSON.stringify(line));
    if (out.length >= 2000) await flush();
  }
  out.push(JSON.stringify({ type: "result", ...state.result }));
  await flush();
}
await flush();
if (args.includes("--stats")) {
  const rows = Object.entries(seen).sort((a, b) => b[1] - a[1]).map(([k, n]) => `${k} ${n}`);
  process.stderr.write(`events seen across the run:\n  ${rows.join("\n  ")}\n`);
}
