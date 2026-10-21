#!/system/bin/sh
# Root side of the Linux app launcher (Android side, as root; deploy.sh starts
# it). Two fifos in the app's files dir, both created by Wayland Bridge:
#   launch: a desktop file id per line, optionally followed by a link, when a
#           stub app's icon is clicked or a browser opens a link in its
#           scheme; each one runs via awb-run.sh and gtk-launch.
#   open:   a link per line from the chroot's apps (awb-open, set up by
#           awb-run.sh as their browser), opened by Android (Chrome).
#   task:   window moves and resizes the apps asked for, carried out by
#           dev.awb.TaskHelper from the app's APK (apps can't move tasks).
APPDIR=/data/user/10/dev.awb/files
FIFO=$APPDIR/launch
OPEN=$APPDIR/open
echo $$ > /data/local/tmp/awb-launcher.pid

# Links out of the chroot. Only web links, and only as the data of a VIEW
# intent, so nothing in the line can become a command or an option.
(
    while :; do
        [ -p $OPEN ] || { sleep 2; continue; }
        while read -r url; do
            case "$url" in http://*|https://*) ;; *) continue ;; esac
            case "$url" in *" "*) continue ;; esac
            am start --user 10 -a android.intent.action.VIEW -d "$url" >/dev/null 2>&1
        done < $OPEN
    done
) &
OPENER=$!

# Interactive moves (xdg_toplevel.move/resize). Restarted after it exits,
# with the APK's current path (it changes on reinstall).
(
    while :; do
        APK=$(pm path --user 10 dev.awb | sed -n 's/^package://p' | head -n 1)
        [ -n "$APK" ] && CLASSPATH=$APK app_process /system/bin dev.awb.TaskHelper $APPDIR/task >/dev/null 2>&1
        sleep 2
    done
) &
MOVER=$!
trap 'kill $OPENER $MOVER; pkill -f "dev.awb.[T]askHelper"; exit' TERM INT

while :; do
    # The app creates the fifo; wait for it (first start, reinstall).
    [ -p $FIFO ] || { sleep 2; continue; }
    while read -r id uri; do
        # Ids only, never commands: gtk-launch runs the chroot's own desktop
        # file. A link (already checked by the app) is one argument.
        case "$id" in ""|*[!A-Za-z0-9._-]*) continue ;; esac
        case "$uri" in *" "*) continue ;; esac
        if [ -n "$uri" ]; then
            /data/local/tmp/awb-run.sh gtk-launch "$id" "$uri" </dev/null >/dev/null 2>&1 &
        else
            /data/local/tmp/awb-run.sh gtk-launch "$id" </dev/null >/dev/null 2>&1 &
        fi
    done < $FIFO
done
