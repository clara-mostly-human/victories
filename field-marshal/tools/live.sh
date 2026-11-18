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
# No address preference is set here. On 2026-11-18 one connection to 172.67.157.96 was slow
# for its whole life (230 to 290 ms a tick, timed-out ticks) while 104.21.89.84 was fast;
# hours later a connection to 172.67.157.96 ran at 66 ms a tick and 104.21.89.84 at 110.
# So it is the connection, not the address: the client's reconnect after a slow match is
# the remedy. Set ARENA_PREFER_ADDRESS in the environment to force one for a test.
if [ -f data/releases/field-marshal-use-fleet ] && [ -x data/releases/live/marshal ]; then
  rel="data/releases/live"
  RELEASE="$(readlink "$rel" 2>/dev/null | xargs basename 2>/dev/null)"
  [ -f data/releases/live.env ] && source data/releases/live.env
  version="$(tr -d '[:space:]' < field-marshal/versions/field-marshal)"
  echo "$(date '+%F %T') starting field-marshal $version from fleet release ${RELEASE:-?}"
  # The flagship's own playbook: what the scouts' ladder games have earned it. The scouts'
  # control arm stays on the release's default.json, so an experiment's baseline never
  # moves when the flagship is promoted.
  book="$rel/playbooks/default.json"
  [ -f field-marshal/playbooks/flagship.json ] && book="field-marshal/playbooks/flagship.json"
  # Behind the fleet gateway since 2026-11-18 16:20Z: the gateway process holds the ladder
  # socket (and the token), this process attaches to it over a local socket and talks to
  # it on stdin and stdout, so restarting the bot no longer closes its connection. With
  # --provisional the bot also hands over a quick answer that the gateway plays if the
  # search is late. To roll back: remove "gateway": true for field-marshal in
  # fleet/bots.json and run the marshal directly again with --token-file bots/field-marshal
  # in place of --stdio --provisional.
  exec /usr/bin/caffeinate -i data/bin/fleet attach field-marshal -- "$rel/marshal" --bot field-marshal \
    --version "$version" --playbooks "$book" --log-dir logs/field-marshal \
    --fleet-db data/fleet.db --release "${RELEASE:-unknown}" --ours "$ours" --stdio --provisional
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
