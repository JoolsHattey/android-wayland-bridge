#!/system/bin/sh
# Root side of the Linux app launcher (Android side, as root; deploy.sh starts
# it). Wayland Bridge writes a desktop file id per line to the files/launch
# fifo when a stub app's icon is clicked; each one runs via awb-run.sh.
APPDIR=/data/user/10/dev.awb/files
FIFO=$APPDIR/launch
echo $$ > /data/local/tmp/awb-launcher.pid

while :; do
    # The app creates the fifo; wait for it (first start, reinstall).
    [ -p $FIFO ] || { sleep 2; continue; }
    while read -r id; do
        # Ids only, never commands: gtk-launch runs the chroot's own desktop file.
        case "$id" in ""|*[!A-Za-z0-9._-]*) continue ;; esac
        /data/local/tmp/awb-run.sh gtk-launch "$id" </dev/null >/dev/null 2>&1 &
    done < $FIFO
done
