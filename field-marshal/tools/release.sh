#!/bin/zsh
# Freezes the current build of field-marshal as a release and puts it live.
#   field-marshal/tools/release.sh            build, freeze, switch the live bot to it
#   field-marshal/tools/release.sh --no-switch   build and freeze only
# The version is field-marshal/versions/field-marshal. A release is a folder under
# data/releases/ holding the binary, the playbooks and a VERSION file, so later rebuilds
# and arena runs cannot touch the live bot. Switching sends the running bot SIGTERM: it
# finishes its match and exits, and launchd starts tools/live.sh on the new release.
set -e
cd "$(dirname "$0")/.."
version="$(tr -d '[:space:]' < versions/field-marshal)"
sha="$(git rev-parse --short HEAD)"
git diff --quiet HEAD -- crates playbooks versions || sha="$sha-dirty"
nice -n 15 cargo build --release -p marshal -j 4
rel="../data/releases/field-marshal-$version-$sha"
mkdir -p "$rel/playbooks"
cp target/release/marshal "$rel/marshal"
cp playbooks/default.json "$rel/playbooks/default.json"
echo "$version" > "$rel/VERSION"
echo "froze $rel"
[ "$1" = "--no-switch" ] && exit 0
ln -sfn "field-marshal-$version-$sha" ../data/releases/field-marshal-current
# The bot itself, not caffeinate or any shell whose command line happens to mention it.
pid="$(ps -axo pid=,args= | awk '$2 ~ /\/marshal$/ && / --bot field-marshal / {print $1; exit}')"
if [ -n "$pid" ]; then
  kill -TERM "$pid"
  echo "live bot (pid $pid) asked to finish its match; launchd will start $version"
else
  echo "no live bot running; launchd will start $version when its agent is loaded"
fi
