//! Compositor state shared by all protocol handlers. Everything here is
//! owned by the compositor thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1;
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_surface, xdg_toplevel};
use wayland_server::backend::ObjectId;
use wayland_server::backend::protocol::ProtocolError;
use wayland_server::protocol::{wl_output, wl_surface};
use wayland_server::{Client, DisplayHandle};

use crate::Shared;
use crate::host::Host;
use crate::seat::Seat;
use crate::shell::Positioner;
use crate::shm::{BufferInfo, Pool};
use crate::surface::{RegionOp, Surface};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct XdgSurf {
    pub surface: wl_surface::WlSurface,
    pub res: xdg_surface::XdgSurface,
    pub role: XdgRole,
    pub geometry: Option<Rect>,
    pub pending_geometry: Option<Rect>,
    pub configured: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum XdgRole {
    None,
    Toplevel(u32),
    Popup,
}

/// One xdg_toplevel = one Android window (Activity in its own task).
pub struct Window {
    pub id: u32,
    pub surface: wl_surface::WlSurface,
    pub xdg_surface: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
    pub decoration: Option<ZxdgToplevelDecorationV1>,
    pub title: String,
    pub app_id: String,
    pub mapped: bool,
    /// Android surface size in px (0 until the Activity's surface exists).
    pub px: (i32, i32),
    /// Last configured size in logical px and activated state.
    pub configured: Option<(i32, i32, bool)>,
    pub focused: bool,
    /// Popup wl_surfaces of this window, bottom to top.
    pub popups: Vec<ObjectId>,
    pub dirty: bool,
    pub caption: crate::Caption,
    pub caption_light: Option<bool>,
    /// Gesture-exclusion rects last sent to Android (auto mode).
    pub exclusion: Vec<[i32; 4]>,
}

#[allow(dead_code)]
pub struct Popup {
    pub surface: wl_surface::WlSurface,
    pub xdg_surface: xdg_surface::XdgSurface,
    pub popup: xdg_popup::XdgPopup,
    /// Parent wl_surface (a toplevel's or another popup's).
    pub parent: ObjectId,
    pub window: u32,
    /// Geometry relative to the parent's window geometry.
    pub rect: Rect,
    pub grab: bool,
}

pub struct State {
    pub dh: DisplayHandle,
    pub shared: Arc<Shared>,
    pub host: Host,
    /// Output scale: Android px per logical px.
    pub scale: i32,
    pub display_px: (i32, i32),
    pub start: Instant,
    serial: u32,
    pub surfaces: HashMap<ObjectId, Surface>,
    pub regions: HashMap<ObjectId, Vec<RegionOp>>,
    pub pools: HashMap<ObjectId, Rc<RefCell<Pool>>>,
    pub buffers: HashMap<ObjectId, BufferInfo>,
    pub positioners: HashMap<ObjectId, Positioner>,
    pub xdg_surfaces: HashMap<ObjectId, XdgSurf>,
    pub windows: HashMap<u32, Window>,
    pub popups: HashMap<ObjectId, Popup>,
    pub next_window: u32,
    pub seat: Seat,
    pub outputs: Vec<wl_output::WlOutput>,
}

/// Runs a request handler, turning a panic into a disconnect of the client
/// that triggered it. One misbehaving client must never take down the app.
pub fn guard(what: &str, client: &Client, dh: &DisplayHandle, f: impl FnOnce()) {
    if let Err(e) = catch_unwind(AssertUnwindSafe(f)) {
        log::error!("panic handling {what}: {}; disconnecting client", crate::panic_message(&e));
        client.kill(
            dh,
            ProtocolError {
                code: 0,
                object_id: 1,
                object_interface: "wl_display".into(),
                message: "compositor internal error".into(),
            },
        );
    }
}

impl State {
    pub fn new(dh: DisplayHandle, shared: Arc<Shared>, host: Host, scale: i32, display_px: (i32, i32)) -> Self {
        State {
            dh,
            shared,
            host,
            scale,
            display_px,
            start: Instant::now(),
            serial: 1,
            surfaces: HashMap::new(),
            regions: HashMap::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            positioners: HashMap::new(),
            xdg_surfaces: HashMap::new(),
            windows: HashMap::new(),
            popups: HashMap::new(),
            next_window: 1,
            seat: Seat::default(),
            outputs: Vec::new(),
        }
    }

    pub fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    pub fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// The window a surface belongs to (walking up subsurfaces and popups).
    pub fn window_of(&self, sid: &ObjectId) -> Option<u32> {
        let mut cur = sid.clone();
        for _ in 0..64 {
            if let Some(p) = self.popups.get(&cur) {
                return Some(p.window);
            }
            let s = self.surfaces.get(&cur)?;
            if let Some(xs) = s.xdg.as_ref().and_then(|x| self.xdg_surfaces.get(x)) {
                if let XdgRole::Toplevel(w) = xs.role {
                    return Some(w);
                }
            }
            cur = s.sub.as_ref()?.parent.clone();
        }
        None
    }

    pub fn xdg_of(&self, sid: &ObjectId) -> Option<&XdgSurf> {
        self.surfaces.get(sid)?.xdg.as_ref().and_then(|x| self.xdg_surfaces.get(x))
    }

    /// Where a toplevel's or popup's window geometry starts, in window space
    /// (logical px, origin = toplevel window geometry).
    pub fn geometry_origin(&self, sid: &ObjectId) -> (i32, i32) {
        let mut cur = sid.clone();
        let (mut x, mut y) = (0, 0);
        for _ in 0..64 {
            match self.popups.get(&cur) {
                Some(p) => {
                    x += p.rect.x;
                    y += p.rect.y;
                    cur = p.parent.clone();
                }
                None => break,
            }
        }
        (x, y)
    }

    /// Surfaces of a window in paint order with their origins (window space).
    pub fn paint_list(&self, win: &Window) -> Vec<(ObjectId, i32, i32)> {
        let mut out = Vec::new();
        let add_role_surface = |sid: &ObjectId, out: &mut Vec<_>| {
            let (gx, gy) = self.geometry_origin(sid);
            let g = self.xdg_of(sid).and_then(|x| x.geometry).unwrap_or_default();
            self.push_tree(sid, gx - g.x, gy - g.y, out, 0);
        };
        add_role_surface(&win.surface.id(), &mut out);
        for p in &win.popups {
            add_role_surface(p, &mut out);
        }
        out
    }

    fn push_tree(&self, sid: &ObjectId, x: i32, y: i32, out: &mut Vec<(ObjectId, i32, i32)>, depth: u32) {
        let Some(s) = self.surfaces.get(sid) else { return };
        if depth > 32 {
            return;
        }
        for c in &s.below {
            if let Some(sub) = self.surfaces.get(c).and_then(|cs| cs.sub.as_ref()) {
                self.push_tree(c, x + sub.pos.0, y + sub.pos.1, out, depth + 1);
            }
        }
        out.push((sid.clone(), x, y));
        for c in &s.above {
            if let Some(sub) = self.surfaces.get(c).and_then(|cs| cs.sub.as_ref()) {
                self.push_tree(c, x + sub.pos.0, y + sub.pos.1, out, depth + 1);
            }
        }
    }

    /// Topmost surface accepting input at window-space point (x, y), with
    /// surface-local coordinates.
    pub fn surface_at(&self, win: &Window, x: f64, y: f64) -> Option<(ObjectId, f64, f64)> {
        for (sid, ox, oy) in self.paint_list(win).into_iter().rev() {
            let s = self.surfaces.get(&sid)?;
            let Some((w, h)) = s.logical_size() else { continue };
            let (lx, ly) = (x - ox as f64, y - oy as f64);
            if lx < 0.0 || ly < 0.0 || lx >= w as f64 || ly >= h as f64 {
                continue;
            }
            if s.accepts_input(lx as i32, ly as i32) {
                return Some((sid, lx, ly));
            }
        }
        None
    }

    /// Sends configure for a toplevel using its Android size (if known).
    pub fn configure_window(&mut self, id: u32) {
        let scale = self.scale;
        let serial = self.next_serial();
        let Some(w) = self.windows.get_mut(&id) else { return };
        let (lw, lh) = if w.px.0 > 0 { (w.px.0 / scale, (w.px.1 - w.caption.top) / scale) } else { (0, 0) };
        let v = w.toplevel.version();
        let mut states: Vec<u32> = Vec::new();
        if w.focused {
            states.push(xdg_toplevel::State::Activated as u32);
        }
        if v >= 2 {
            // Tiled on all edges: GTK and friends then draw no shadows or
            // rounded corners, so the content fills the Android window.
            states.extend([
                xdg_toplevel::State::TiledLeft as u32,
                xdg_toplevel::State::TiledRight as u32,
                xdg_toplevel::State::TiledTop as u32,
                xdg_toplevel::State::TiledBottom as u32,
            ]);
        }
        let bytes: Vec<u8> = states.iter().flat_map(|s| s.to_ne_bytes()).collect();
        if let Some(d) = &w.decoration {
            d.configure(wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode::ClientSide);
        }
        w.toplevel.configure(lw, lh, bytes);
        w.xdg_surface.configure(serial);
        w.configured = Some((lw, lh, w.focused));
    }

    pub fn mark_dirty(&mut self, sid: &ObjectId) {
        if let Some(w) = self.window_of(sid) {
            if let Some(win) = self.windows.get_mut(&w) {
                win.dirty = true;
            }
        }
    }
}

use wayland_server::Resource;
