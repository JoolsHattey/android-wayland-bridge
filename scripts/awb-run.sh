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

exec /data/debian/enter env WAYLAND_DISPLAY=/run/awb/wayland-0 GDK_BACKEND=wayland "$@"
