#!/bin/zsh
# What launchd runs for field-marshal. It starts whichever release
# data/releases/field-marshal-current points at, as the version in that release's VERSION
# file. A new release therefore needs no change to the launchd agent: `tools/release.sh`
# moves the pointer and asks the running bot to finish its match; launchd then starts
# this script again and it picks up the new release.
cd "$(dirname "$0")/../.."
rel="data/releases/field-marshal-current"
if [ ! -x "$rel/marshal" ]; then
  echo "$(date '+%F %T') no release at $rel; run field-marshal/tools/release.sh first" >&2
  sleep 60
  exit 1
fi
version="$(tr -d '[:space:]' < "$rel/VERSION")"
echo "$(date '+%F %T') starting field-marshal $version from $(readlink "$rel" || echo "$rel")"
# Releases built since the fleet store existed carry a `fleet` marker: they report their
# results to the shared store (report only: no experiments, playbooks stay ours).
extra=()
if [ -f "$rel/fleet" ]; then
  extra=(--fleet-db data/fleet.db --release "$(basename "$(readlink "$rel" || echo "$rel")")" \
    --ours field-marshal,foot-soldier,shock-trooper,quarter-master,spy-master)
fi
exec /usr/bin/caffeinate -i "$rel/marshal" --bot field-marshal --version "$version" \
  --playbooks "$rel/playbooks/default.json" --log-dir logs/field-marshal --token-file bots/field-marshal "${extra[@]}"
