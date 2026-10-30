//! Android's appearance as the guest's settings portal, as the compositor
//! serves it to the chroot (native/src/portal.rs, same settings): dark
//! mode, accent colour and GTK's button layout, live through
//! SettingChanged.
//!
//! The VM's Debian has xdg-desktop-portal, which owns
//! org.freedesktop.portal.Desktop and merges the Settings of its backends.
//! So we are one of those (org.freedesktop.impl.portal.Settings, set up by
//! setup::portal); only without xdg-desktop-portal do we serve the portal
//! itself, as in the chroot.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::zvariant::{OwnedValue, Value};

const PATH: &str = "/org/freedesktop/portal/desktop";
const APPEARANCE: &str = "org.freedesktop.appearance";
const WM_PREFERENCES: &str = "org.gnome.desktop.wm.preferences";
/// Our backend's name, as awb.portal declares it.
pub const BACKEND: &str = "org.freedesktop.impl.portal.desktop.awb";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Appearance {
    pub dark: bool,
    /// sRGB 0..1, if Android has a (Material You) accent colour.
    pub accent: Option<(f64, f64, f64)>,
}

#[derive(Default)]
pub struct Portal {
    state: Mutex<Option<Appearance>>,
    changed: Condvar,
}

impl Portal {
    pub fn set(&self, a: Appearance) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *s != Some(a) {
            *s = Some(a);
            self.changed.notify_all();
        }
    }

    fn get(&self) -> Option<Appearance> {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The settings we publish: (namespace, key, value).
fn settings(a: Option<Appearance>) -> Vec<(&'static str, &'static str, Value<'static>)> {
    // A button on both sides gives each side a windowcontrols box, whose
    // gtk.css min-width keeps the header clear of Android's caption controls.
    let mut v = vec![(WM_PREFERENCES, "button-layout", Value::from("close:close"))];
    let Some(a) = a else { return v };
    // color-scheme: 0 = no preference, 1 = prefer dark, 2 = prefer light.
    v.push((APPEARANCE, "color-scheme", Value::U32(if a.dark { 1 } else { 2 })));
    if let Some(rgb) = a.accent {
        v.push((APPEARANCE, "accent-color", Value::from(rgb)));
    }
    v
}

fn lookup(portal: &Portal, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
    let found = settings(portal.get()).into_iter().find(|(n, k, _)| *n == namespace && *k == key);
    let (_, _, value) = found.ok_or_else(|| PortalError::NotFound(format!("{namespace}.{key}")))?;
    OwnedValue::try_from(value).map_err(|e| PortalError::ZBus(e.into()))
}

/// Namespace filter of ReadAll: exact names, or a trailing `*` wildcard.
fn matches(patterns: &[String], namespace: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => namespace.starts_with(prefix),
            None => p == namespace,
        })
}

fn read_all(portal: &Portal, namespaces: &[String]) -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut out: HashMap<String, HashMap<String, OwnedValue>> = HashMap::new();
    for (namespace, key, value) in settings(portal.get()) {
        if !matches(namespaces, namespace) {
            continue;
        }
        if let Ok(value) = OwnedValue::try_from(value) {
            out.entry(namespace.to_string()).or_default().insert(key.to_string(), value);
        }
    }
    out
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
}

/// The backend interface xdg-desktop-portal reads.
struct Backend(Arc<Portal>);

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl Backend {
    fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        read_all(&self.0, &namespaces)
    }

    /// The backend's Read is unwrapped (the frontend wraps it for apps).
    fn read(&self, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
        lookup(&self.0, namespace, key)
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}

/// The portal itself, where there is no xdg-desktop-portal.
struct Frontend(Arc<Portal>);

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Frontend {
    fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        read_all(&self.0, &namespaces)
    }

    fn read_one(&self, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
        lookup(&self.0, namespace, key)
    }

    /// Deprecated v1 call; its value is wrapped in an extra variant.
    fn read(&self, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
        let inner = lookup(&self.0, namespace, key)?;
        OwnedValue::try_from(Value::new(Value::from(inner))).map_err(|e| PortalError::ZBus(e.into()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

fn connect(portal: &Arc<Portal>, backend: bool) -> zbus::Result<Connection> {
    let b = Builder::session()?;
    if backend {
        b.serve_at(PATH, Backend(portal.clone()))?.name(BACKEND)?.build()
    } else {
        b.serve_at(PATH, Frontend(portal.clone()))?.name("org.freedesktop.portal.Desktop")?.build()
    }
}

fn alive(conn: &Connection) -> bool {
    conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetId", &())
        .is_ok()
}

/// Keeps the portal (or with `backend`, our xdg-desktop-portal backend) on
/// the session bus and emits SettingChanged on updates.
pub fn run(portal: Arc<Portal>, backend: bool) {
    let iface = if backend { "org.freedesktop.impl.portal.Settings" } else { "org.freedesktop.portal.Settings" };
    let mut last_error = String::new();
    loop {
        let conn = match connect(&portal, backend) {
            Ok(c) => c,
            Err(e) => {
                let e = e.to_string();
                if e != last_error {
                    eprintln!("awb-display: settings portal: waiting for the session bus ({e})");
                    last_error = e;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        eprintln!("awb-display: settings portal: serving {iface}");
        last_error.clear();
        let mut published = portal.get();
        loop {
            let guard = portal.state.lock().unwrap_or_else(|e| e.into_inner());
            // Wake on changes, and now and then to notice a dead bus.
            let (guard, _) = portal
                .changed
                .wait_timeout_while(guard, Duration::from_secs(30), |s| *s == published)
                .unwrap_or_else(|e| e.into_inner());
            let current = *guard;
            drop(guard);
            if current == published {
                if alive(&conn) {
                    continue;
                }
                break;
            }
            let old = settings(published);
            let mut ok = true;
            for (namespace, key, value) in settings(current) {
                if old.iter().any(|(n, k, v)| *n == namespace && *k == key && *v == value) {
                    continue;
                }
                ok &= conn.emit_signal(None::<()>, PATH, iface, "SettingChanged", &(namespace, key, value)).is_ok();
            }
            if !ok {
                break;
            }
            eprintln!("awb-display: settings portal: published {current:?}");
            published = current;
        }
        eprintln!("awb-display: settings portal: lost the session bus, reconnecting");
    }
}
