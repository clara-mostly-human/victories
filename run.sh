#!/bin/zsh
# Runs one bot on the ladder: ./run.sh <bot-name> [version]
# The token is read from bots/<bot-name> (gitignored) and never printed.
set -e
cd "$(dirname "$0")"
name="${1:?usage: run.sh <bot-name> [version]}"
export ARENA_TOKEN="$(tr -d '[:space:]' < "bots/$name")"
export ARENA_VERSION="${2:-${ARENA_VERSION:-v1}}"
cd "$name"
exec ./node_modules/.bin/tsx bot.ts
