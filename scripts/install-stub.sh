#!/usr/bin/env bash
# Builds and installs the stub app of one Linux app (host side), so that it
# gets a launcher icon, its windows open as that app's tasks, and links in its
# URL schemes (the desktop file's x-scheme-handler types) open in it:
#   scripts/install-stub.sh org.gnome.Adwaita1.Demo
# Name and icon come from the chroot's desktop file. SVG icons are rendered
# with a headless Chrome or Edge. ADB_SERIAL selects the device.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
ID=${1:?usage: install-stub.sh <desktop file id>}
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
R=/data/debian
OUT=build/stub-icons
mkdir -p "$OUT"
# Native paths for Windows tools (Gradle, the browser) under Git Bash.
path() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else echo "$1"; fi; }

DESKTOP=$("${A[@]}" shell cat "$R/usr/share/applications/$ID.desktop" | tr -d '\r')
# First [Desktop Entry] keys, untranslated.
key() { printf '%s\n' "$DESKTOP" | sed -n '/^\[Desktop Entry\]/,/^\[/p' | grep -m1 "^$1=" | cut -d= -f2-; }
NAME=$(key Name)
ICON=$(key Icon)
[ -n "$NAME" ] || { echo "no Name in $ID.desktop" >&2; exit 1; }
SCHEMES=$( (key MimeType || true) | tr ';' '\n' | sed -n 's|^x-scheme-handler/||p' | paste -sd, -)

# The largest PNG in hicolor, else the scalable SVG.
SRC=
if [[ "$ICON" == /* ]]; then
    SRC=$R$ICON
else
    for size in 512x512 256x256 192x192 128x128 96x96 64x64 48x48; do
        f=$R/usr/share/icons/hicolor/$size/apps/$ICON.png
        if "${A[@]}" shell "[ -f '$f' ]"; then SRC=$f; break; fi
    done
    [ -n "$SRC" ] || SRC=$R/usr/share/icons/hicolor/scalable/apps/$ICON.svg
fi
PNG=$OUT/$ID.png
case "$SRC" in
    *.png) "${A[@]}" pull "$SRC" "$PNG" >/dev/null ;;
    *.svg)
        "${A[@]}" pull "$SRC" "$OUT/$ID.svg" >/dev/null
        printf '<html><body style="margin:0"><img src="%s.svg" style="width:256px;height:256px;display:block"></body></html>' \
            "$ID" > "$OUT/$ID.html"
        BROWSER=
        for b in "/c/Program Files/Google/Chrome/Application/chrome.exe" \
                 "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
                 "$(command -v chromium || true)" "$(command -v google-chrome || true)"; do
            [ -n "$b" ] && [ -x "$b" ] && { BROWSER=$b; break; }
        done
        [ -n "$BROWSER" ] || { echo "no Chrome/Edge to render $SRC" >&2; exit 1; }
        "$BROWSER" --headless --disable-gpu --hide-scrollbars --window-size=256,256 \
            --default-background-color=00000000 --screenshot="$(path "$PWD/$PNG")" \
            "file:///$(path "$PWD/$OUT/$ID.html")" >/dev/null 2>&1
        ;;
    *) echo "unsupported icon $SRC" >&2; exit 1 ;;
esac
[ -s "$PNG" ] || { echo "no icon rendered from $SRC" >&2; exit 1; }

echo "$ID: '$NAME', icon $SRC${SCHEMES:+, links $SCHEMES}"
./gradlew :stub:assembleDebug --console=plain -q \
    "-Pstub.appId=$ID" "-Pstub.label=$NAME" "-Pstub.icon=$(path "$PWD/$PNG")" "-Pstub.schemes=$SCHEMES"
"${A[@]}" install -r stub/build/outputs/apk/debug/stub-debug.apk
