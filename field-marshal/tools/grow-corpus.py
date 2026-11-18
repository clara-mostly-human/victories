#!/usr/bin/env python3
"""Grows the replay corpus used by the conformance check.

Lists recent public ladder matches and downloads the replays that are not on disk yet
into <repo>/replays/corpus (about 2.5 KB each). Usage: tools/grow-corpus.py [target-count]
"""
import json, os, sys, time, urllib.parse, urllib.request

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "replays")
CORPUS = os.path.join(ROOT, "corpus")
UA = {"User-Agent": "arena-stats/1.0"}
target = int(sys.argv[1]) if len(sys.argv) > 1 else 5000


def get_json(url):
    for i in range(4):
        try:
            return json.load(urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60))
        except Exception as e:  # noqa: BLE001
            err = e
            time.sleep(2 * (i + 1))
    raise err


def listing(url):
    m = get_json(url)
    return m["matches"] if isinstance(m, dict) else m


os.makedirs(CORPUS, exist_ok=True)
have = {f[: -len(".jsonl.gz")] for f in os.listdir(CORPUS) if f.endswith(".jsonl.gz")}
want = {}


def add(matches):
    for m in matches:
        # Only replays served from the ladder's own site, saved under the match's own id.
        mid, url = m.get("id", ""), m.get("replay_url", "")
        if not url.startswith("https://victrisarena.com/replays/") or not mid.replace("_", "").isalnum():
            continue
        if m.get("voided") or mid in have:
            continue
        want[mid] = url


add(listing("https://victrisarena.com/api/matches?limit=5000"))
print(len(want), "new matches from the recent list", flush=True)
if len(have) + len(want) < target:
    # The recent list is capped, so also take each ranked bot's own recent matches.
    board = get_json("https://victrisarena.com/api/leaderboard")
    rows = board if isinstance(board, list) else board.get("bots", [])
    for row in rows:
        if len(have) + len(want) >= target:
            break
        name = row.get("name") or row.get("bot")
        if not name:
            continue
        try:
            add(listing(f"https://victrisarena.com/api/matches?bot={urllib.parse.quote(name)}&limit=500"))
        except Exception as e:  # noqa: BLE001
            print(name, "listing failed:", e, flush=True)
    print(len(want), "new matches after the per-bot lists", flush=True)

todo = list(want.items())[: max(0, target - len(have))]


def fetch(item):
    mid, url = item
    path = os.path.join(CORPUS, mid + ".jsonl.gz")
    for i in range(3):
        try:
            data = urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=30).read()
            if data[:2] != b"\x1f\x8b":
                raise ValueError("not a gzip file")
            with open(path + ".part", "wb") as f:
                f.write(data)
            os.replace(path + ".part", path)
            return 1
        except Exception as e:  # noqa: BLE001
            err = e
            time.sleep(1 + i)
    print(mid, err, flush=True)
    return 0


# One at a time: the replay host slows parallel downloads from one address to a crawl.
n = 0
for k, item in enumerate(todo):
    n += fetch(item)
    if k % 250 == 249:
        print(f"{k + 1} of {len(todo)} tried, {n} downloaded", flush=True)
print(f"{n} replays downloaded; corpus now holds {len(have) + n}")
