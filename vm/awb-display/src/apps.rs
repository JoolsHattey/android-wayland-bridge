//! The guest's apps, for launcher icons on Android (stub apps, made by the
//! root side as for the chroot's apps, see scripts/awb-stubs.sh): the
//! desktop files a launcher would show, with their names, URL schemes and
//! icons as PNGs, and their launch, by desktop id, under awb-run.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::setup::home;

#[derive(Clone, PartialEq)]
pub struct App {
    /// Desktop file id, also the app's Wayland app id for GTK apps.
    pub id: String,
    pub name: String,
    /// The x-scheme-handler types of its MimeType.
    pub schemes: Vec<String>,
    pub icon: Option<Vec<u8>>,
}

/// Icons of more than this are left out (the stub gets its default icon).
const MAX_ICON: usize = 1 << 20;

/// $XDG_DATA_HOME, then $XDG_DATA_DIRS: earlier ones override later ones.
fn data_dirs() -> Vec<PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"));
    let system = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or("/usr/local/share:/usr/share".into());
    std::iter::once(user).chain(system.split(':').map(PathBuf::from)).collect()
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The desktop files by id, the first of each id in data_dirs order.
fn desktop_files() -> Vec<(String, PathBuf)> {
    let mut seen = HashMap::new();
    let mut out = Vec::new();
    for dir in data_dirs() {
        let Ok(entries) = fs::read_dir(dir.join("applications")) else { continue };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".desktop")) else { continue };
            if valid_id(id) && seen.insert(id.to_string(), ()).is_none() {
                out.push((id.to_string(), path));
            }
        }
    }
    out
}

/// A change stamp of the desktop files: their paths, sizes and times.
pub fn stamp() -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut v: Vec<_> = data_dirs()
        .into_iter()
        .map(|d| d.join("applications"))
        .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .map(|p| {
            let m = fs::metadata(&p).ok();
            let (len, time) = m.map_or((0, None), |m| (m.len(), m.modified().ok()));
            (p, len, time)
        })
        .collect();
    v.sort();
    v
}

/// The [Desktop Entry] group's untranslated keys, with string escapes undone.
fn entry(path: &Path) -> Option<HashMap<String, String>> {
    let text = fs::read_to_string(path).ok()?;
    let mut keys = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            inside = line == "[Desktop Entry]";
            continue;
        }
        if !inside || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.contains('[') {
                keys.entry(k.to_string()).or_insert_with(|| unescape(v.trim()));
            }
        }
    }
    Some(keys)
}

fn unescape(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

/// Whether a launcher shows it (not a helper, a handler, a terminal
/// program or another desktop's part), as awb-stubs.sh decides for the chroot.
fn shown(keys: &HashMap<String, String>) -> bool {
    let t = |k: &str| keys.get(k).is_some_and(|v| v == "true");
    keys.get("Type").is_some_and(|v| v == "Application")
        && keys.contains_key("Exec")
        && !t("NoDisplay")
        && !t("Hidden")
        && !t("Terminal")
        && !keys.contains_key("OnlyShowIn")
}

/// The apps a launcher would show, sorted by id. `icons` caches rendered
/// icons by source file and time (rendering an SVG takes a process).
pub fn scan(icons: &mut HashMap<PathBuf, (Option<SystemTime>, Option<Vec<u8>>)>) -> Vec<App> {
    let mut apps = Vec::new();
    for (id, path) in desktop_files() {
        let Some(keys) = entry(&path) else { continue };
        if !shown(&keys) {
            continue;
        }
        let Some(name) = keys.get("Name").filter(|n| !n.is_empty() && !n.contains('\n')) else { continue };
        let schemes = keys
            .get("MimeType")
            .map(|m| m.split(';').filter_map(|t| t.strip_prefix("x-scheme-handler/")).filter(|s| scheme_ok(s)).map(str::to_string).collect())
            .unwrap_or_default();
        let icon = keys.get("Icon").and_then(|i| find_icon(i)).and_then(|src| {
            let time = fs::metadata(&src).and_then(|m| m.modified()).ok();
            if let Some((_, png)) = icons.get(&src).filter(|(t, _)| *t == time) {
                return png.clone();
            }
            let png = png(&src).filter(|p| p.len() <= MAX_ICON);
            icons.insert(src, (time, png.clone()));
            png
        });
        apps.push(App { id, name: name.clone(), schemes, icon });
    }
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    apps
}

/// A scheme the app's stub may claim on Android: not web links, which are
/// Android's browser's (the VM's own go there too, through awb-open).
fn scheme_ok(s: &str) -> bool {
    !matches!(s, "http" | "https")
        && s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
}

/// The icon file: a path, else the largest PNG in hicolor, else its SVG,
/// else one in pixmaps.
fn find_icon(icon: &str) -> Option<PathBuf> {
    if icon.starts_with('/') {
        return Some(PathBuf::from(icon)).filter(|p| p.is_file());
    }
    if !valid_id(icon) {
        return None;
    }
    let roots: Vec<PathBuf> = std::iter::once(home().join(".icons")).chain(data_dirs().into_iter().map(|d| d.join("icons"))).collect();
    for size in ["512x512", "256x256", "192x192", "128x128", "96x96", "64x64", "48x48"] {
        for r in &roots {
            let f = r.join("hicolor").join(size).join("apps").join(format!("{icon}.png"));
            if f.is_file() {
                return Some(f);
            }
        }
    }
    for r in &roots {
        let f = r.join("hicolor/scalable/apps").join(format!("{icon}.svg"));
        if f.is_file() {
            return Some(f);
        }
    }
    ["png", "svg"].iter().map(|e| PathBuf::from(format!("/usr/share/pixmaps/{icon}.{e}"))).find(|f| f.is_file())
}

/// The icon as PNG. Android has no SVG renderer, so SVGs are rendered
/// here: by librsvg's rsvg-convert (librsvg2-bin) at 256 px if the guest
/// has it, else by GTK's image tool at the SVG's own size (128 px for
/// GNOME's app icons).
fn png(src: &Path) -> Option<Vec<u8>> {
    let data = fs::read(src).ok()?;
    if data.starts_with(b"\x89PNG") {
        return Some(data);
    }
    if src.extension().is_none_or(|e| e != "svg") {
        return None;
    }
    let out = crate::runtime_dir().join(format!("awb-icon-{}.png", std::process::id()));
    let _ = fs::remove_file(&out);
    let quiet = |c: &mut Command| c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    let ok = quiet(Command::new("rsvg-convert").args(["-w", "256", "-h", "256", "-a", "-o"]).arg(&out).arg(src))
        || quiet(Command::new("gtk4-image-tool").arg("convert").arg(src).arg(&out));
    let png = fs::read(&out).ok().filter(|p| ok && p.starts_with(b"\x89PNG"));
    let _ = fs::remove_file(&out);
    if png.is_none() {
        eprintln!("awb-display: cannot render {} (apt install librsvg2-bin)", src.display());
    }
    png
}

/// Runs app `id` (its desktop file's Exec, with `uri` for its %u) under
/// awb-run, so its windows reach the compositor.
pub fn launch(id: &str, uri: Option<&str>) {
    let Some((_, path)) = desktop_files().into_iter().find(|(i, _)| i == id) else {
        eprintln!("awb-display: launch {id}: no desktop file");
        return;
    };
    let Some(keys) = entry(&path) else { return };
    let Some(argv) = keys.get("Exec").and_then(|e| exec(e, uri, &keys, &path)).filter(|a| !a.is_empty()) else {
        eprintln!("awb-display: launch {id}: bad Exec");
        return;
    };
    eprintln!("awb-display: launch {id}: {}", argv.join(" "));
    let run = if Path::new("/usr/local/bin/awb-run").exists() { "/usr/local/bin/awb-run" } else { "awb-run" };
    let mut cmd = Command::new(run);
    cmd.args(&argv).stdin(Stdio::null()).stdout(Stdio::null());
    if let Some(dir) = keys.get("Path").filter(|p| Path::new(p).is_dir()) {
        cmd.current_dir(dir);
    } else {
        cmd.current_dir(home());
    }
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    match cmd.spawn() {
        // Reaped on its own thread: the app runs for as long as it likes.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("awb-display: launch {id}: {e}"),
    }
}

/// The desktop entry spec's Exec as argv: quoting undone, field codes
/// expanded (the link for %u/%f and their lists, nothing else from outside).
fn exec(line: &str, uri: Option<&str>, keys: &HashMap<String, String>, path: &Path) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        let Some(&first) = chars.peek() else { break };
        let mut word = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => word.push(chars.next()?),
                    c => word.push(c),
                }
            }
            // A quoted word is literal (field codes are not allowed in it).
            words.push((word, true));
            continue;
        }
        while let Some(c) = chars.next_if(|c| *c != ' ' && *c != '\t') {
            word.push(c);
        }
        words.push((word, false));
    }
    let mut argv = Vec::new();
    for (word, quoted) in words {
        if quoted {
            argv.push(word);
            continue;
        }
        match word.as_str() {
            "%f" | "%F" | "%u" | "%U" => argv.extend(uri.map(str::to_string)),
            "%i" => {
                if let Some(i) = keys.get("Icon") {
                    argv.push("--icon".into());
                    argv.push(i.clone());
                }
            }
            _ => {
                let mut out = String::new();
                let mut cs = word.chars();
                while let Some(c) = cs.next() {
                    if c != '%' {
                        out.push(c);
                        continue;
                    }
                    match cs.next() {
                        Some('%') => out.push('%'),
                        Some('f' | 'F' | 'u' | 'U') => out.push_str(uri.unwrap_or("")),
                        Some('c') => out.push_str(keys.get("Name").map_or("", |s| s)),
                        Some('k') => out.push_str(&path.to_string_lossy()),
                        _ => {}
                    }
                }
                argv.push(out);
            }
        }
    }
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str, uri: Option<&str>) -> Vec<String> {
        exec(line, uri, &HashMap::new(), Path::new("/x.desktop")).unwrap()
    }

    #[test]
    fn exec_lines() {
        assert_eq!(argv("gnome-text-editor %U", None), ["gnome-text-editor"]);
        assert_eq!(argv("claude-desktop %U", Some("claude://x")), ["claude-desktop", "claude://x"]);
        assert_eq!(argv(r#"sh -c "echo \"a b\"" %%x"#, None), ["sh", "-c", "echo \"a b\"", "%x"]);
        assert_eq!(argv("featherpad --win", None), ["featherpad", "--win"]);
        assert!(exec("\"unterminated", None, &HashMap::new(), Path::new("/x")).is_none());
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape(r"a\sb\\c"), r"a b\c");
    }
}
