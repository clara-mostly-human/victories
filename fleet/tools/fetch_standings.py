#!/usr/bin/env python3
"""Fetches the ladder's standings and our bots' own pages into one small JSON file.

It runs on GitHub (workflow .github/workflows/standings.yml), never on the machine the bots
play from: that machine talks to the ladder only through the gateway's bot sockets. The
fleet reads the file from the `standings` branch.

  fetch_standings.py --out standings.json [--ours a,b,c]

Only the standard library. A handful of requests per run; it stops at the first 403 or 429.
"""
import argparse, json, sys, time, urllib.error, urllib.parse, urllib.request

HOST = "https://victrisarena.com"
AGENT = {"User-Agent": "standings-collector/1.0"}
ROW_KEEP = ("name", "display_name", "rating", "rating_deviation", "score", "version", "wins", "losses", "draws", "games", "idle_penalty", "house", "online", "owner")
BOT_KEEP = ("name", "rating", "rank", "score", "online", "status", "idle_penalty", "played")
VERSION_KEEP = ("version", "label", "alias", "rating", "rating_deviation", "score", "rank", "wins", "losses", "draws", "games", "ranked", "last_played", "health")


def get(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=AGENT), timeout=30) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--ours", default="field-marshal,foot-soldier,shock-trooper,quarter-master,spy-master")
    a = ap.parse_args()
    out = {"fetched": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "leaderboard": [], "bots": {}, "errors": []}
    try:
        board = get(f"{HOST}/api/leaderboard?limit=500")
        rows = board if isinstance(board, list) else board.get("bots", [])
        for i, r in enumerate(rows):
            if r.get("name"):
                row = {k: r[k] for k in ROW_KEEP if k in r}
                row["rank"] = r.get("rank", i + 1)
                out["leaderboard"].append(row)
        for name in [n for n in a.ours.split(",") if n]:
            time.sleep(1.0)
            try:
                b = get(f"{HOST}/api/bots/{urllib.parse.quote(name)}")
            except urllib.error.HTTPError as e:
                if e.code in (403, 429):
                    raise
                out["errors"].append(f"{name}: HTTP {e.code}")
                continue
            # The bot's page is large (recent matches); keep the standing and the versions.
            keep = {k: b.get(k) for k in BOT_KEEP if k in b}
            # Every plain field of each version (the label's key is not documented), and its health.
            keep["versions"] = [{k: x for k, x in v.items() if not isinstance(x, (list, dict)) or k == "health"} for v in (b.get("versions") or [])[-12:]]
            out["bots"][name] = keep
    except urllib.error.HTTPError as e:
        out["errors"].append(f"stopped: HTTP {e.code} for {e.url}")
    except Exception as e:  # noqa: BLE001  a failed run still writes what it has, with the reason
        out["errors"].append(f"stopped: {e}")
    if not out["leaderboard"]:
        print("no standings fetched:", out["errors"], flush=True)
        return 1
    with open(a.out, "w") as f:
        json.dump(out, f, separators=(",", ":"))
    print(f"{len(out['leaderboard'])} rows, {len(out['bots'])} of our bots, errors: {out['errors']}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
