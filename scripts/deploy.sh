#!/usr/bin/env bash
# Dev loop (host side): build, install, restart the compositor, push the
# chroot launcher and restart the root side of the app launcher. ADB_SERIAL
# selects the device (default 192.168.0.40:5555).
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
./gradlew.bat :app:assembleDebug --console=plain -q
"${A[@]}" install -r app/build/outputs/apk/debug/app-debug.apk
"${A[@]}" push scripts/awb-run.sh /data/local/tmp/awb-run.sh >/dev/null
"${A[@]}" push scripts/awb-launcher.sh /data/local/tmp/awb-launcher.sh >/dev/null
"${A[@]}" shell 'chmod 755 /data/local/tmp/awb-run.sh /data/local/tmp/awb-launcher.sh
P=/data/local/tmp/awb-launcher.pid
# Its whole process group (setsid): the subshells and the task helper too.
[ -f $P ] && kill -- -$(cat $P) 2>/dev/null
nohup setsid /data/local/tmp/awb-launcher.sh </dev/null >/dev/null 2>&1 &
appops set --user 10 dev.awb SYSTEM_ALERT_WINDOW allow
am force-stop --user 10 dev.awb
am start-foreground-service --user 10 -n dev.awb/.WaylandService >/dev/null'
