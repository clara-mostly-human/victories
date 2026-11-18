#!/usr/bin/env python3
"""Writes field-marshal/playbooks/flagship.json from the ladder standings in the fleet store.

  tools/flagship-playbook.py [--threshold 2250] [--out field-marshal/playbooks/default.json] [--dry-run]

The flagship holds its frostburst for the opening (matrix-6) only against opponents rated
at or above the threshold, and plays the old way (matrix-5) against everyone else. Why:
scout games since 2026-11-18 04:45Z, the arms that hold the burst against the other arms,
by opponent rating: 2500 and up 26% against 7%; 2250 to 2500 57% against 40%; 2000 to 2250
75% against 72%; under 2000 89% against 98%. Against weaker bots the thrown burst is what
slows the target and sets up our shots; held back, we stop shooting and games run to the
tick limit. Against stronger bots a burst thrown while their dash is ready is punished.

With --out the same playbook can be written as the scouts' default (playbooks/default.json),
which makes it the baseline that new experiment arms are laid over.

The standings come from data/fleet.db (table `ladder`, refreshed by the fleet every 30
minutes; this script only reads it). Run it again when the ladder has moved, then restart
field-marshal between matches: the playbook is read when the bot starts.
"""
import json, os, sqlite3, sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
args = sys.argv[1:]
threshold = float(args[args.index("--threshold") + 1]) if "--threshold" in args else 2250.0
ours = {"field-marshal", "foot-soldier", "shock-trooper", "quarter-master", "spy-master"}
hold = {"name": "matrix-6", "frost_combo": 8, "frost_wait_cost": 4}

db = sqlite3.connect("file:" + os.path.join(root, "data", "fleet.db") + "?mode=ro", uri=True)
rows = db.execute("select name, rating from ladder where rating >= ? order by rating desc", (threshold,)).fetchall()
strong = [name for name, _ in rows if name not in ours]
book = {"default": {"name": "matrix-5"}, "opponents": {name: hold for name in strong}}
text = json.dumps(book, indent=2) + "\n"
if "--dry-run" in args:
    print(text)
else:
    out = args[args.index("--out") + 1] if "--out" in args else os.path.join("field-marshal", "playbooks", "flagship.json")
    with open(os.path.join(root, out), "w") as f:
        f.write(text)
print(f"{len(strong)} opponents rated {threshold:.0f} and up hold the burst: {', '.join(strong)}", file=sys.stderr)
