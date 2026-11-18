#!/usr/bin/env python3
"""Fetches ladder replays into a folder, politely, from wherever this runs.

It exists so that replay downloads do not come from the address the live bots play from:
on 2026-11-18 fetching from that address came with bursts of dropped bot connections.
The GitHub workflow .github/workflows/corpus.yml runs it and commits the result to the
`corpus` branch; the Mac pulls that branch (`fleet collect import`), verifies every replay
on the exact engine and indexes it.

  fetch_replays.py --out DIR [--max 300] [--gap 1.5] [--top 12] [--ours a,b,c] [--ours-share 0.4]

DIR gets replays/<first 5 characters of the id>/<id>.jsonl.gz and index.jsonl, one line per
replay: the ladder's own record of the match (bots, versions, ratings, result).
Only the standard library is used. One request at a time; it stops at the first 403 or 429.
"""
import argparse, json, os, sys, time, urllib.error, urllib.parse, urllib.request

HOST = "https://victrisarena.com"
AGENT = {"User-Agent": "replay-collector/1.0"}
KEEP = ("id", "started", "ended", "map", "seed", "winner", "reason", "final_tick", "voided")
BOT_KEEP = ("id", "name", "version", "fighter", "rating_before", "rating_after", "house")


class Blocked(Exception):
    pass


def get(url, gap):
    """One request, then the pause. Raises Blocked when the host says slow down."""
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=AGENT), timeout=45) as r:
            data = r.read()
    except urllib.error.HTTPError as e:
        time.sleep(gap)
        if e.code in (403, 429):
            raise Blocked(f"HTTP {e.code} for {url}")
        return None
    except Exception as e:  # noqa: BLE001  a network hiccup costs one item, not the run
        print(f"  {url}: {e}", flush=True)
        time.sleep(gap)
        return None
    time.sleep(gap)
    return data


def safe(match_id):
    return bool(match_id) and len(match_id) <= 64 and all(c.isalnum() or c in "_-" for c in match_id)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--max", type=int, default=300)
    ap.add_argument("--gap", type=float, default=1.5)
    ap.add_argument("--top", type=int, default=12)
    ap.add_argument("--ours", default="field-marshal,foot-soldier,shock-trooper,quarter-master,spy-master")
    # Our own games first: the fight scorecards are about our arms (2026-11-18).
    ap.add_argument("--ours-share", type=float, default=0.8)
    a = ap.parse_args()

    index_path = os.path.join(a.out, "index.jsonl")
    have = set()
    if os.path.exists(index_path):
        for line in open(index_path):
            try:
                have.add(json.loads(line)["id"])
            except (ValueError, KeyError):
                pass
    print(f"{len(have)} replays already in the corpus", flush=True)

    ours = [n for n in a.ours.split(",") if n]
    added = 0
    try:
        board = get(f"{HOST}/api/leaderboard", a.gap)
        rows = json.loads(board) if board else []
        rows = rows if isinstance(rows, list) else rows.get("bots", [])
        top = [r["name"] for r in rows if r.get("name") and r["name"] not in ours][: a.top]
        print("top bots:", ", ".join(top), flush=True)

        def wanted(names, budget):
            """Newest unfetched matches of these bots, taking turns between the bots."""
            lists = []
            for name in names:
                body = get(f"{HOST}/api/matches?bot={urllib.parse.quote(name)}&limit=200", a.gap)
                ms = json.loads(body) if body else []
                ms = ms if isinstance(ms, list) else ms.get("matches", [])
                lists.append([m for m in ms if safe(m.get("id", "")) and m["id"] not in have and not m.get("voided") and m.get("ended")])
            out, seen, k = [], set(), 0
            while len(out) < budget and any(k < len(l) for l in lists):
                for l in lists:
                    if k < len(l) and l[k]["id"] not in seen and len(out) < budget:
                        seen.add(l[k]["id"])
                        out.append(l[k])
                k += 1
            return out

        ours_budget = int(a.max * a.ours_share)
        todo = wanted(top, a.max - ours_budget)
        chosen = {m["id"] for m in todo}
        todo += [m for m in wanted(ours, ours_budget + (a.max - ours_budget - len(todo))) if m["id"] not in chosen]
        print(f"{len(todo)} replays to fetch", flush=True)

        with open(index_path, "a") as index:
            for m in todo[: a.max]:
                data = get(f"{HOST}/replays/{m['id']}.jsonl.gz", a.gap)
                if not data or data[:2] != b"\x1f\x8b":
                    continue
                folder = os.path.join(a.out, "replays", m["id"][:5])
                os.makedirs(folder, exist_ok=True)
                with open(os.path.join(folder, m["id"] + ".jsonl.gz"), "wb") as f:
                    f.write(data)
                record = {k: m.get(k) for k in KEEP}
                record["bots"] = [{k: b.get(k) for k in BOT_KEEP} for b in m.get("bots", [])]
                index.write(json.dumps(record, separators=(",", ":")) + "\n")
                index.flush()
                have.add(m["id"])
                added += 1
    except Blocked as e:
        print(f"stopping early: {e}", flush=True)
    print(f"{added} replays added; the corpus branch now holds {len(have)}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
