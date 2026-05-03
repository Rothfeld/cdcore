#!/usr/bin/env bash
# Scripted cdfuse demo for asciinema.
#
# Record:  asciinema rec demo.cast --command 'bash crates/cdfuse/_demo.sh'
# Convert: agg demo.cast demo.gif
#
# Requires:
#   crates/cdfuse/target/release/cdfuse  (cargo build --release)
#   /cd                                   game directory
#   fusermount                            (apt install libfuse3)

set -euo pipefail

BINARY="$(cd "$(dirname "$0")" && pwd)/target/release/cdfuse"
GAME_DIR="/cd"
MOUNT="/tmp/cdfuse-demo"

# -- helpers ------------------------------------------------------------------

# Print a command prompt then type the string one char at a time.
type_cmd() {
    printf '\e[1;32m$ \e[0m'
    local str="$1"
    for (( i=0; i<${#str}; i++ )); do
        printf '%s' "${str:$i:1}"
        sleep 0.04
    done
    sleep 0.25
    echo
}

# Type and execute.
run() {
    type_cmd "$*"
    "$@" 2>/dev/null || true
    sleep 0.6
}

pause() { sleep "${1:-1.2}"; }

comment() {
    echo
    printf '\e[2m# %s\e[0m\n' "$1"
    sleep 0.7
}

# -- mount (invisible setup) --------------------------------------------------

mkdir -p "$MOUNT"
"$BINARY" "$GAME_DIR" "$MOUNT" < /dev/null &
CDFUSE_PID=$!

# Wait until FUSE confirms the mount.
for _ in $(seq 1 30); do
    mountpoint -q "$MOUNT" 2>/dev/null && break
    sleep 0.3
done

# -- demo ---------------------------------------------------------------------

clear
sleep 0.4

comment "cdfuse mounts Crimson Desert archives as a filesystem"

run ls "$MOUNT/"
pause

comment "Virtual directories expose binary formats without touching the archives"

run ls "$MOUNT/.paloc.jsonl/gamedata/" | head -8
pause

comment "Localisation strings as JSON lines — edit with any text editor"

PALOC=$(ls "$MOUNT/.paloc.jsonl/gamedata/" 2>/dev/null | grep "eng" | head -1)
if [ -n "$PALOC" ]; then
    type_cmd "head -3 $MOUNT/.paloc.jsonl/gamedata/$PALOC"
    head -3 "$MOUNT/.paloc.jsonl/gamedata/$PALOC" 2>/dev/null || true
    sleep 0.6
fi
pause

comment "Textures as PNG — open directly in any image editor"

run ls "$MOUNT/.dds.png/ui/" | head -6
pause

comment "Meshes as FBX — drag into Blender, Maya, or Unreal"

run ls "$MOUNT/.pam.fbx/object/" | head -6
pause

# Show source vs generated size to demonstrate on-demand rendering.
PAM=$(ls "$MOUNT/object/" 2>/dev/null | grep "\.pam$" | head -1)
if [ -n "$PAM" ]; then
    comment "FBX is generated on access — source PAM is compressed"
    type_cmd "ls -lh $MOUNT/object/$PAM $MOUNT/.pam.fbx/object/${PAM}.fbx"
    ls -lh "$MOUNT/object/$PAM" "$MOUNT/.pam.fbx/object/${PAM}.fbx" 2>/dev/null || true
    sleep 0.6
    pause
fi

comment "Edit a file and it repacks into the PAZ archive automatically on close"
pause 1.5

# -- teardown -----------------------------------------------------------------

kill "$CDFUSE_PID" 2>/dev/null || true
fusermount -u "$MOUNT" 2>/dev/null || true
rmdir "$MOUNT" 2>/dev/null || true

echo
printf '\e[2m# github.com/Rothfeld/cdcore\e[0m\n'
sleep 2
