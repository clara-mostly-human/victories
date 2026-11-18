// Protocol gauntlet: plays the real `marshal` binary against local opponents over a real
// WebSocket, with the official simulator's match core as the server. Everything the ladder
// would send goes through the same code path: queue, hello, ready, one observation and one
// turn per tick, end. Nothing here touches the ladder.
//
//   ../foot-soldier/node_modules/.bin/tsx tools/gauntlet.ts [--games 40] [--seed 1]
//       [--opponents random,ts-starter,fs-v1,fs-v2] [--truth] [--port 8787] [--log-dir dir]
//
// --truth sends the true match state with every observation (as `_truth`, which a bot must
// ignore on the ladder); marshal compares its tracked state with it and reports how often
// they are identical. Opponents: the simulator's house bots and foot-soldier's strategies.

import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { HOUSE_BOTS, MatchCore, SHIPPED_MAPS, SHIPPED_RULES, frameOutcome, timeoutOutcome } from "../../sim/arena-sim.mjs";
import { decide as fsV2 } from "../../foot-soldier/strategy.ts";
import { decide as fsV1 } from "../../sim/opponents/v1.ts";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const { WebSocketServer } = require(join(here, "../../foot-soldier/node_modules/ws"));

const args = process.argv.slice(2);
const opt = (name: string, dflt: string) => {
  const i = args.indexOf(`--${name}`);
  return i < 0 ? dflt : args[i + 1]!;
};
const GAMES = Number(opt("games", "40"));
const SEED = Number(opt("seed", "1"));
const PORT = Number(opt("port", "8787"));
const TRUTH = args.includes("--truth");
const OPPONENTS = opt("opponents", "random,ts-starter,fs-v1,fs-v2").split(",");
const LOG_DIR = opt("log-dir", "");
const BOT = opt("bot", "gauntlet");
const PLAYBOOKS = opt("playbooks", "");
// --record <file>: write every frame of the run, both directions, as JSON lines (a tracker test fixture).
const RECORD = opt("record", "");
const recorded: string[] = [];

type Brain = (obs: any) => Record<string, unknown> | null;
function brain(name: string, hello: any): Brain {
  if (name === "fs-v1" || name === "fs-v2") {
    const fn = name === "fs-v1" ? fsV1 : fsV2;
    const match = { id: hello.match_id, rules: hello.rules, map: hello.map, pads: hello.pickups };
    return (obs) => {
      try {
        return fn(obs, match as never) as Record<string, unknown>;
      } catch {
        return {};
      }
    };
  }
  const house = (HOUSE_BOTS as Record<string, any>)[name];
  if (!house) throw new Error(`unknown opponent ${name}`);
  return house.start(hello);
}

interface Tally {
  win: number; loss: number; draw: number; ticks: number; timeouts: number; refused: number; rejected: number; ms: number[];
}
const tally: Record<string, Tally> = {};
const reasons: Record<string, number> = {};
let game = 0;

const wss = new WebSocketServer({ port: PORT, host: "127.0.0.1" });
wss.on("connection", (ws: any, req: any) => {
  if (!String(req.headers.authorization ?? "").startsWith("Bearer ")) {
    ws.send(JSON.stringify({ type: "error", code: "missing_authorization", message: "no token" }));
    return ws.close();
  }
  if (RECORD) {
    const send = ws.send.bind(ws);
    ws.send = (frame: string) => { recorded.push(JSON.stringify({ dir: "server", frame: JSON.parse(frame) })); send(frame); };
  }
  let waiting: ((frame: string | null) => void) | null = null;
  let wantTick = -1;
  let wantReady = false;
  let queued = false;
  ws.on("message", (data: Buffer) => {
    const text = data.toString();
    let msg: any;
    try { msg = JSON.parse(text); } catch { msg = null; }
    if (RECORD && msg) recorded.push(JSON.stringify({ dir: "bot", frame: msg }));
    if (msg?.type === "queue") { queued = true; return void next(); }
    if (msg?.type === "ready") { if (wantReady && waiting) { const w = waiting; waiting = null; w(text); } return; }
    if (msg?.type === "leave") return;
    // A turn: late replies for an older tick are discarded, as on the ladder.
    if (msg && typeof msg.tick === "number" && msg.tick < wantTick) return;
    if (!wantReady && waiting) { const w = waiting; waiting = null; w(text); }
  });
  const reply = (ms: number) =>
    new Promise<string | null>((resolve) => {
      const timer = setTimeout(() => { waiting = null; resolve(null); }, ms);
      waiting = (frame) => { clearTimeout(timer); resolve(frame); };
    });

  let running = false;
  async function next() {
    if (running || !queued) return;
    if (game >= GAMES) return void ws.close();
    running = true;
    queued = false;
    const g = game++;
    const seed = SEED * 1000 + g;
    const us = (g % 2) + 1;
    const them = 3 - us;
    const oppName = OPPONENTS[Math.floor(g / 2) % OPPONENTS.length]!;
    const map = SHIPPED_MAPS[seed % SHIPPED_MAPS.length]!;
    const seats = [
      { fighterId: us, name: BOT, version: "local" },
      { fighterId: them, name: oppName, version: "local" },
    ].sort((a, b) => a.fighterId - b.fighterId);
    const core = new MatchCore({ matchId: `local-${SEED}-${g}`, rules: SHIPPED_RULES, map, seed, limits: SHIPPED_RULES.limits, seats });
    const t = (tally[oppName] ??= { win: 0, loss: 0, draw: 0, ticks: 0, timeouts: 0, refused: 0, rejected: 0, ms: [] });
    ws.send(JSON.stringify(core.hello(us)));
    wantReady = true;
    const ready = await reply(SHIPPED_RULES.limits.startup_ms);
    wantReady = false;
    if (ready === null) core.forfeitBeforeStart([us]);
    const think = ready === null ? null : brain(oppName, JSON.parse(JSON.stringify(core.hello(them))));
    while (!core.result && think) {
      const tick = core.state.tick;
      const obs: any = core.takeObservation(us);
      for (const e of obs.events) {
        if (e.kind === "rejected" && e.fighter === us) {
          if (e.part === "(message)") t.refused++; else t.rejected++;
          reasons[`${e.part}:${e.reason}`] = (reasons[`${e.part}:${e.reason}`] ?? 0) + 1;
        }
      }
      if (TRUTH) {
        const { rules: _r, map: _m, events: _e, ...rest } = core.state as any;
        obs._truth = rest;
      }
      wantTick = tick;
      const t0 = performance.now();
      ws.send(JSON.stringify(obs));
      const frame = await reply(SHIPPED_RULES.limits.tick_ms);
      const ms = performance.now() - t0;
      core.charge(us, Math.round(ms));
      t.ms.push(ms);
      const mine = frame === null ? timeoutOutcome() : frameOutcome(frame, tick);
      if (frame === null) t.timeouts++;
      const oppObs = JSON.parse(JSON.stringify(core.takeObservation(them)));
      const oppReply = think(oppObs);
      const theirs = oppReply === null ? timeoutOutcome() : frameOutcome(JSON.stringify({ tick, ...oppReply }), tick);
      core.resolve({ [us]: mine, [them]: theirs });
    }
    const summary = core.summary();
    const end = core.endMessage(us, summary, null);
    t[end.result as "win" | "loss" | "draw"]++;
    t.ticks += summary.result.final_tick;
    ws.send(JSON.stringify(end));
    running = false;
    if (game >= GAMES) setTimeout(() => ws.close(), 200);
    else next();
  }
});

wss.on("listening", () => {
  const bin = join(here, "../target/release/marshal");
  const botArgs = ["--bot", BOT, "--url", `ws://127.0.0.1:${PORT}`, "--matches", String(GAMES)];
  if (LOG_DIR) botArgs.push("--log-dir", LOG_DIR);
  if (PLAYBOOKS) botArgs.push("--playbooks", PLAYBOOKS);
  const child = spawn(bin, botArgs, { env: { ...process.env, ARENA_TOKEN: "dev" }, stdio: ["ignore", "inherit", args.includes("--quiet") ? "ignore" : "inherit"] });
  child.on("exit", (code) => {
    const pct = (a: number[], p: number) => (a.length ? [...a].sort((x, y) => x - y)[Math.floor((a.length - 1) * p)]!.toFixed(1) : "-");
    let all = { win: 0, loss: 0, draw: 0 };
    for (const [name, t] of Object.entries(tally)) {
      const n = t.win + t.loss + t.draw;
      all = { win: all.win + t.win, loss: all.loss + t.loss, draw: all.draw + t.draw };
      console.log(
        `vs ${name.padEnd(10)} ${t.win}W ${t.loss}L ${t.draw}D of ${n} | avg ticks ${Math.round(t.ticks / Math.max(1, n))} | reply ms p50 ${pct(t.ms, 0.5)} p99 ${pct(t.ms, 0.99)} max ${pct(t.ms, 1)} | timeouts ${t.timeouts} | refused frames ${t.refused} | rejected parts ${t.rejected}`,
      );
    }
    console.log(`total ${all.win}W ${all.loss}L ${all.draw}D; bot exit code ${code}; rejected by reason ${JSON.stringify(reasons)}`);
    if (RECORD) writeFileSync(RECORD, recorded.join("\n") + "\n");
    wss.close();
    process.exit(code === 0 ? 0 : 1);
  });
});
