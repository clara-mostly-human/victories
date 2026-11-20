#!/usr/bin/env python3
"""Checks a running playground end to end: plays a game with the bot's brain on both sides,
goes back and branches, loads the newest ladder replay of field-marshal and takes a seat in it.
Usage: playground/tools/smoke.py [port]   (the session in the browser is replaced)"""
import json, statistics, sys, time, urllib.error, urllib.request

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8790


def call(path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"http://localhost:{PORT}{path}", data=data, method="POST" if body is not None else "GET")
    try:
        return json.load(urllib.request.urlopen(req, timeout=120))
    except urllib.error.HTTPError as e:
        return json.load(e)


ok = True


def check(name, cond, detail=""):
    global ok
    ok = ok and bool(cond)
    print(("PASS " if cond else "FAIL ") + name + (": " + str(detail) if detail != "" else ""))


d = call("/api/state")
st = d["settings"]
print("playbook file:", st["book"], "| playbook:", st["playbook"], "| tree_ms:", st["tree_ms"], "| search view:", st["root_hook"])
check("search view built in", st["root_hook"])

d = call("/api/new", {"map": "arena-21", "seed": 7, "seat": 0})
t0 = time.time()
sims, ms = [], []
while not d["pos"]["result"] and d["pos"]["tick"] < 200:
    d = call("/api/botmove", {})
    if "error" in d:
        check("bot plays both sides", False, d["error"])
        break
    th = d["pos"]["prev_thoughts"][1]
    if th and th["note"] == "tree":
        sims.append(th["sims"])
        ms.append(th["ms"])
print(f"game: {d['pos']['tick']} ticks in {time.time() - t0:.1f} s, result {d['pos']['result']}")
check("tree decisions made", len(sims) > 5, f"{len(sims)} decisions, median {statistics.median(sims) if sims else 0:.0f} iterations in {statistics.median(ms) if ms else 0:.0f} ms")
check("search root visible", any(d2 for d2 in [d["pos"]["prev_thoughts"][1]] if d2 and (d2["root"] or d2["note"] != "tree")))

line = d["line"]
back = line[min(20, len(line) - 1)]["id"]
d = call("/api/goto", {"node": back})
check("rewind", d["pos"]["tick"] == min(20, len(line) - 1), f"at tick {d['pos']['tick']}")
d = call("/api/turn", {"turn": {"dash": "N"}})
check("branch from an earlier tick", d["pos"].get("siblings") and len(d["pos"]["siblings"]) >= 1 and d["pos"]["tick"] == min(20, len(line) - 1) + 1, f"{len(d['pos']['siblings'] or [])} ways played")
check("line saved", bool(d["session"]["saved"]), d["session"]["saved"])

r = call("/api/replays?q=field-marshal&limit=50")
print("replays listed:", r["total"])
loaded = None
for row in r["rows"]:
    d2 = call("/api/load", {"id": row["id"]})
    if "error" not in d2:
        loaded, d = row["id"], d2
        break
if loaded:
    v = d["session"]["verified"]
    check("ladder replay re-simulated exactly", v["wrong"] == 0 and v["checked"] > 0, f"{loaded}: {v['checked']} ticks checked, {v['wrong']} differ")
    mid = d["line"][len(d["line"]) // 2]["id"]
    d = call("/api/goto", {"node": mid})
    d = call("/api/think", {"seat": 0})
    check("bot asked at a replay position", d["pos"]["thoughts"][0] is not None, d["pos"]["thoughts"][0] and d["pos"]["thoughts"][0]["label"])
    d = call("/api/seat", {"seat": 1})
    d = call("/api/turn", {"turn": {}})
    d = call("/api/turn", {"turn": {}})
    check("seat taken in a replay", "error" not in d and not d["pos"]["recorded"], f"tick {d['pos']['tick']}")
else:
    print("no replay of field-marshal on this machine: replay checks skipped")
call("/api/new", {"map": "arena-21", "seat": 0})
print("ALL PASS" if ok else "SOME FAILED")
sys.exit(0 if ok else 1)
