#!/usr/bin/env bash
# Brings the stub apps in line with the chroot (host side; deploy.sh runs it
# after installing dev.awb). ADB_SERIAL selects the device.
#  - removes stubs whose Linux app (desktop file) is gone;
#  - rebuilds stubs whose manifest format is older than stub/build.gradle.kts's
#    stubFormat, or whose desktop file or icon changed since they were built;
#  - builds stubs for the Linux apps that opened windows without one
#    (dev.awb notes them in files/stub-requests) and have a desktop file.
# Changes to dev.awb's window code need none of this: stubs run it from
# dev.awb's APK.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
S=/data/local/tmp/awb-stubs.sh
REQUESTS=/data/user/10/dev.awb/files/stub-requests
FORMAT=$(sed -n 's/^val stubFormat = \([0-9]*\)$/\1/p' stub/build.gradle.kts)

"${A[@]}" push scripts/awb-stubs.sh $S >/dev/null
"${A[@]}" shell chmod 755 $S
"${A[@]}" shell $S prune | tr -d '\r'

build=()
while read -r pkg code name id; do
    [ "$id" = - ] && continue
    stamp=$("${A[@]}" shell $S info "$id" </dev/null | tr -d '\r' | sed -n 's/^stamp=//p')
    if [ "${code:-0}" -lt "$FORMAT" ]; then
        echo "$id: stub format $code, now $FORMAT"
    elif [ "$name" != "$FORMAT-$stamp" ]; then
        echo "$id: desktop file or icon changed"
    else
        continue
    fi
    build+=("$id")
done < <("${A[@]}" shell $S list | tr -d '\r')

installed=$("${A[@]}" shell $S list | tr -d '\r' | cut -d' ' -f4)
for id in $("${A[@]}" shell "cat $REQUESTS 2>/dev/null" | tr -d '\r'); do
    case "$id" in ""|*[!A-Za-z0-9._-]*) continue ;; esac
    grep -qxF "$id" <<<"$installed" && continue
    printf '%s\n' "${build[@]}" | grep -qxF "$id" && continue
    if "${A[@]}" shell $S info "$id" </dev/null >/dev/null 2>&1; then
        echo "$id: opened windows without a stub"
        build+=("$id")
    fi
done

for id in "${build[@]}"; do
    bash scripts/install-stub.sh "$id"
done
# Handled: built now, or without a desktop file to build from.
"${A[@]}" shell "rm -f $REQUESTS"
