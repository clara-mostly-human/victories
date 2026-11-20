#!/bin/zsh
# Builds and runs the playground: play the game by hand against the bot's brain, no clock,
# rewind to any tick. Then open http://localhost:8790/ (or this Mac's name, port 8790).
#
#   playground/run.sh                 build if needed, then serve on port 8790
#   playground/run.sh --build         build only
#   playground/run.sh --port 8791     any other argument goes to the server (--help lists them)
#
# The build runs in the background class (taskpolicy -b), so it never takes a fast core
# from the live bots, and into its own folder (data/build/playground), so it never waits
# on or blocks a build in field-marshal/target. The server itself is idle until you move.
set -e
cd "$(dirname "$0")"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/../data/build/playground}"
export PATH="$HOME/.cargo/bin:$PATH"
bin="$CARGO_TARGET_DIR/release/playground"
slow() { if command -v taskpolicy >/dev/null 2>&1; then taskpolicy -b "$@"; else nice -n 15 "$@"; fi }
if [ "$1" = "--build" ]; then
  slow cargo build --release
  exit 0
fi
# Rebuild when the playground or the bot's code is newer than the binary.
if [ ! -x "$bin" ] || [ -n "$(find src build.rs Cargo.toml ../field-marshal/crates/engine/src ../field-marshal/crates/bot/src -newer "$bin" -print -quit 2>/dev/null)" ]; then
  if [ -x "$bin" ] && [ -n "$PLAYGROUND_NO_REBUILD" ]; then
    echo "playground: sources are newer than the build; serving the existing build (PLAYGROUND_NO_REBUILD)"
  else
    echo "playground: building (a few minutes the first time)…"
    slow cargo build --release || { [ -x "$bin" ] && echo "playground: build failed, serving the previous build" || exit 1; }
  fi
fi
exec "$bin" --root "$PWD/.." "$@"
