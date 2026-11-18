#!/bin/sh
# Turns on the off-network replay fetcher. Run once, by a person, from the repository:
#
#   sh fleet/tools/enable-corpus.sh
#
# It does four things, all on GitHub's side or in this repository:
#   1. creates an empty `corpus` branch on origin (if there is none),
#   2. puts the workflow at .github/workflows/corpus.yml on main,
#   3. sets "corpus_branch": true in fleet/bots.json, so the fleet's health pass pulls,
#      verifies and indexes what the workflow commits,
#   4. commits and pushes those two files, and starts the first run.
#
# After that GitHub fetches up to 300 new replays every three hours from its own network;
# nothing is downloaded from the address the bots play from.
set -e
cd "$(git rev-parse --show-toplevel)"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "run this on main"; exit 1; }

if git ls-remote --exit-code --heads origin corpus >/dev/null 2>&1; then
  echo "the corpus branch is already there"
else
  blob=$(printf '# Replay corpus\n\nLadder replays fetched by the `Replay corpus` workflow on `main` (`fleet/tools/fetch_replays.py`).\n`index.jsonl` has one line per replay; the files are under `replays/`.\nThe fleet pulls this branch, verifies every replay on the exact engine and indexes it.\n' | git hash-object -w --stdin)
  tree=$(printf '100644 blob %s\tREADME.md\n' "$blob" | git mktree)
  commit=$(git commit-tree "$tree" -m "corpus: an empty start for the replay collector")
  git push origin "$commit:refs/heads/corpus"
  echo "created the corpus branch"
fi

mkdir -p .github/workflows
# The parked copy without its "not active" header.
sed '1,/^# Grows the replay corpus/{/^# Grows the replay corpus/!d;}' fleet/tools/corpus.workflow.yml > .github/workflows/corpus.yml
python3 - <<'E'
import re
p = "fleet/bots.json"
s = open(p).read()
if '"corpus_branch"' in s:
    s = re.sub(r'"corpus_branch":\s*(true|false)', '"corpus_branch": true', s)
else:
    k = s.rstrip().rfind("}")
    s = s[:k].rstrip() + ',\n  "corpus_branch": true\n}\n'
open(p, "w").write(s)
E
python3 -c "import json; assert json.load(open('fleet/bots.json'))['corpus_branch'] is True"

git add .github/workflows/corpus.yml fleet/bots.json
if git diff --cached --quiet; then
  echo "the workflow and the setting are already committed"
else
  git commit -q -m "corpus: turn on the off-network replay fetcher (workflow and corpus_branch)"
  git push origin main
fi

repo=$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')
if command -v gh >/dev/null 2>&1; then
  gh workflow run corpus.yml --repo "$repo" --ref main && echo "first run started: https://github.com/$repo/actions/workflows/corpus.yml"
else
  echo "start the first run by hand: https://github.com/$repo/actions/workflows/corpus.yml (Run workflow)"
fi
