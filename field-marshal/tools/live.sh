#!/bin/zsh
# What launchd runs for field-marshal. The search thread owns this bot: nothing else
# starts, stops or restarts it.
#
# Two sources of builds:
#  * fleet releases (the normal case once switched on): data/releases/live, promoted by the
#    fleet pipeline, with the release id in data/releases/live.env. Used when the switch
#    file data/releases/field-marshal-use-fleet exists.
#  * this thread's own frozen builds: data/releases/field-marshal-current, moved by
#    tools/release.sh. Used otherwise, and as the fallback if the fleet release is missing.
# The ladder version label always comes from field-marshal/versions/field-marshal when the
# fleet release is used, and from the frozen build's VERSION file otherwise.
cd "$(dirname "$0")/../.."
ours="field-marshal,foot-soldier,shock-trooper,quarter-master,spy-master"
if [ -f data/releases/field-marshal-use-fleet ] && [ -x data/releases/live/marshal ]; then
  rel="data/releases/live"
  RELEASE="$(readlink "$rel" 2>/dev/null | xargs basename 2>/dev/null)"
  [ -f data/releases/live.env ] && source data/releases/live.env
  version="$(tr -d '[:space:]' < field-marshal/versions/field-marshal)"
  echo "$(date '+%F %T') starting field-marshal $version from fleet release ${RELEASE:-?}"
  exec /usr/bin/caffeinate -i "$rel/marshal" --bot field-marshal --version "$version" \
    --playbooks "$rel/playbooks/default.json" --log-dir logs/field-marshal --token-file bots/field-marshal \
    --fleet-db data/fleet.db --release "${RELEASE:-unknown}" --ours "$ours"
fi
rel="data/releases/field-marshal-current"
if [ ! -x "$rel/marshal" ]; then
  echo "$(date '+%F %T') no release at $rel; run field-marshal/tools/release.sh first" >&2
  sleep 60
  exit 1
fi
version="$(tr -d '[:space:]' < "$rel/VERSION")"
name="$(basename "$(readlink "$rel" || echo "$rel")")"
# Builds made since the fleet store existed carry a `fleet` marker: they report results
# to the shared store (report only: no experiments, playbooks stay ours).
extra=()
[ -f "$rel/fleet" ] && extra=(--fleet-db data/fleet.db --release "$name" --ours "$ours")
echo "$(date '+%F %T') starting field-marshal $version from $name"
exec /usr/bin/caffeinate -i "$rel/marshal" --bot field-marshal --version "$version" \
  --playbooks "$rel/playbooks/default.json" --log-dir logs/field-marshal --token-file bots/field-marshal "${extra[@]}"
