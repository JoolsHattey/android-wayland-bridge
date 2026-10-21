#!/system/bin/sh
# Run a Linux app from the Debian chroot on the awb compositor (Android side,
# as root): awb-run.sh <command...>
# Bind-mounts the app's socket directory into the chroot at /run/awb.
R=/data/debian
APPDIR=/data/user/10/dev.awb/files

/data/debian/enter true || exit 1   # sets up the chroot mounts once per boot
mkdir -p $R/run/awb
mountpoint -q $R/run/awb || mount -o bind $APPDIR $R/run/awb
[ -S $APPDIR/wayland-0 ] || { echo "awb is not running (open Wayland Bridge)" >&2; exit 1; }

# GTK: Android draws the window controls. Keep GTK's (invisible, see the
# generated /run/awb/gtk.css) only so header bars leave room for Android's.
CFG=$R/root/.config/gtk-4.0
mkdir -p $CFG
if grep -q '^gtk-decoration-layout' $CFG/settings.ini 2>/dev/null; then
    sed -i 's/^gtk-decoration-layout=.*/gtk-decoration-layout=close:close/' $CFG/settings.ini
else
    printf '[Settings]\ngtk-decoration-layout=close:close\n' >> $CFG/settings.ini
fi
IMPORT='@import url("file:///run/awb/gtk.css");'
grep -qF "$IMPORT" $CFG/gtk.css 2>/dev/null || echo "$IMPORT" >> $CFG/gtk.css

# Session bus for the apps, in the app's directory (/run/awb/bus here). The
# compositor connects to it from Android to serve the settings portal (dark
# mode, accent colour). dbus-daemon only admits uids it can resolve, so the
# app's uid becomes the system user "awb" (kept in sync across reinstalls).
AWB_UID=$(stat -c %u $APPDIR)
for f in passwd group; do
    sed -i '/^awb:/d' $R/etc/$f
done
echo "awb:x:$AWB_UID:$AWB_UID:Wayland Bridge compositor:/nonexistent:/usr/sbin/nologin" >> $R/etc/passwd
echo "awb:x:$AWB_UID:" >> $R/etc/group
BUSCONF=$R/etc/dbus-1/awb-session.conf
cat > $BUSCONF <<'XML'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=/run/awb/bus</listen>
  <auth>EXTERNAL</auth>
  <standard_session_servicedirs/>
  <policy context="default">
    <allow user="root"/>
    <allow user="awb"/>
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
XML
BUS=unix:path=/run/awb/bus
# Under a lock (-o: the daemon must not inherit it), so concurrent launches
# start one daemon. GTK looks for the settings portal once, at start-up, so
# then wait for the compositor (which retries every second) to register.
/data/debian/enter flock -o /run/awb-bus.lock sh -c '
    BUS=unix:path=/run/awb/bus
    if ! dbus-send --bus=$BUS --dest=org.freedesktop.DBus / org.freedesktop.DBus.Peer.Ping 2>/dev/null; then
        rm -f /run/awb/bus
        dbus-daemon --config-file=/etc/dbus-1/awb-session.conf --fork --nopidfile --nosyslog >/dev/null
    fi
    # Apps the bus starts (DBusActivatable desktop files, such as GNOME
    # Console) get its environment, which /data/debian/enter pointed at the
    # Terminal app'"'"'s compositor.
    DBUS_SESSION_BUS_ADDRESS=$BUS dbus-update-activation-environment WAYLAND_DISPLAY=/run/awb/wayland-0 GDK_BACKEND=wayland \
        DBUS_SESSION_BUS_ADDRESS=$BUS GTK_A11Y=none >/dev/null 2>&1
    for i in 1 2 3 4 5 6 7 8 9 10; do
        dbus-send --bus=$BUS --print-reply --dest=org.freedesktop.DBus / org.freedesktop.DBus.NameHasOwner \
            string:org.freedesktop.portal.Desktop 2>/dev/null | grep -q true && break
        sleep 0.3
    done'

# Web links open in Android's browser: awb-open writes them to the app's open
# fifo, and awb-launcher.sh hands them to Android. It is the chroot's default
# http(s) handler (xdg-open) and $BROWSER.
cat > $R/usr/local/bin/awb-open <<'SH'
#!/bin/sh
# Opens web links in Android's browser (see awb-run.sh, awb-launcher.sh).
for url in "$@"; do
    case "$url" in
        http://*|https://*) printf '%s\n' "$url" | timeout 2 tee -a /run/awb/open >/dev/null ||
            echo "awb-open: the Android launcher is not running" >&2 ;;
        *) echo "awb-open: not a web link: $url" >&2 ;;
    esac
done
SH
chmod 755 $R/usr/local/bin/awb-open
mkdir -p $R/usr/local/share/applications
cat > $R/usr/local/share/applications/awb-android-browser.desktop <<'INI'
[Desktop Entry]
Type=Application
Name=Android browser
Exec=/usr/local/bin/awb-open %u
MimeType=x-scheme-handler/http;x-scheme-handler/https;
NoDisplay=true
INI
grep -q '^x-scheme-handler/https=awb-android-browser.desktop' $R/root/.config/mimeapps.list 2>/dev/null ||
    /data/debian/enter xdg-mime default awb-android-browser.desktop x-scheme-handler/http x-scheme-handler/https

exec /data/debian/enter env WAYLAND_DISPLAY=/run/awb/wayland-0 GDK_BACKEND=wayland DBUS_SESSION_BUS_ADDRESS=$BUS \
    GTK_A11Y=none BROWSER=/usr/local/bin/awb-open "$@"
