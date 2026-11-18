#!/usr/bin/env node
// Build against build: plays two `marshal` binaries against each other, with the official
// simulator's match core as the referee. Each bot runs as its own process in --stdio mode
// and sees exactly the frames the ladder would send (hello, one observation per tick, end),
// with the ladder's time limits. Nothing here touches the ladder.
//
//   node fleet/duel.mjs --a <marshal> [--a-playbooks file] --b <marshal> [--b-playbooks file]
//        [--games 200] [--workers 2] [--seed 1]
//
// Games come in pairs: the same map and seed twice with the seats swapped, so map, spawn
// and seat effects cancel. One JSON line per finished game on stdout, from A's side:
//   {"game":0,"map":"arena-21","seed":100000,"a_seat":1,"result":"win","reason":"last_standing","ticks":143,...}
// and a final {"done":true,...} line. `fleet release gate` reads these.

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sim = await import(pathToFileURL(process.env.VICTRIS_SIM ?? join(here, "../sim/arena-sim.mjs")).href);
const { MatchCore, SHIPPED_MAPS, SHIPPED_RULES, frameOutcome, timeoutOutcome } = sim;

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i < 0 ? dflt : args[i + 1];
};
const GAMES = Number(opt("games", "200"));
const WORKERS = Number(opt("workers", "2"));
const SEED = Number(opt("seed", "1"));
const SIDES = {
  a: { bin: opt("a", ""), playbooks: opt("a-playbooks", "") },
  b: { bin: opt("b", ""), playbooks: opt("b-playbooks", "") },
};
if (!SIDES.a.bin || !SIDES.b.bin) {
  console.error("usage: duel.mjs --a <marshal> --b <marshal> [--a-playbooks f] [--b-playbooks f] [--games N] [--workers W] [--seed S]");
  process.exit(2);
}
const LIMITS = SHIPPED_RULES.limits;
const children = new Set();

class Bot {
  constructor(side) {
    this.side = side;
    this.waiting = null;
    this.dead = false;
    const a = ["--bot", `duel-${side}`, "--stdio"];
    if (SIDES[side].playbooks) a.push("--playbooks", SIDES[side].playbooks);
    this.child = spawn(SIDES[side].bin, a, { stdio: ["pipe", "pipe", "ignore"] });
    children.add(this.child);
    this.child.on("exit", () => {
      this.dead = true;
      children.delete(this.child);
      this.settle(null);
    });
    this.child.on("error", () => { this.dead = true; this.settle(null); });
    this.child.stdin.on("error", () => {});
    createInterface({ input: this.child.stdout }).on("line", (line) => {
      if (!this.waiting) return;
      let msg;
      try { msg = JSON.parse(line); } catch { return; }
      // Frames that are not the answer we are waiting for (a late turn, the queue frame
      // sent after `end`) are dropped, as the ladder drops them.
      if (this.waiting.accept(msg)) this.settle(line);
    });
  }
  settle(value) {
    const w = this.waiting;
    if (!w) return;
    this.waiting = null;
    clearTimeout(w.timer);
    w.resolve(value);
  }
  send(frame) {
    if (!this.dead) this.child.stdin.write(JSON.stringify(frame) + "\n");
  }
  // The next frame `accept` takes, as raw text, or null after `ms`.
  reply(ms, accept) {
    if (this.dead) return Promise.resolve(null);
    return new Promise((resolve) => {
      const timer = setTimeout(() => this.settle(null), ms);
      this.waiting = { resolve, accept, timer };
    });
  }
  stop() {
    try { this.child.kill("SIGKILL"); } catch {}
  }
}

async function play(game, bots) {
  const pair = game >> 1;
  const aSeat = (game & 1) + 1;
  const map = SHIPPED_MAPS[pair % SHIPPED_MAPS.length];
  const seed = SEED * 100000 + pair;
  const sideOf = { [aSeat]: "a", [3 - aSeat]: "b" };
  const seats = [1, 2].map((id) => ({ fighterId: id, name: `duel-${sideOf[id]}`, version: sideOf[id] }));
  const core = new MatchCore({ matchId: `duel-${SEED}-${game}`, rules: SHIPPED_RULES, map, seed, limits: LIMITS, seats });
  const stat = { a: { timeouts: 0, max_ms: 0, sum_ms: 0, n: 0 }, b: { timeouts: 0, max_ms: 0, sum_ms: 0, n: 0 } };

  for (const id of [1, 2]) bots[sideOf[id]].send(core.hello(id));
  const ready = await Promise.all([1, 2].map((id) => bots[sideOf[id]].reply(LIMITS.startup_ms, (m) => m.type === "ready")));
  const late = [1, 2].filter((id) => ready[id - 1] === null);
  if (late.length) {
    // A bot that cannot even start would hand the other side a free win in every game.
    // That is a broken build or a bad playbooks file, not a result: stop the whole run.
    fail(`${late.map((id) => `side ${sideOf[id]} (${SIDES[sideOf[id]].bin})`).join(" and ")} did not answer hello`);
  }

  while (!core.result) {
    const tick = core.state.tick;
    const outcomes = {};
    // Both observations go out before either answer is awaited: the bots think at the
    // same time, as on the ladder.
    await Promise.all([1, 2].map(async (id) => {
      const bot = bots[sideOf[id]];
      const obs = core.takeObservation(id);
      const t0 = performance.now();
      bot.send(obs);
      const frame = await bot.reply(LIMITS.tick_ms, (m) => typeof m.tick === "number" && m.tick === tick);
      const ms = performance.now() - t0;
      const s = stat[sideOf[id]];
      s.n++;
      s.sum_ms += ms;
      s.max_ms = Math.max(s.max_ms, ms);
      if (frame === null) s.timeouts++;
      outcomes[id] = { ms, outcome: frame === null ? timeoutOutcome() : frameOutcome(frame, tick) };
    }));
    for (const id of [1, 2]) core.charge(id, Math.round(outcomes[id].ms));
    core.resolve({ 1: outcomes[1].outcome, 2: outcomes[2].outcome });
  }
  const summary = core.summary();
  const ends = {};
  for (const id of [1, 2]) {
    ends[sideOf[id]] = core.endMessage(id, summary, null);
    bots[sideOf[id]].send(ends[sideOf[id]]);
  }
  const round = (x) => Math.round(x * 10) / 10;
  return {
    game, map: map.name ?? String(pair % SHIPPED_MAPS.length), seed, a_seat: aSeat,
    result: ends.a.result, reason: ends.a.reason ?? summary.result?.reason, ticks: summary.result?.final_tick,
    a_timeouts: stat.a.timeouts, b_timeouts: stat.b.timeouts,
    a_ms: { mean: round(stat.a.sum_ms / Math.max(1, stat.a.n)), max: round(stat.a.max_ms) },
    b_ms: { mean: round(stat.b.sum_ms / Math.max(1, stat.b.n)), max: round(stat.b.max_ms) },
  };
}

function fail(why) {
  process.stdout.write(JSON.stringify({ error: why }) + "\n");
  for (const c of children) { try { c.kill("SIGKILL"); } catch {} }
  process.exit(1);
}

let next = 0;
let stopping = false;
const tally = { win: 0, loss: 0, draw: 0 };

async function worker() {
  let bots = { a: new Bot("a"), b: new Bot("b") };
  while (!stopping) {
    const game = next++;
    if (game >= GAMES) break;
    // A bot that died plays the rest of that game as timeouts; it gets a fresh process after.
    for (const side of ["a", "b"]) if (bots[side].dead) bots[side] = new Bot(side);
    const r = await play(game, bots);
    if (stopping) break;
    tally[r.result] = (tally[r.result] ?? 0) + 1;
    process.stdout.write(JSON.stringify(r) + "\n");
  }
  bots.a.stop();
  bots.b.stop();
}

function shutdown() {
  stopping = true;
  for (const c of children) { try { c.kill("SIGKILL"); } catch {} }
  process.exit(0);
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
process.stdout.on("error", shutdown); // the reader went away

await Promise.all(Array.from({ length: Math.max(1, WORKERS) }, worker));
process.stdout.write(JSON.stringify({ done: true, games: tally.win + tally.loss + tally.draw, ...tally }) + "\n");
for (const c of children) { try { c.kill("SIGKILL"); } catch {} }
process.exit(0);
