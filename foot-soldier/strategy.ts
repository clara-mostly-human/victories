// foot-soldier v2: a one-ply scoring strategy. No imports and no I/O, so the same code runs in
// bot.ts on the ladder and inside the simulator (sim/eval.ts).
//
// What v1 (the official starter) taught us, from 31 straight ladder losses:
//  - A move is only possible every other tick (move cooldown 2). A shot fired from within 2 tiles
//    lands the tick it is sent, so anyone within 2 tiles of a fighter that has just moved gets a
//    guaranteed hit. v1 walked up to the enemy and stood there.
//  - Repulsor + wall slam (damage, then a 3-tick stun and free follow-up shots) was 30% of the
//    damage v1 took.
//  - v1's frostburst replaced shots and hit 6% of the time; opponents took 3-4x the pickups.
//
// So every tick v2 scores each tile it could end on (stay, or one of observation.moves) for
// incoming danger, the zone, distance to the enemy, wall-slam exposure and pickups, takes the best,
// and dashes only when nothing else avoids a hit. Every number comes from hello.rules.

type Dir = "N" | "NE" | "E" | "SE" | "S" | "SW" | "W" | "NW";
type Pos = [number, number];

/** The parts of an observation this bot reads. Every field: /docs/schema/observation.json. */
export interface Observation {
  tick: number;
  self: {
    id: number;
    pos: Pos;
    hp: number;
    max_hp: number;
    state: string;
    cooldowns: Record<string, number>;
  };
  enemies: {
    id: number;
    pos: Pos;
    hp: number;
    state: string;
    dash_cooldown: number;
    slow_ticks_left: number;
  }[];
  moves: { dir: Dir; tile: Pos }[];
  dashes: { dir: Dir; tile: Pos }[];
  danger: { tile: Pos; tick: number; damage?: number }[];
  pickups: { pos: Pos; kind: string; ready_in: number }[];
  zone: { center: Pos; radius: number; next_radius: number | null; shrinks_in: number | null };
  events?: { kind: string; fighter?: number; owner?: number; attack?: string }[];
}

/** The parts of hello.rules this bot reads. Every field: /docs/rules. */
interface Rules {
  attacks: {
    ranged: { damage: number; cooldown: number };
    frostburst: { range: number };
    repulsor: { cooldown: number; push: number };
  };
  dash: { distance: number };
  pickups?: {
    first_spawn?: number;
    health?: { amount: number; respawn: number };
    power?: { respawn: number };
  };
}

/** One tick's reply: a step or a dash (or neither), then an optional attack. An empty turn is idle. */
export interface Turn {
  move?: Dir;
  dash?: Dir;
  attack?: { name: string; target?: Pos };
}

/** What the bot keeps during one match. bot.ts creates it from hello; `mem` is filled in here. */
export interface Match {
  id: string;
  rules: Rules;
  /** hello.map: rows of tiles, "#" is a wall. Optional so the bot still runs without it. */
  map?: string[];
  /** hello.pickups: every pad on the map. */
  pads?: { pos: Pos; kind: string }[];
  mem?: Memory;
}

interface Memory {
  tick: number;
  enemyPos: Pos | null; // where the enemy was on the previous observation, if it was in sight
  enemyMovedLastTick: boolean; // so it cannot step this tick
  enemyRepulsorReadyAt: number;
  lastSeen: { pos: Pos; tick: number } | null;
  padReadyAt: Record<string, number>;
  fields: Record<string, number[]>; // walking distance to a target tile, per target
}

const DIRS: Record<Dir, Pos> = {
  N: [0, -1],
  NE: [1, -1],
  E: [1, 0],
  SE: [1, 1],
  S: [0, 1],
  SW: [-1, 1],
  W: [-1, 0],
  NW: [-1, -1],
};
const DIR_LIST = Object.values(DIRS);

const dist = (a: Pos, b: Pos) => Math.max(Math.abs(a[0] - b[0]), Math.abs(a[1] - b[1]));
const same = (a: Pos, b: Pos) => a[0] === b[0] && a[1] === b[1];
const key = (p: Pos) => `${p[0]},${p[1]}`;

function isWall(match: Match, p: Pos): boolean {
  const map = match.map;
  if (!map) return false;
  const row = map[p[1]];
  return row === undefined || p[0] < 0 || p[0] >= row.length || row[p[0]] === "#";
}

/** Would a step (or a shove) from `p` along `d` be stopped? Walls, the edge, and cut corners. */
function blocked(match: Match, p: Pos, d: Pos): boolean {
  if (isWall(match, [p[0] + d[0], p[1] + d[1]])) return true;
  if (d[0] !== 0 && d[1] !== 0)
    return isWall(match, [p[0] + d[0], p[1]]) || isWall(match, [p[0], p[1] + d[1]]);
  return false;
}

/**
 * Walking distance from `p` to `target`, around walls (a breadth-first search from the target,
 * cached: the map never changes). Greedy steps by straight-line distance get stuck behind pillars.
 */
function walk(match: Match, mem: Memory, p: Pos, target: Pos): number {
  const map = match.map;
  if (!map) return dist(p, target);
  const w = map[0]!.length;
  let field = mem.fields[key(target)];
  if (!field) {
    field = new Array<number>(w * map.length).fill(99);
    if (!isWall(match, target)) {
      field[target[1] * w + target[0]] = 0;
      const queue: Pos[] = [target];
      for (let i = 0; i < queue.length; i++) {
        const c = queue[i]!;
        const dc = field[c[1] * w + c[0]]!;
        for (const dv of DIR_LIST) {
          if (blocked(match, c, dv)) continue;
          const n: Pos = [c[0] + dv[0], c[1] + dv[1]];
          if (field[n[1] * w + n[0]]! > dc + 1) {
            field[n[1] * w + n[0]] = dc + 1;
            queue.push(n);
          }
        }
      }
    }
    mem.fields[key(target)] = field;
  }
  return field[p[1] * w + p[0]] ?? 99;
}

/** A fighter on `target` shoved by a repulsor fired from the adjacent tile `from`: does it hit a wall? */
function slams(match: Match, from: Pos, target: Pos): boolean {
  if (!match.map) return false;
  const d: Pos = [Math.sign(target[0] - from[0]), Math.sign(target[1] - from[1])];
  let p = target;
  for (let i = 0; i < match.rules.attacks.repulsor.push; i++) {
    if (blocked(match, p, d)) return true;
    p = [p[0] + d[0], p[1] + d[1]];
  }
  return false;
}

/** Damage landing on tile `p` exactly `offset` ticks from now (0 = the tick we are replying to). */
function hitAt(obs: Observation, p: Pos, offset: number): number {
  let total = 0;
  for (const d of obs.danger)
    if (d.tick - obs.tick === offset && same(d.tile, p)) total += d.damage ?? 10;
  return total;
}

/** Inside the zone, and inside the next one too once a shrink is announced. */
function zoneExcess(obs: Observation, p: Pos): number {
  const radius = obs.zone.shrinks_in === null ? obs.zone.radius : obs.zone.next_radius!;
  return Math.max(0, dist(p, obs.zone.center) - radius);
}

function remember(obs: Observation, match: Match): Memory {
  const first = match.rules.pickups?.first_spawn ?? 0;
  const mem = (match.mem ??= {
    tick: -1,
    enemyPos: null,
    enemyMovedLastTick: false,
    enemyRepulsorReadyAt: 0,
    lastSeen: null,
    padReadyAt: Object.fromEntries((match.pads ?? []).map((p) => [key(p.pos), first])),
    fields: {},
  });
  const enemy = obs.enemies[0];
  mem.enemyMovedLastTick =
    !!enemy && !!mem.enemyPos && mem.tick === obs.tick - 1 && !same(enemy.pos, mem.enemyPos);
  mem.enemyPos = enemy ? enemy.pos : null;
  mem.tick = obs.tick;
  if (enemy) mem.lastSeen = { pos: enemy.pos, tick: obs.tick };
  for (const e of obs.events ?? [])
    if (e.kind === "repulse" && e.owner !== obs.self.id)
      mem.enemyRepulsorReadyAt = obs.tick - 1 + match.rules.attacks.repulsor.cooldown;
  for (const p of obs.pickups) mem.padReadyAt[key(p.pos)] = obs.tick + p.ready_in;
  return mem;
}

/** The pad most worth walking to right now, with how much we want it (0 = none). */
function wantedPad(obs: Observation, match: Match, mem: Memory): { pos: Pos; want: number } | null {
  const me = obs.self;
  const heal = match.rules.pickups?.health?.amount ?? 0;
  const enemy = obs.enemies[0];
  let best: { pos: Pos; want: number } | null = null;
  for (const pad of match.pads ?? obs.pickups) {
    const d = walk(match, mem, me.pos, pad.pos);
    const readyAt = mem.padReadyAt[key(pad.pos)] ?? obs.tick;
    if (readyAt > obs.tick + 2 * d + 2) continue; // not stocked by the time we could get there
    if (zoneExcess(obs, pad.pos) > 0) continue;
    if (enemy && walk(match, mem, enemy.pos, pad.pos) < d) continue; // the enemy gets there first
    const value = pad.kind === "health" ? Math.min(heal, me.max_hp - me.hp) / heal : 0.7;
    if (value < 0.4) continue;
    const want = value / (1 + d / 6);
    if (!best || want > best.want) best = { pos: pad.pos, want };
  }
  return best;
}

/** Where to look for an enemy that is out of sight: where it was last seen, else the zone centre. */
function searchGoal(obs: Observation, mem: Memory): Pos {
  if (mem.lastSeen && obs.tick - mem.lastSeen.tick < 40 && !same(mem.lastSeen.pos, obs.self.pos))
    return mem.lastSeen.pos;
  const r = Math.max(1, Math.min(3, Math.floor(obs.zone.radius / 2)));
  const [cx, cy] = obs.zone.center;
  const corners: Pos[] = [
    [cx - r, cy - r],
    [cx + r, cy - r],
    [cx + r, cy + r],
    [cx - r, cy + r],
  ];
  return corners[Math.floor(obs.tick / 24) % corners.length]!;
}

/** How good it is to end this tick on tile `p`. `moved` = we used our step to get there. */
function scoreTile(
  obs: Observation,
  match: Match,
  mem: Memory,
  p: Pos,
  moved: boolean,
  pad: { pos: Pos; want: number } | null,
  goal: Pos,
): number {
  const me = obs.self;
  let s = 0;
  // Incoming shots and blasts. After a step we are stuck there next tick, so that tick counts
  // almost as much; if we stay and still have our step, next tick's danger can be dodged then.
  const canStepNext = !moved && obs.moves.length > 0;
  s -= hitAt(obs, p, 0) * 100;
  s -= hitAt(obs, p, 1) * (canStepNext ? 4 : 80);
  s -= hitAt(obs, p, 2) * 6;
  // The zone hurts every tick outside it.
  const out = zoneExcess(obs, p);
  if (out > 0) s -= 300 + 60 * out + 5 * walk(match, mem, p, obs.zone.center);

  const enemy = obs.enemies[0];
  if (enemy) {
    const d = dist(p, enemy.pos);
    const enemyStuck = mem.enemyMovedLastTick || enemy.state !== "idle";
    const enemyCanDash = enemy.dash_cooldown === 0 && enemy.state === "idle";
    const ready = (name: string) => me.cooldowns[name] === 0;
    // How close the enemy can be to us when it attacks this tick or next.
    const reach = enemyStuck ? d : d - 1;
    // Within 2 tiles a shot lands the tick it is fired: no dodging it once we have stepped.
    if (reach <= 2) s -= moved ? 70 : 45;
    else if (reach === 3) s -= 12;
    if (enemyCanDash && d <= 5 && d > 2) s -= 8;
    if (d > 7) s -= (d - 7) * 12; // keep it in sight (radius 8) and in range
    // Wall slam: an adjacent enemy with its repulsor ready shoves us; into a wall that stuns.
    if (reach <= 1 && obs.tick >= mem.enemyRepulsorReadyAt) {
      s -= 25;
      for (const dv of DIR_LIST) {
        const from: Pos = [p[0] - dv[0], p[1] - dv[1]];
        if (isWall(match, from) || dist(from, enemy.pos) > (enemyStuck ? 0 : 1)) continue;
        if (slams(match, from, p)) {
          s -= 90;
          break;
        }
      }
    }
    // Our own chances. A stuck enemy that cannot dash is hit for certain from within 2 tiles,
    // and shoved into a wall for certain from next to it.
    if (enemyStuck && !enemyCanDash) {
      if (ready("ranged") && d <= 2) s += 75;
      if (ready("repulsor") && d === 1) s += slams(match, p, enemy.pos) ? 140 : 20;
    } else if (ready("repulsor") && d === 1 && slams(match, p, enemy.pos)) s += 45;
    // Being boxed in by walls next to an enemy leaves nowhere to dodge.
    if (d <= 4) for (const dv of DIR_LIST) if (blocked(match, p, dv)) s -= 3;
  } else {
    s -= walk(match, mem, p, goal) * 4;
  }
  if (pad) s -= walk(match, mem, p, pad.pos) * 14 * pad.want;
  // Prefer the middle of the zone a little: the fight ends there and the walls are further away.
  s -= dist(p, obs.zone.center) * 0.5;
  return s;
}

/** Movement phase: the best-scoring tile; a dash only to avoid a hit nothing else avoids. */
function movement(obs: Observation, match: Match, mem: Memory): Turn {
  const me = obs.self;
  const pad = wantedPad(obs, match, mem);
  const goal = searchGoal(obs, mem);
  let best: { turn: Turn; score: number } = {
    turn: {},
    score: scoreTile(obs, match, mem, me.pos, false, pad, goal),
  };
  for (const m of obs.moves) {
    const score = scoreTile(obs, match, mem, m.tile, true, pad, goal);
    if (score > best.score) best = { turn: { move: m.dir }, score };
  }
  // A dash is invulnerable on its own tick, then recovers for one: the landing must be safe after.
  const endTile = best.turn.move ? obs.moves.find((m) => m.dir === best.turn.move)!.tile : me.pos;
  const stuckHere = !best.turn.move && obs.moves.length === 0;
  const unavoidable =
    hitAt(obs, endTile, 0) > 0 ||
    ((best.turn.move || stuckHere) && hitAt(obs, endTile, 1) > 0) ||
    zoneExcess(obs, endTile) > 1;
  if (unavoidable) {
    let dash: { dir: Dir; score: number } | null = null;
    for (const d of obs.dashes) {
      if (hitAt(obs, d.tile, 1) + hitAt(obs, d.tile, 2) > 0) continue;
      const score = scoreTile(obs, match, mem, d.tile, true, pad, goal);
      if (!dash || score > dash.score) dash = { dir: d.dir, score };
    }
    if (dash) return { dash: dash.dir };
  }
  return best.turn;
}

/** Where the movement ends: the step's or dash's landing tile, or where we already stand. */
function endTile(obs: Observation, turn: Turn): Pos {
  const found = turn.move
    ? obs.moves.find((m) => m.dir === turn.move)
    : turn.dash
      ? obs.dashes.find((d) => d.dir === turn.dash)
      : null;
  return found ? found.tile : obs.self.pos;
}

/** Attack phase: fires from `from`, the tile the movement ends on. Each weapon has its own cooldown. */
function attack(obs: Observation, match: Match, from: Pos): Turn["attack"] {
  const enemy = obs.enemies[0];
  if (!enemy) return undefined;
  const ready = (name: string) => obs.self.cooldowns[name] === 0;
  const distance = dist(from, enemy.pos);
  // The repulsor hits every tile around us and needs no target.
  if (distance === 1 && ready("repulsor")) return { name: "repulsor" };
  // A shot is never wasted on a cooldown we don't need: fire whenever it is ready.
  if (ready("ranged")) return { name: "ranged", target: enemy.pos };
  // Frostburst only fills ticks with no shot to take. It slows whoever stands in it, which makes
  // the next shot easier to land. Cast standing still: sight is only known from where we stand.
  if (
    same(from, obs.self.pos) &&
    ready("frostburst") &&
    distance <= match.rules.attacks.frostburst.range &&
    !enemy.slow_ticks_left
  )
    return { name: "frostburst", target: enemy.pos };
  return undefined;
}

export function decide(obs: Observation, match: Match): Turn {
  const mem = remember(obs, match);
  // Stunned or recovering from a dash: nothing to decide meanwhile.
  if (obs.self.state !== "idle") return {};
  const turn = movement(obs, match, mem);
  const fire = attack(obs, match, endTile(obs, turn));
  return fire ? { ...turn, attack: fire } : turn;
}
