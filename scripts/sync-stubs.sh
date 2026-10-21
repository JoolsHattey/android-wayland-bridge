#!/usr/bin/env bash
# Brings the stub apps in line with the chroot now (host side; deploy.sh
# runs it after installing dev.awb). The device does the work (awb-stubs.sh
# sync, which the root launcher also runs on its own): it removes stubs
# whose Linux app is gone, remakes those of an older format or a changed
# desktop file or icon, and makes stubs for the apps that opened windows
# without one or were installed in the chroot. ADB_SERIAL selects the device.
set -euo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
A=(adb -s "${ADB_SERIAL:-192.168.0.40:5555}")
"${A[@]}" push scripts/awb-stubs.sh /data/local/tmp/awb-stubs.sh >/dev/null
"${A[@]}" shell 'chmod 755 /data/local/tmp/awb-stubs.sh && /data/local/tmp/awb-stubs.sh sync' | tr -d '\r'
