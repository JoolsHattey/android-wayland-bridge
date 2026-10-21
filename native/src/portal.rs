//! The desktop's settings portal: like GNOME Shell or KWin, the compositor
//! is the source of appearance settings. It connects to the chroot's session
//! bus (a socket in our files dir, /run/awb/bus inside the chroot) and serves
//! org.freedesktop.portal.Settings there, so GTK and libadwaita follow
//! Android's dark mode and accent colour, live.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::zvariant::{OwnedValue, Value};

const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";

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
    /// Called from JNI on every Android configuration change.
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

/// The settings we publish: namespace -> key -> value.
fn settings(a: Option<Appearance>) -> Vec<(&'static str, Value<'static>)> {
    let Some(a) = a else { return Vec::new() };
    // color-scheme: 0 = no preference, 1 = prefer dark, 2 = prefer light.
    let mut v = vec![("color-scheme", Value::U32(if a.dark { 1 } else { 2 }))];
    if let Some(rgb) = a.accent {
        v.push(("accent-color", Value::from(rgb)));
    }
    v
}

fn lookup(portal: &Portal, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
    let found = (namespace == APPEARANCE)
        .then(|| settings(portal.get()).into_iter().find(|(k, _)| *k == key))
        .flatten();
    let (_, value) = found.ok_or_else(|| PortalError::NotFound(format!("{namespace}.{key}")))?;
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

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
}

struct Settings(Arc<Portal>);

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Settings {
    fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut out = HashMap::new();
        if matches(&namespaces, APPEARANCE) {
            let values = settings(self.0.get())
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_string(), OwnedValue::try_from(v).ok()?)))
                .collect();
            out.insert(APPEARANCE.to_string(), values);
        }
        out
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

fn connect(address: &str, portal: &Arc<Portal>) -> zbus::Result<Connection> {
    Builder::address(address)?
        .serve_at(PATH, Settings(portal.clone()))?
        .name("org.freedesktop.portal.Desktop")?
        .build()
}

fn alive(conn: &Connection) -> bool {
    conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetId", &())
        .is_ok()
}

/// Keeps the portal on the bus at `address` (the bus comes and goes with
/// the chroot's dbus-daemon) and emits SettingChanged on updates.
pub fn run(address: String, portal: Arc<Portal>) {
    let mut last_error = String::new();
    loop {
        let conn = match connect(&address, &portal) {
            Ok(c) => c,
            Err(e) => {
                let e = e.to_string();
                if e != last_error {
                    log::info!("settings portal: waiting for session bus {address} ({e})");
                    last_error = e;
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        log::info!("settings portal: serving on {address}");
        last_error.clear();
        let mut published = portal.get();
        loop {
            let guard = portal.state.lock().unwrap_or_else(|e| e.into_inner());
            // Wake on changes, and now and then to notice a dead bus.
            let (guard, _) = portal
                .changed
                .wait_timeout_while(guard, Duration::from_secs(5), |s| *s == published)
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
            for (key, value) in settings(current) {
                if old.iter().any(|(k, v)| *k == key && *v == value) {
                    continue;
                }
                ok &= conn.emit_signal(None::<()>, PATH, IFACE, "SettingChanged", &(APPEARANCE, key, value)).is_ok();
            }
            if !ok {
                break;
            }
            log::info!("settings portal: published {current:?}");
            published = current;
        }
        log::info!("settings portal: lost the session bus, reconnecting");
    }
}
