#!/usr/bin/env bash
# Scripted cdfuse demo for asciinema.
#
# Record:  asciinema rec demo.cast -c 'bash cdfuse/demo.sh'
# Render:  agg --cols 80 --rows 24 --idle-time-limit 1.5 demo.cast cdfuse/demo.gif

set -euo pipefail

BINARY="$(cd "$(dirname "$0")" && pwd)/target/release/cdfuse"
GAME="/cd"
MOUNT="/tmp/cd"

step() { printf '\n\e[2m# %s\e[0m\n' "$1"; sleep 0.8; }
cmd()  { printf '\e[1;32m$\e[0m %s\n' "$1"; }

# -- invisible setup ----------------------------------------------------------
fusermount -u "$MOUNT" 2>/dev/null || true
sleep 0.3
rm -rf "$MOUNT"
mkdir -p "$MOUNT"

# -- demo ---------------------------------------------------------------------
clear; sleep 0.5

step "mount the game archives"
cmd "cdfuse /cd /tmp/cd"
"$BINARY" "$GAME" "$MOUNT" < /dev/null &
CDFUSE_PID=$!
for _ in $(seq 1 20); do mountpoint -q "$MOUNT" 2>/dev/null && break; sleep 0.3; done
sleep 0.5

cmd "cd /tmp/cd"
cd "$MOUNT"
sleep 1.2

cmd "ls -a"
ls -a
sleep 2.5

step "localisation strings as editable JSON"
cmd "ls .paloc.jsonl/gamedata/"
ls .paloc.jsonl/gamedata/ | head -5
sleep 1.5

PALOC=$(ls .paloc.jsonl/gamedata/ | grep eng | head -1)
cmd "head -4 .paloc.jsonl/gamedata/$PALOC"
head -4 ".paloc.jsonl/gamedata/$PALOC"
sleep 2.5

sleep 5

# -- teardown -----------------------------------------------------------------
kill "$CDFUSE_PID" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
