//! The guest user's desktop configuration, as scripts/awb-run.sh sets up
//! the chroot's: GTK's header bars leave room for Android's caption,
//! web links go to Android's browser, and our settings portal backend is
//! one xdg-desktop-portal reads. All in the user's home, idempotent, done
//! at each start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::portal::BACKEND;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/home/droid".into())
}

/// Where the app's generated files (gtk.css, caption.json) go.
pub fn awb_dir() -> PathBuf {
    home().join(".config/awb")
}

/// Writes `text` to `path` (and its directory) unless it already holds it; whether it changed.
pub fn write(path: &Path, text: &[u8]) -> bool {
    if fs::read(path).is_ok_and(|old| old == text) {
        return false;
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // Through a rename: GTK may read gtk.css while we write it.
    let tmp = path.with_extension("awb-tmp");
    match fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, path)) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("awb-display: cannot write {}: {e}", path.display());
            false
        }
    }
}

/// GTK 3 and 4: Android draws the window controls. GTK keeps its own
/// (invisible, see the app's gtk.css) only so header bars leave room for
/// Android's: one button on each side, and the generated CSS imported.
/// GSettings' button-layout, which GTK prefers where its schema exists,
/// is set by awb-run.
pub fn gtk() {
    let import = format!("@import url(\"file://{}/gtk.css\");", awb_dir().display());
    for v in ["gtk-3.0", "gtk-4.0"] {
        let dir = home().join(".config").join(v);
        let ini = dir.join("settings.ini");
        let old = fs::read_to_string(&ini).unwrap_or_default();
        write(&ini, layout(&old).as_bytes());
        let css = dir.join("gtk.css");
        let old = fs::read_to_string(&css).unwrap_or_default();
        if !old.lines().any(|l| l.trim() == import) {
            write(&css, format!("{old}{}{import}\n", if old.is_empty() || old.ends_with('\n') { "" } else { "\n" }).as_bytes());
        }
    }
}

/// settings.ini with gtk-decoration-layout=close:close in its [Settings].
fn layout(ini: &str) -> String {
    const KEY: &str = "gtk-decoration-layout=close:close";
    let mut lines: Vec<String> = ini.lines().map(str::to_string).collect();
    if let Some(l) = lines.iter_mut().find(|l| l.trim_start().starts_with("gtk-decoration-layout")) {
        *l = KEY.into();
    } else if let Some(i) = lines.iter().position(|l| l.trim() == "[Settings]") {
        lines.insert(i + 1, KEY.into());
    } else {
        lines.push("[Settings]".into());
        lines.push(KEY.into());
    }
    lines.join("\n") + "\n"
}

/// `awb-open`, our binary under that name, as the default http(s) handler
/// (xdg-open, GIO) and, through awb-run, $BROWSER.
pub fn browser() {
    let Ok(exe) = std::env::current_exe() else { return };
    let link = home().join(".local/bin/awb-open");
    if fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
        let _ = fs::create_dir_all(link.parent().unwrap());
        let _ = fs::remove_file(&link);
        if let Err(e) = std::os::unix::fs::symlink(&exe, &link) {
            eprintln!("awb-display: cannot link {}: {e}", link.display());
        }
    }
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=Android browser\nExec={} %u\n\
         MimeType=x-scheme-handler/http;x-scheme-handler/https;\nNoDisplay=true\n",
        link.display()
    );
    write(&home().join(".local/share/applications/awb-android-browser.desktop"), desktop.as_bytes());
    let mimeapps = fs::read_to_string(home().join(".config/mimeapps.list")).unwrap_or_default();
    if !mimeapps.lines().any(|l| l == "x-scheme-handler/https=awb-android-browser.desktop") {
        run("xdg-mime", &["default", "awb-android-browser.desktop", "x-scheme-handler/http", "x-scheme-handler/https"]);
    }
}

/// Makes our settings backend one that xdg-desktop-portal reads, if the
/// guest has it (else we are the portal; false). It finds backends by
/// their .portal files in $XDG_DATA_DIRS, so a drop-in for its user
/// service puts ours first there, and portals.conf picks ours for Settings.
/// And only ours: GTK's backend needs a display, which the guest's services
/// lack (apps reach one only through awb-run), and starting it fails so
/// slowly (25 s per interface) that xdg-desktop-portal itself never comes
/// up. Without the other portals, apps use their own file dialogs.
/// A running portal is restarted if that changed.
pub fn portal() -> bool {
    let exe = ["/usr/libexec/xdg-desktop-portal", "/usr/lib/xdg-desktop-portal"];
    if !exe.iter().any(|p| Path::new(p).exists()) {
        return false;
    }
    let data = home().join(".local/share/awb");
    let mut changed = write(
        &data.join("xdg-desktop-portal/portals/awb.portal"),
        format!("[portal]\nDBusName={BACKEND}\nInterfaces=org.freedesktop.impl.portal.Settings;\n").as_bytes(),
    );
    let dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or("/usr/local/share:/usr/share".into());
    changed |= write(
        &home().join(".config/systemd/user/xdg-desktop-portal.service.d/awb.conf"),
        format!("# dev.awb's settings backend (awb-display).\n[Service]\nEnvironment=XDG_DATA_DIRS={}:{dirs}\n", data.display())
            .as_bytes(),
    );
    let conf = home().join(".config/xdg-desktop-portal/portals.conf");
    const MARK: &str = "# Written by awb-display";
    match fs::read_to_string(&conf) {
        Ok(old) if !old.starts_with(MARK) => {
            if !old.contains("org.freedesktop.impl.portal.Settings=awb") {
                eprintln!("awb-display: {} is not ours; add org.freedesktop.impl.portal.Settings=awb;gtk to it", conf.display());
            }
        }
        _ => {
            changed |= write(
                &conf,
                format!("{MARK} (dev.awb): Android's appearance, and no other backend.\n[preferred]\ndefault=none\norg.freedesktop.impl.portal.Settings=awb\n")
                    .as_bytes(),
            );
        }
    }
    if changed {
        run("systemctl", &["--user", "daemon-reload"]);
        run("systemctl", &["--user", "try-restart", "xdg-desktop-portal.service"]);
    }
    true
}

fn run(cmd: &str, args: &[&str]) {
    match Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::null()).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("awb-display: {cmd} {}: {s}", args.join(" ")),
        Err(e) => eprintln!("awb-display: {cmd}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::layout;

    #[test]
    fn decoration_layout() {
        assert_eq!(layout(""), "[Settings]\ngtk-decoration-layout=close:close\n");
        assert_eq!(layout("[Settings]\na=b\n"), "[Settings]\ngtk-decoration-layout=close:close\na=b\n");
        assert_eq!(layout("[Settings]\ngtk-decoration-layout=menu:close\n"), "[Settings]\ngtk-decoration-layout=close:close\n");
    }
}
