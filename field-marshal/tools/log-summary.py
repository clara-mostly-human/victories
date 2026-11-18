#!/usr/bin/env python3
"""One-line (or per-opponent) summary of a bot's match log.

  tools/log-summary.py <bot-name> [--by-opponent] [--version v1]

Reads <repo>/logs/<bot-name>/*.jsonl, the files `marshal` writes.
"""
import collections, glob, json, os, sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "logs")
args = sys.argv[1:]
bot = args[0] if args else "field-marshal"
by_opponent = "--by-opponent" in args
# Lines written before versions were logged belong to v1.
version = args[args.index("--version") + 1] if "--version" in args else None

matches, notes, think = [], collections.Counter(), []
for path in sorted(glob.glob(os.path.join(root, bot, "*.jsonl"))):
    for line in open(path):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        t = r.get("t")
        if version and r.get("version", "v1") != version:
            continue
        if t == "match":
            matches.append(r)
        elif t == "tick":
            think.append(r.get("us", 0))
        elif t not in ("hello",):
            notes[t] += 1

w = sum(m["result"] == "win" for m in matches)
l = sum(m["result"] == "loss" for m in matches)
d = len(matches) - w - l
track = collections.Counter()
for m in matches:
    for k, v in (m.get("tracking") or {}).items():
        track[k] += v
refused = sum(m.get("server_refusals", 0) for m in matches)
rejected = sum(m.get("rejected_parts", 0) for m in matches)
panics = sum(m.get("panics", 0) for m in matches)
rating = sum(m.get("rating_change") or 0 for m in matches)
think.sort()
p = lambda q: think[int((len(think) - 1) * q)] / 1000 if think else 0
print(
    f"{bot}{' ' + version if version else ''}: {len(matches)} games, {w}W {l}L {d}D, rating change {rating:+.0f}; "
    f"tracker mismatches {track['mismatch']} in {track['ticks']} ticks; refused frames {refused}, "
    f"rejected turn parts {rejected}, panics {panics}; think ms p50 {p(0.5):.2f} p99 {p(0.99):.2f} max {p(1):.2f}; "
    f"other log notes {dict(notes)}"
)
if by_opponent:
    per = collections.defaultdict(lambda: [0, 0, 0])
    for m in matches:
        per[m["opponent"]][("win", "loss").index(m["result"]) if m["result"] in ("win", "loss") else 2] += 1
    for name, (a, b, c) in sorted(per.items(), key=lambda kv: -sum(kv[1])):
        print(f"  {name}: {a}W {b}L {c}D")
