#!/usr/bin/env bash
# Builds and installs the stub app of one Linux app (host side), so that it
# gets a launcher icon, its windows open as that app's tasks, and links in its
# URL schemes (the desktop file's x-scheme-handler types) open in it:
#   scripts/install-stub.sh org.gnome.Adwaita1.Demo
# Name and icon come from the chroot's desktop file (SVG icons become vector
# drawables in the build). ADB_SERIAL selects the device.
#
# Stubs run dev.awb's window code (see stub/build.gradle.kts), so there is
# no need to rerun this after changing dev.awb: scripts/sync-stubs.sh (run
# by deploy.sh) rebuilds a stub only when its desktop file or icon changed or
# its manifest format is outdated, and adds stubs for the apps used.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
ID=${1:?usage: install-stub.sh <desktop file id>}
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
OUT=build/stub-icons
mkdir -p "$OUT"
# Native paths for Windows tools (Gradle) under Git Bash.
path() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else echo "$1"; fi; }

# The desktop file's name, icon, schemes and stamp, worked out on the device.
"${A[@]}" push scripts/awb-stubs.sh /data/local/tmp/awb-stubs.sh >/dev/null
"${A[@]}" shell chmod 755 /data/local/tmp/awb-stubs.sh
INFO=$("${A[@]}" shell /data/local/tmp/awb-stubs.sh info "$ID" | tr -d '\r')
get() { printf '%s\n' "$INFO" | sed -n "s/^$1=//p"; }
NAME=$(get name)
SRC=$(get icon)
SCHEMES=$(get schemes)
STAMP=$(get stamp)
[ -n "$NAME" ] || { echo "no Name in $ID.desktop" >&2; exit 1; }

# No usable icon: the stub keeps its default one.
ICONARG=()
ICONFILE=$OUT/$ID.${SRC##*.}
case "$SRC" in
    *.png|*.svg)
        rm -f "$ICONFILE"
        if "${A[@]}" pull "$SRC" "$ICONFILE" >/dev/null 2>&1 && [ -s "$ICONFILE" ]; then
            ICONARG=("-Pstub.icon=$(path "$PWD/$ICONFILE")")
        else
            echo "no icon at $SRC, using the default" >&2
        fi
        ;;
    *) echo "unsupported icon $SRC, using the default" >&2 ;;
esac

echo "$ID: '$NAME', icon $SRC${SCHEMES:+, links $SCHEMES}"
./gradlew :stub:assembleDebug --console=plain -q \
    "-Pstub.appId=$ID" "-Pstub.label=$NAME" ${ICONARG[@]+"${ICONARG[@]}"} "-Pstub.schemes=$SCHEMES" "-Pstub.stamp=$STAMP"
"${A[@]}" install -r stub/build/outputs/apk/debug/stub-debug.apk
