#!/bin/zsh
# Installs the launchd agent that keeps foot-soldier on the ladder (restarts on crash and at login).
# Uninstall: launchctl bootout gui/$(id -u)/com.victris.foot-soldier && rm ~/Library/LaunchAgents/com.victris.foot-soldier.plist
set -e
# Retired 2026-11-18: foot-soldier now runs the Rust bot under the fleet supervisor (fleet/README.md).
# Starting the TypeScript bot again would open a second connection on the same token.
if [ "${ALLOW_RETIRED_TS_BOT:-}" != "yes" ]; then
  echo "retired: foot-soldier is run by the fleet now (see fleet/README.md)" >&2
  exit 0
fi
cd "$(dirname "$0")"
cp com.victris.foot-soldier.plist ~/Library/LaunchAgents/
launchctl bootout "gui/$(id -u)/com.victris.foot-soldier" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.victris.foot-soldier.plist
echo "installed; log: ../logs/foot-soldier.log"
