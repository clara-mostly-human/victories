#!/bin/zsh
# Runs one bot from this workspace on the ladder: field-marshal/run.sh <bot-name>
# One process per bot. The token is read from ../bots/<bot-name> (gitignored) by the
# binary itself and is never printed. The version to queue as is versions/<bot-name>,
# falling back to versions/default. To ship a change: build, bump the version file, and
# send the process SIGTERM; it finishes its match and exits, and launchd starts the new one.
set -e
cd "$(dirname "$0")"
name="${1:?usage: run.sh <bot-name>}"
if [ ! -f "../bots/$name" ]; then
  echo "no token file bots/$name: the bot must be registered first" >&2
  exit 2
fi
vfile="versions/$name"
[ -f "$vfile" ] || vfile="versions/default"
version="$(tr -d '[:space:]' < "$vfile")"
[ -x target/release/marshal ] || cargo build --release -p marshal
echo "$(date '+%F %T') starting $name as $version"
# caffeinate -i keeps the Mac from idling into sleep while the bot runs.
exec caffeinate -i target/release/marshal --bot "$name" --version "$version" \
  --playbooks playbooks/default.json --log-dir "../logs/$name"
