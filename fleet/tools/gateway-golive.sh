#!/bin/sh
# One gateway restart with every pending gateway change (approved by Mike, 2026-11-19 01:47Z):
# scouts first for match slots, a mid-match socket swap after 3 wire-lost turns, the ladder's
# close reason recorded at connect, a first "replaced" retried, and per-fight recording.
# Each bot finishes its match, then all five reconnect (about a minute without games).
#   sh fleet/tools/gateway-golive.sh
cd "$(dirname "$0")/../.." || exit 1
F=data/build/fleet-dev-target/release/fleet
[ -x "$F" ] || { echo "no built fleet binary at $F"; exit 1; }
date -u +%T
$F install gateway
cp "$F" field-marshal/target/release/fleet.new && mv field-marshal/target/release/fleet.new field-marshal/target/release/fleet
$F install status
$F install watch
$F install online
n=0; until data/bin/fleet health >/dev/null 2>&1; do n=$((n+1)); [ $n -gt 90 ] && break; sleep 2; done
date -u +%T
data/bin/fleet health | cut -c1-160
