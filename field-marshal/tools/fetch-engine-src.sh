#!/bin/zsh
# Downloads the official engine's TypeScript source for reference. Every simulator release
# ships it in its source map. Usage: tools/fetch-engine-src.sh [release]   (default 0.82.2)
# Output: <repo>/engine-src/ (not in git). The port in crates/engine was written from
# packages/engine/src/*.ts of release 0.82.2 (engine 0.23.0).
set -e
release="${1:-0.82.2}"
dest="$(cd "$(dirname "$0")/../.." && pwd)/engine-src"
mkdir -p "$dest/raw"
curl -fsS -o "$dest/raw/arena-sim.mjs.map" "https://assets.victris.ai/download/$release/arena-sim.mjs.map"
python3 -I - "$dest" <<'EOF'
import json, os, sys
dest = sys.argv[1]
d = json.load(open(os.path.join(dest, "raw", "arena-sim.mjs.map")))
n = 0
for src, content in zip(d["sources"], d["sourcesContent"]):
    if "node_modules" in src or content is None:
        continue
    rel = src.replace("../../../", "").replace("../../", "packages/").replace("../src/", "packages/sim/src/")
    path = os.path.normpath(os.path.join(dest, rel))
    if not path.startswith(dest + os.sep):
        continue
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(content)
    n += 1
print(f"{n} source files written to {dest}")
EOF
