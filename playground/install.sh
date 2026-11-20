#!/bin/zsh
# Keeps the playground running on this Mac: a launchd agent (com.victris.playground) that
# serves http://<this Mac>:8790/ and comes back after a restart or a crash.
#   playground/install.sh            install or reload
#   playground/install.sh --remove   stop and remove
# It serves the build that exists and never builds by itself: run `playground/run.sh --build`
# after changing the code, then this script again.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
plist="$HOME/Library/LaunchAgents/com.victris.playground.plist"
label="com.victris.playground"
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
if [ "$1" = "--remove" ]; then
  rm -f "$plist"
  echo "playground: agent removed"
  exit 0
fi
bin="$here/../data/build/playground/release/playground"
[ -x "$bin" ] || { echo "playground: no build yet, run playground/run.sh --build first" >&2; exit 1; }
mkdir -p "$here/../logs"
cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array>
    <string>$bin</string><string>--root</string><string>$here/..</string><string>--port</string><string>${PLAYGROUND_PORT:-8790}</string>
    <string>--web</string><string>$here/web/index.html</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>StandardOutPath</key><string>$here/../logs/playground.log</string>
  <key>StandardErrorPath</key><string>$here/../logs/playground.log</string>
</dict></plist>
PLIST
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "playground: running at http://localhost:${PLAYGROUND_PORT:-8790}/"
