#!/usr/bin/env python3
"""Summarise a bot's recent ladder games from the public API: ./sim/ladder_stats.py <bot> [limit]"""
import json, sys, time, urllib.request, urllib.parse
bot = sys.argv[1]; limit = int(sys.argv[2]) if len(sys.argv) > 2 else 40
def get(u):
    req = urllib.request.Request(u, headers={"User-Agent": "arena-stats/1.0"})
    return json.load(urllib.request.urlopen(req, timeout=30))
ms = get(f"https://victrisarena.com/api/matches?bot={urllib.parse.quote(bot)}&limit={limit}")
ms = ms["matches"] if isinstance(ms, dict) else ms
def strip(d):
    return {k: v for k, v in d.items() if k != "occupancy"}
first = True
for m in ms:
    try: s = get(m["stats_url"])
    except Exception as e: print(m["id"], "stats error", e); continue
    me = next(f for f in s["fighters"] if f["name"] == bot); op = next(f for f in s["fighters"] if f["name"] != bot)
    if first: print(json.dumps(strip(me), indent=1)); first = False
    ob = next(b for b in m["bots"] if b["name"] != bot)
    print("ROW", json.dumps({"opp": op["name"], "r": round(ob["rating_before"]), "ticks": s["ticks"], "won": m["winner"] != ob["id"], "me": strip(me), "op": strip(op)}))
    time.sleep(0.1)
