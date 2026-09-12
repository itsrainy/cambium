#!/bin/sh
# Runs every ui-feel demo in order, pausing between so each can be looked at.
set -e
cd "$(dirname "$0")"
BIN=./target/debug/ui-feel
SPEED="${SPEED:-0.35}"
REPORT="${REPORT:-kv}"
[ -x "$BIN" ] || cargo build
for demo in plain stream affected chart full; do
    printf '\n\033[1;36m======== %s ========\033[0m\n\n' "$demo"
    "$BIN" --speed "$SPEED" --report "$REPORT" "$demo"
    printf '\n\033[2m[enter] for the next demo\033[0m'
    read -r _ </dev/tty || true
done
printf '\n\033[1mdone\033[0m\n'
