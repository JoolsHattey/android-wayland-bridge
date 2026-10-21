#!/system/bin/sh
# The stub apps' bookkeeping, and their making (Android side, as root;
# pushed by deploy.sh, run by awb-launcher.sh). Maps the chroot's desktop
# files to stub packages:
#   info <id>  the stub of desktop file <id>, as key=value lines: pkg, file,
#              name, icon (the file the stub's icon comes from), schemes and
#              stamp (a hash of the desktop file and the icon: a stub built
#              from other ones is out of date).
#   list       one line per installed stub: package, versionCode,
#              versionName and its desktop file id ("-" if it has none).
#   prune      uninstalls the stubs whose desktop file is gone (the Linux
#              app was removed).
#   make <id>  makes and installs the stub of desktop file <id> here, from
#              the template in dev.awb's APK (dev.awb.StubMaker), signed
#              with the key deploy.sh put in /data/adb/awb.
#   sync       prunes; remakes stubs of an older format than dev.awb's or
#              of a changed desktop file or icon; makes stubs for the apps
#              that opened windows without one (dev.awb notes them in
#              files/stub-requests) and for apps installed in the chroot
#              since the last sync. awb-launcher.sh runs this.
R=/data/debian
DIRS="$R/usr/local/share/applications $R/usr/share/applications"
KEYS=/data/adb/awb
REQUESTS=/data/user/10/dev.awb/files/stub-requests
TMP=/data/local/tmp

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

# Whether desktop file $1 is an app a launcher shows (not a helper, a
# handler, a terminal program or another desktop's part).
shown() {
    s=$(sed -n '/^\[Desktop Entry\]/,/^\[/p' "$1")
    echo "$s" | grep -q '^Type=Application' && echo "$s" | grep -q '^Exec=' &&
        ! echo "$s" | grep -qE '^(NoDisplay|Hidden|Terminal)=true' && ! echo "$s" | grep -q '^OnlyShowIn='
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

info() {
    f=$(desktop "$1") || { echo "no desktop file for $1" >&2; return 1; }
    i=$(icon "$(key "$f" Icon)")
    echo "pkg=$(pkg "$1")"
    echo "file=$f"
    echo "name=$(key "$f" Name)"
    echo "icon=$i"
    echo "schemes=$(key "$f" MimeType | tr ';' '\n' | sed -n 's|^x-scheme-handler/||p' | paste -sd, -)"
    echo "stamp=$( { cat "$f"; cat "$i" 2>/dev/null; } | sha256sum | cut -c1-12)"
}

# The installed stubs, as "pkg versionCode versionName id" lines.
stubs() {
    MAP=$(ids)
    # No desktop files at all: the chroot is missing, not every app.
    [ -n "$MAP" ] || { echo "no desktop files under $R" >&2; return 1; }
    pm list packages dev.awb.app. | sed -n 's/^package://p' | while read -r p; do
        id=$(echo "$MAP" | while read -r mp mid; do [ "$mp" = "$p" ] && { echo "$mid"; break; }; done)
        v=$(dumpsys package "$p" | sed -n 's/^ *versionCode=\([0-9]*\).*/\1/p; s/^ *versionName=//p' | head -n 2 | paste -sd' ' -)
        echo "$p $v ${id:--}"
    done
}

# dev.awb.StubMaker, from dev.awb's installed APK (which has the template).
maker() {
    APK=$(pm path --user 10 dev.awb | sed -n 's/^package://p' | head -n 1)
    [ -n "$APK" ] || { echo "dev.awb is not installed" >&2; return 1; }
    CLASSPATH=$APK app_process /system/bin dev.awb.StubMaker "$@"
}

# Makes and installs the stub of desktop file $1, of stub format $2.
make_stub() {
    [ -f $KEYS/stub-key.pk8 ] || { echo "no signing key in $KEYS (deploy.sh pushes it)" >&2; return 1; }
    INFO=$(info "$1") || return 1
    get() { echo "$INFO" | sed -n "s/^$1=//p"; }
    name=$(get name)
    src=$(get icon)
    [ -n "$name" ] || { echo "no Name in $1.desktop" >&2; return 1; }
    png=
    case "$src" in
        *.png) [ -s "$src" ] && png=$src ;;
        *.svg)
            # Android has no SVG renderer: librsvg's, in the chroot.
            if [ ! -s "$src" ]; then
                :
            elif [ -x $R/usr/bin/rsvg-convert ]; then
                out=/tmp/awb-stub-icon-$$.png
                $R/enter rsvg-convert -w 256 -h 256 -a -o $out "${src#$R}" 2>/dev/null && png=$R$out
            else
                echo "$1: no rsvg-convert for its SVG icon (apt install librsvg2-bin), using the default" >&2
            fi
            ;;
    esac
    [ -n "$png" ] || echo "$1: no icon at $src, using the default" >&2
    apk=$TMP/awb-stub-$$.apk
    maker make $apk $KEYS/stub-key.pk8 $KEYS/stub-cert.der "$(get pkg)" "$1" "$name" "$2-$(get stamp)" "$(get schemes)" $png &&
        pm install -r $apk >/dev/null
    ok=$?
    rm -f $apk $R/tmp/awb-stub-icon-$$.png
    [ $ok = 0 ] && echo "$1: made $(get pkg), '$name'${png:+ with its icon}"
    return $ok
}

# The sync command, under the lock on fd 9: one at a time (the launcher,
# deploy.sh).
sync_stubs() {
    flock 9
    FORMAT=$(maker format) || return 1
    STUBS=$(stubs) || return 1
    made=" "
    remake() { make_stub "$1" "$FORMAT" && made="$made$1 "; }

    echo "$STUBS" | while read -r p code name id; do
        [ -n "$p" ] || continue
        if [ "$id" = - ]; then
            echo "removing $p: its desktop file is gone"
            pm uninstall "$p" >/dev/null
        elif [ "${code:-0}" -lt "$FORMAT" ]; then
            echo "$id: stub format $code, now $FORMAT"
            make_stub "$id" "$FORMAT"
        elif [ "$name" != "$FORMAT-$(info "$id" | sed -n 's/^stamp=//p')" ]; then
            echo "$id: desktop file or icon changed"
            make_stub "$id" "$FORMAT"
        fi
    done
    have() { echo "$STUBS" | cut -d' ' -f4 | grep -qxF "$1" || case "$made" in *" $1 "*) true ;; *) false ;; esac; }

    # Apps that opened windows without a stub. Those without a desktop
    # file (helpers, portals) stay listed, so dev.awb does not add them again.
    if [ -f $REQUESTS ]; then
        keep=
        for id in $(cat $REQUESTS); do
            case "$id" in ""|*[!A-Za-z0-9._-]*) continue ;; esac
            if have "$id"; then
                :
            elif ! desktop "$id" >/dev/null; then
                keep="$keep$id\n"
            else
                echo "$id: opened windows without a stub"
                remake "$id" || keep="$keep$id\n"
            fi
        done
        # In place: the file stays dev.awb's.
        printf "$keep" > $REQUESTS
    fi

    # Apps installed since the last sync (the first one only takes note).
    KNOWN=$KEYS/known-apps
    apps=$(for d in $DIRS; do
        for f in "$d"/*.desktop; do
            [ -f "$f" ] && shown "$f" && { f=${f##*/}; echo "${f%.desktop}"; }
        done
    done | sort -u)
    if [ -f $KNOWN ]; then
        for id in $apps; do
            grep -qxF "$id" $KNOWN && continue
            have "$id" && continue
            echo "$id: installed"
            remake "$id"
        done
    fi
    echo "$apps" > $KNOWN
}

case "$1" in
info)
    info "$2"
    ;;
list)
    stubs
    ;;
prune)
    stubs | while read -r p code name id; do
        [ "$id" = - ] || continue
        echo "removing $p: its desktop file is gone"
        pm uninstall "$p" >/dev/null
    done
    ;;
make)
    FORMAT=$(maker format) || exit 1
    make_stub "$2" "$FORMAT"
    ;;
sync)
    # A redirect, not exec: mksh would close it for flock.
    sync_stubs 9>$TMP/awb-stubs.lock
    ;;
*)
    echo "usage: awb-stubs.sh info <desktop id> | list | prune | make <desktop id> | sync" >&2
    exit 2
    ;;
esac
