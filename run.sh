#!/bin/zsh
# Runs one bot on the ladder: ./run.sh <bot-name>
# The token is read from bots/<bot-name> (gitignored) and never printed.
# The version to queue as comes from <bot-name>/VERSION, so shipping a new version is:
# edit the strategy, bump VERSION, restart the process (launchd restarts it when it exits).
set -e
# Retired 2026-11-18: foot-soldier now runs the Rust bot under the fleet supervisor (fleet/README.md).
# Starting the TypeScript bot again would open a second connection on the same token.
if [ "${ALLOW_RETIRED_TS_BOT:-}" != "yes" ]; then
  echo "retired: foot-soldier is run by the fleet now (see fleet/README.md)" >&2
  exit 0
fi
cd "$(dirname "$0")"
name="${1:?usage: run.sh <bot-name>}"
export ARENA_TOKEN="$(tr -d '[:space:]' < "bots/$name")"
if [ -f "$name/VERSION" ]; then
  export ARENA_VERSION="$(tr -d '[:space:]' < "$name/VERSION")"
else
  export ARENA_VERSION="${2:-${ARENA_VERSION:-v1}}"
fi
echo "$(date '+%F %T') starting $name as $ARENA_VERSION"
cd "$name"
exec ./node_modules/.bin/tsx bot.ts
