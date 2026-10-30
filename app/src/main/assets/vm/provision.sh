#!/bin/sh
# Sets the guest up for dev.awb: run as root by cloud-init from the cidata
# disk that dev.awb.VmCiData writes, on first boot and again whenever that
# disk's content changes (its instance-id is a hash of it). Safe to rerun.
# awb-display configures the rest (GTK, portals) itself when it starts.
set -u
D=$(dirname "$0")
install -m 755 "$D/awb-display" "$D/awb-run" /usr/local/bin/
install -m 644 "$D/awb-display.service" /etc/systemd/user/awb-display.service
usermod -aG render droid

# awb-display runs in droid's user manager, which must run from boot
# without a login: lingering. On boot the manager starts after this (and
# then the unit); if it is already up, restart the unit on the new binary.
systemctl --global enable awb-display.service
loginctl enable-linger droid
if systemctl --user -M droid@ daemon-reload 2>/dev/null; then
    systemctl --user -M droid@ restart awb-display.service
fi

# waypipe for awb-run (0.9.2 in trixie, the same as the APK's). Needs the
# network, which may still be coming up.
for i in 1 2 3 4 5; do
    apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y waypipe && break
    sleep 10
done
echo "awb provision done"
