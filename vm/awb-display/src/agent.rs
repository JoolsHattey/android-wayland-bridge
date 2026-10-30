//! The control connection (see dev.awb.VmAgent): the app connects to vsock
//! port 7006 while the guest boots and keeps the connection open; a new one
//! replaces the last. Messages are lines of space-separated ASCII words; a
//! message with a payload ends in its byte lengths, and the payloads follow
//! the line.
//!
//! App to guest:
//!   appearance <dark 0|1> <accent rrggbb|->   the settings portal's values
//!   file <name> <len>                         gtk.css or caption.json, for ~/.config/awb
//!   launch <desktop id> [<link>]              runs the app under awb-run
//! Guest to app:
//!   hello 1                                   on connecting (protocol version)
//!   open <link>                               an http(s) link, for Android's browser
//!   apps begin, then per app
//!   app <desktop id> <info len> <png len>,    info: name=, schemes= lines; png: its icon
//!   apps end                                  the whole list, sent on connecting and on changes

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::apps::{self, App};
use crate::portal::{self, Appearance, Portal};
use crate::setup;

pub const PORT: u32 = 7006;
/// Our own processes' way in (awb-open), in $XDG_RUNTIME_DIR.
const SOCKET: &str = "awb-agent.sock";
const MAX_FILE: usize = 1 << 20;

#[derive(Default)]
struct Agent {
    /// The app's current connection (with its number), for writing.
    conn: Mutex<Option<(u64, File)>>,
    portal: Arc<Portal>,
    /// The last app list, sent to each new connection.
    apps: Mutex<Option<Vec<App>>>,
}

static AGENT: OnceLock<Agent> = OnceLock::new();

fn agent() -> &'static Agent {
    AGENT.get_or_init(Agent::default)
}

pub fn start() {
    let a = agent();
    let portal = a.portal.clone();
    thread::spawn(move || {
        setup::gtk();
        setup::browser();
        let backend = setup::portal();
        portal::run(portal, backend);
    });
    thread::spawn(control);
    thread::spawn(links);
    thread::spawn(watch_apps);
}

/// Writes one message (whole, so messages don't interleave); false if the
/// app isn't connected.
fn send(msg: &[u8]) -> bool {
    let mut conn = agent().conn.lock().unwrap_or_else(|e| e.into_inner());
    let Some((_, f)) = conn.as_mut() else { return false };
    if f.write_all(msg).is_ok() {
        return true;
    }
    *conn = None;
    false
}

fn control() {
    let listener = match crate::vsock_listen(PORT) {
        Ok(l) => l,
        Err(e) => return eprintln!("awb-display: control port {PORT}: {e}"),
    };
    let mut n = 0u64;
    loop {
        let fd = match crate::vsock_accept(&listener) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("awb-display: control accept: {e}");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        n += 1;
        let file = File::from(fd);
        let Ok(writer) = file.try_clone() else { continue };
        eprintln!("awb-display: app connected (control {n})");
        *agent().conn.lock().unwrap_or_else(|e| e.into_inner()) = Some((n, writer));
        send(b"hello 1\n");
        if let Some(list) = agent().apps.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            send(&encode(&list));
        }
        let id = n;
        thread::spawn(move || {
            if let Err(e) = read(file) {
                eprintln!("awb-display: control {id}: {e}");
            }
            let mut conn = agent().conn.lock().unwrap_or_else(|e| e.into_inner());
            if conn.as_ref().is_some_and(|(i, _)| *i == id) {
                *conn = None;
            }
            eprintln!("awb-display: app disconnected (control {id})");
        });
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Handles the app's messages until it disconnects.
fn read(file: File) -> io::Result<()> {
    let mut r = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if r.by_ref().take(4096).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let text = std::str::from_utf8(&line).map_err(|_| bad("not text"))?.trim_end_matches('\n');
        let words: Vec<&str> = text.split(' ').collect();
        match words.as_slice() {
            ["appearance", dark, accent] => {
                let rgb = u32::from_str_radix(accent, 16).ok().filter(|_| accent.len() == 6).map(|c| {
                    let ch = |s: u32| ((c >> s) & 0xff) as f64 / 255.0;
                    (ch(16), ch(8), ch(0))
                });
                agent().portal.set(Appearance { dark: *dark == "1", accent: rgb });
            }
            ["file", name, len] => {
                let len: usize = len.parse().map_err(|_| bad("file length"))?;
                if len > MAX_FILE {
                    return Err(bad("file too large"));
                }
                let mut data = vec![0; len];
                r.read_exact(&mut data)?;
                // Only the app's two generated files.
                if ["gtk.css", "caption.json"].contains(name) {
                    setup::write(&setup::awb_dir().join(name), &data);
                }
            }
            ["launch", id] if apps::valid_id(id) => apps::launch(id, None),
            ["launch", id, uri] if apps::valid_id(id) && link_ok(uri) => apps::launch(id, Some(uri)),
            _ => eprintln!("awb-display: unknown message: {text}"),
        }
    }
}

/// A well-formed link: a scheme and one word of printable ASCII.
fn link_ok(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else { return false };
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
        && uri.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn web_link(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://")) && link_ok(url)
}

fn encode(list: &[App]) -> Vec<u8> {
    let mut out = b"apps begin\n".to_vec();
    for a in list {
        let info = format!("name={}\nschemes={}\n", a.name, a.schemes.join(","));
        let icon = a.icon.as_deref().unwrap_or_default();
        out.extend(format!("app {} {} {}\n", a.id, info.len(), icon.len()).bytes());
        out.extend(info.bytes());
        out.extend(icon);
    }
    out.extend(b"apps end\n");
    out
}

/// Rescans the apps when the desktop files change (checked every 5 s:
/// a few dozen stats) and sends the list when it differs.
fn watch_apps() {
    let mut icons = HashMap::new();
    let mut last_stamp = None;
    loop {
        let stamp = apps::stamp();
        if last_stamp.as_ref() != Some(&stamp) {
            let list = apps::scan(&mut icons);
            let mut current = agent().apps.lock().unwrap_or_else(|e| e.into_inner());
            if current.as_ref() != Some(&list) {
                eprintln!("awb-display: {} apps", list.len());
                *current = Some(list.clone());
                drop(current);
                send(&encode(&list));
            }
            last_stamp = Some(stamp);
        }
        thread::sleep(Duration::from_secs(5));
    }
}

/// awb-open's requests: `open <link>`, answered `ok` once passed on to the app.
fn links() {
    let path = crate::runtime_dir().join(SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return eprintln!("awb-display: {}: {e}", path.display()),
    };
    for stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        let Ok(mut w) = stream.try_clone() else { continue };
        if BufReader::new(stream).take(8192).read_line(&mut line).is_err() {
            continue;
        }
        let reply = match line.trim_end().strip_prefix("open ") {
            Some(url) if web_link(url) => {
                if send(format!("open {url}\n").as_bytes()) { "ok\n" } else { "no app\n" }
            }
            _ => "bad link\n",
        };
        let _ = w.write_all(reply.as_bytes());
    }
}

/// awb-open: hands web links to Android's browser, through the agent.
pub fn open(urls: impl Iterator<Item = String>) -> i32 {
    let mut status = 0;
    for url in urls {
        if !web_link(&url) {
            eprintln!("awb-open: not a web link: {url}");
            status = 1;
            continue;
        }
        let reply = (|| -> io::Result<String> {
            let mut s = UnixStream::connect(crate::runtime_dir().join(SOCKET))?;
            s.set_read_timeout(Some(Duration::from_secs(3)))?;
            s.write_all(format!("open {url}\n").as_bytes())?;
            let mut reply = String::new();
            BufReader::new(s).read_line(&mut reply)?;
            Ok(reply)
        })();
        match reply.as_deref().map(str::trim_end) {
            Ok("ok") => {}
            Ok("no app") => {
                eprintln!("awb-open: Wayland Bridge is not connected");
                status = 1;
            }
            Ok(r) => {
                eprintln!("awb-open: {r}");
                status = 1;
            }
            Err(e) => {
                eprintln!("awb-open: awb-display is not running ({e})");
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        assert!(web_link("https://example.com/a?b=c"));
        assert!(!web_link("file:///etc/passwd"));
        assert!(!web_link("https://a b"));
        assert!(!web_link("javascript:alert(1)"));
        assert!(link_ok("claude://claude.ai/new"));
    }
}
