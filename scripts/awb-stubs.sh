#!/system/bin/sh
# The stub apps' bookkeeping (Android side, as root; pushed by deploy.sh and
# install-stub.sh). Maps the chroot's desktop files to stub packages:
#   info <id>  the stub of desktop file <id>, as key=value lines: pkg, file,
#              name, icon (the file the stub's icon comes from), schemes and
#              stamp (a hash of the desktop file and the icon: a stub built
#              from other ones is out of date).
#   list       one line per installed stub: package, versionCode,
#              versionName and its desktop file id ("-" if it has none).
#   prune      uninstalls the stubs whose desktop file is gone (the Linux
#              app was removed). awb-launcher.sh runs this now and then.
R=/data/debian
DIRS="$R/usr/local/share/applications $R/usr/share/applications"

# dev.awb.app.<id>, made a valid Java name, as stub/build.gradle.kts does.
pkg() {
    echo "dev.awb.app.$1" | sed 's/[^A-Za-z0-9_.]/_/g; s/\.\([^A-Za-z]\)/._\1/g'
}

desktop() {
    for d in $DIRS; do
        [ -f "$d/$1.desktop" ] && { echo "$d/$1.desktop"; return 0; }
    done
    return 1
}

# The desktop ids of the installed stubs' packages, as "pkg id" lines.
ids() {
    for d in $DIRS; do
        for f in "$d"/*.desktop; do
            [ -f "$f" ] || continue
            id=${f##*/}; id=${id%.desktop}
            echo "$(pkg "$id") $id"
        done
    done
}

# First [Desktop Entry] key $2 of desktop file $1, untranslated.
key() {
    sed -n '/^\[Desktop Entry\]/,/^\[/p' "$1" | grep -m1 "^$2=" | cut -d= -f2- | tr -d '\r'
}

# The largest PNG in hicolor, else the scalable SVG.
icon() {
    case "$1" in
        /*) echo "$R$1"; return ;;
    esac
    for size in 512x512 256x256 192x192 128x128 96x96 64x64 48x48; do
        f=$R/usr/share/icons/hicolor/$size/apps/$1.png
        [ -f "$f" ] && { echo "$f"; return; }
    done
    echo "$R/usr/share/icons/hicolor/scalable/apps/$1.svg"
}

case "$1" in
info)
    f=$(desktop "$2") || { echo "no desktop file for $2" >&2; exit 1; }
    i=$(icon "$(key "$f" Icon)")
    echo "pkg=$(pkg "$2")"
    echo "file=$f"
    echo "name=$(key "$f" Name)"
    echo "icon=$i"
    echo "schemes=$(key "$f" MimeType | tr ';' '\n' | sed -n 's|^x-scheme-handler/||p' | paste -sd, -)"
    echo "stamp=$( { cat "$f"; cat "$i" 2>/dev/null; } | sha256sum | cut -c1-12)"
    ;;
list|prune)
    MAP=$(ids)
    # No desktop files at all: the chroot is missing, not every app.
    [ -n "$MAP" ] || { echo "no desktop files under $R" >&2; exit 1; }
    pm list packages dev.awb.app. | sed -n 's/^package://p' | while read -r p; do
        id=$(echo "$MAP" | while read -r mp mid; do [ "$mp" = "$p" ] && { echo "$mid"; break; }; done)
        if [ "$1" = list ]; then
            v=$(dumpsys package "$p" | sed -n 's/^ *versionCode=\([0-9]*\).*/\1/p; s/^ *versionName=//p' | head -n 2 | paste -sd' ' -)
            echo "$p $v ${id:--}"
        elif [ -z "$id" ]; then
            echo "removing $p: its desktop file is gone"
            pm uninstall "$p" >/dev/null
        fi
    done
    ;;
*)
    echo "usage: awb-stubs.sh info <desktop id> | list | prune" >&2
    exit 2
    ;;
esac
