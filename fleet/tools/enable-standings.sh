#!/bin/sh
# Turns on the off-network standings fetcher. Run once, by a person, from the repository:
#
#   sh fleet/tools/enable-standings.sh
#
# It puts the workflow at .github/workflows/standings.yml on main, pushes it, and starts the
# first run. After that GitHub fetches the ladder's standings and our bots' ratings every
# five minutes from its own network and keeps them on the `standings` branch (the workflow
# creates the branch itself). The fleet reads that branch; the machine the bots play from
# asks the ladder nothing over HTTP.
set -e
cd "$(git rev-parse --show-toplevel)"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "run this on main"; exit 1; }

mkdir -p .github/workflows
# The parked copy without its "not active" header.
sed '1,/^# Fetches the ladder/{/^# Fetches the ladder/!d;}' fleet/tools/standings.workflow.yml > .github/workflows/standings.yml

git add .github/workflows/standings.yml
if git diff --cached --quiet; then
  echo "the workflow is already committed"
else
  git commit -q -m "standings: turn on the off-network standings fetcher (workflow)"
  git push origin main
fi

repo=$(git remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')
echo "waiting for GitHub to register the workflow"
n=0
until gh workflow view standings.yml --repo "$repo" >/dev/null 2>&1; do
  n=$((n + 1)); [ $n -gt 20 ] && break; sleep 3
done
gh workflow run standings.yml --repo "$repo" --ref main && echo "first run started: https://github.com/$repo/actions/workflows/standings.yml"
