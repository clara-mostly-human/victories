#!/bin/zsh
# Installs the launchd agent that keeps foot-soldier on the ladder (restarts on crash and at login).
# Uninstall: launchctl bootout gui/$(id -u)/com.victris.foot-soldier && rm ~/Library/LaunchAgents/com.victris.foot-soldier.plist
set -e
cd "$(dirname "$0")"
cp com.victris.foot-soldier.plist ~/Library/LaunchAgents/
launchctl bootout "gui/$(id -u)/com.victris.foot-soldier" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.victris.foot-soldier.plist
echo "installed; log: ../logs/foot-soldier.log"
