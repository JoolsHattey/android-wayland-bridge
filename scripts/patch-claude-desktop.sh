#!/usr/bin/env bash
# Pads Claude Desktop's title bar clear of Android's caption controls (host
# side). Electron's Linux title bar overlay only keeps clear of Electron's own
# buttons, so this injects scripts/claude-desktop-insets.js at the top of the
# app's main script and repacks its app.asar in the chroot. The first run keeps
# the original as app.asar.awb-orig; rerun after Claude Desktop updates.
# Needs node (npx). ADB_SERIAL selects the device.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
RES=/data/debian/usr/lib/claude-desktop/resources
W=build/claude-asar
ASAR=(npx --yes @electron/asar@3.2.17)

# An archive without our marker is the app's own (first run or an update).
"${A[@]}" shell "grep -q awb-claude-insets $RES/app.asar || cp $RES/app.asar $RES/app.asar.awb-orig"
rm -rf "$W"
mkdir -p "$W"
"${A[@]}" pull $RES/app.asar.awb-orig "$W/app.asar" >/dev/null
"${A[@]}" pull $RES/app.asar.unpacked "$W/" >/dev/null
"${ASAR[@]}" extract "$W/app.asar" "$W/app"

MAIN=$(node -p "require('./$W/app/package.json').main")
node -e '
const fs = require("fs")
const [main, insets] = process.argv.slice(1)
const strict = "\"use strict\";"
let s = fs.readFileSync(main, "utf8")
const inject = fs.readFileSync(insets, "utf8")
s = s.startsWith(strict) ? strict + inject + s.slice(strict.length) : inject + s
fs.writeFileSync(main, s)
' "$W/app/$MAIN" scripts/claude-desktop-insets.js

# Keep the same files unpacked (native modules, binaries) as the original.
UNPACK=$(cd "$W/app.asar.unpacked" && find . -type f -exec dirname {} \; | sed 's|^\./||' | sort -u | paste -sd, -)
"${ASAR[@]}" pack "$W/app" "$W/patched.asar" --unpack-dir "{$UNPACK}"
"${A[@]}" push "$W/patched.asar" $RES/app.asar >/dev/null
echo "Patched $RES/app.asar; restart Claude Desktop to apply."
