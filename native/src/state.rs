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
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DisplayHandle};

use crate::Shared;
use crate::host::Host;
use crate::output::{Outputs, to_logical};
use crate::seat::Seat;
use crate::shell::Positioner;
use crate::shm::{BufferInfo, Pool};

/// What a wl_buffer is backed by.
pub enum Buffer {
    Shm(BufferInfo),
    Dmabuf(Rc<crate::gpu::Dmabuf>),
}
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
    /// ack_configure serial, applied with the next commit.
    pub pending_ack: Option<u32>,
}

/// How long a synced resize waits for the client's frame at the new size.
pub const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Android waiting for a window's frame at its new size (`Cmd::RedrawSync`).
pub struct SyncReq {
    /// The configure the frame must answer.
    pub serial: u32,
    pub start: Instant,
    pub reply: crate::SyncReply,
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
    /// The client asked for server-side decorations: Android draws an
    /// opaque caption above it instead of the app's header showing through.
    pub ssd: bool,
    pub title: String,
    pub app_id: String,
    pub mapped: bool,
    /// Android surface size in px (0 until the Activity's surface exists).
    pub px: (i32, i32),
    /// Android display the window is on; its density sets the scale.
    pub display: i32,
    /// Last configured size in logical px and activated state.
    pub configured: Option<(i32, i32, bool)>,
    /// Serial of the last configure, and the last one the client acked
    /// with a commit.
    pub configure_serial: u32,
    pub acked: u32,
    pub sync: Option<SyncReq>,
    /// The window's process owns its direct layer (`Cmd::OwnsLayer`):
    /// direct frames go to it instead of onto a layer of ours.
    pub window_layer: bool,
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
    pub start: Instant,
    serial: u32,
    pub surfaces: HashMap<ObjectId, Surface>,
    pub regions: HashMap<ObjectId, Vec<RegionOp>>,
    pub pools: HashMap<ObjectId, Rc<RefCell<Pool>>>,
    pub buffers: HashMap<ObjectId, Buffer>,
    pub positioners: HashMap<ObjectId, Positioner>,
    pub xdg_surfaces: HashMap<ObjectId, XdgSurf>,
    pub windows: HashMap<u32, Window>,
    pub popups: HashMap<ObjectId, Popup>,
    pub next_window: u32,
    pub seat: Seat,
    pub data: crate::data::Data,
    pub text_inputs: crate::text_input::TextInputs,
    pub constraints: crate::constraints::Constraints,
    pub outputs: Outputs,
    /// The Vulkan renderer; None draws on the CPU (and offers no dmabuf).
    pub gpu: Option<crate::gpu::Renderer>,
    /// The render node, for explicit sync (wp_linux_drm_syncobj_v1).
    pub drm: Option<Arc<crate::syncobj::Drm>>,
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
    pub fn new(dh: DisplayHandle, shared: Arc<Shared>, host: Host) -> Self {
        State {
            dh,
            shared,
            host,
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
            data: Default::default(),
            text_inputs: Default::default(),
            constraints: Default::default(),
            outputs: Outputs::default(),
            gpu: None,
            drm: None,
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

    /// Scale of a window in 120ths, from the display it is on.
    pub fn window_scale(&self, id: u32) -> u32 {
        let display = self.windows.get(&id).map_or_else(|| self.outputs.default_display(), |w| w.display);
        self.outputs.scale120(display)
    }

    /// Logical size of a window's content area for its Android size.
    pub fn logical_window_size(&self, id: u32) -> (i32, i32) {
        let scale = self.window_scale(id);
        match self.windows.get(&id) {
            Some(w) if w.px.0 > 0 => (
                to_logical(w.px.0 as f64, scale) as i32,
                to_logical((w.px.1 - w.caption.top) as f64, scale) as i32,
            ),
            _ => (0, 0),
        }
    }

    /// Sends configure for a toplevel using its Android size (if known).
    pub fn configure_window(&mut self, id: u32) {
        let (lw, lh) = self.logical_window_size(id);
        let Some((sid, display)) = self.windows.get(&id).map(|w| (w.surface.id(), w.display)) else { return };
        // The scale first, so the client draws the configured frame at it.
        crate::output::ensure_scale(self, &sid, display);
        let serial = self.next_serial();
        let Some(w) = self.windows.get_mut(&id) else { return };
        let v = w.toplevel.version();
        let mut states: Vec<u32> = Vec::new();
        if w.focused {
            states.push(xdg_toplevel::State::Activated as u32);
        }
        // GTK styles it as `window.maximized`, which is how gtk.css gives
        // maximised windows their own caption geometry.
        if w.caption.maximized {
            states.push(xdg_toplevel::State::Maximized as u32);
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
            use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
            d.configure(if w.ssd { Mode::ServerSide } else { Mode::ClientSide });
        }
        w.toplevel.configure(lw, lh, bytes);
        w.xdg_surface.configure(serial);
        w.configured = Some((lw, lh, w.focused));
        w.configure_serial = serial;
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
