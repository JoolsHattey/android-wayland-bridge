//! xdg-shell and xdg-decoration: toplevels become Android windows, popups
//! are composited into their toplevel's window.

use wayland_protocols::xdg::decoration::zv1::server::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::state::{Popup, Rect, State, Window, XdgRole, XdgSurf, guard};
use crate::surface::Role;

#[derive(Clone, Copy, Default, Debug)]
pub struct Positioner {
    pub size: (i32, i32),
    pub anchor_rect: Rect,
    pub anchor: u32,
    pub gravity: u32,
    pub constraint: u32,
    pub offset: (i32, i32),
}

fn wenum<T: Into<u32>>(e: WEnum<T>) -> u32 {
    match e {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

/// Anchor point on the anchor rect for an xdg_positioner anchor value.
fn anchor_point(r: Rect, anchor: u32) -> (i32, i32) {
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    match anchor {
        1 => (cx, r.y),
        2 => (cx, r.y + r.h),
        3 => (r.x, cy),
        4 => (r.x + r.w, cy),
        5 => (r.x, r.y),
        6 => (r.x, r.y + r.h),
        7 => (r.x + r.w, r.y),
        8 => (r.x + r.w, r.y + r.h),
        _ => (cx, cy),
    }
}

/// Top-left of a w x h box placed at point p growing toward `gravity`.
fn apply_gravity(p: (i32, i32), w: i32, h: i32, gravity: u32) -> (i32, i32) {
    let x = match gravity {
        3 | 5 | 6 => p.0 - w,
        4 | 7 | 8 => p.0,
        _ => p.0 - w / 2,
    };
    let y = match gravity {
        1 | 5 | 7 => p.1 - h,
        2 | 6 | 8 => p.1,
        _ => p.1 - h / 2,
    };
    (x, y)
}

fn flip_x(v: u32) -> u32 {
    match v {
        3 => 4,
        4 => 3,
        5 => 7,
        7 => 5,
        6 => 8,
        8 => 6,
        o => o,
    }
}

fn flip_y(v: u32) -> u32 {
    match v {
        1 => 2,
        2 => 1,
        5 => 6,
        6 => 5,
        7 => 8,
        8 => 7,
        o => o,
    }
}

impl Positioner {
    /// Popup rect relative to the parent's geometry, constrained to `bounds`
    /// (also parent-relative): flip, then slide, then resize.
    pub fn place(&self, bounds: Rect) -> Rect {
        let (w, h) = (self.size.0.max(1), self.size.1.max(1));
        let at = |anchor: u32, gravity: u32| {
            let p = anchor_point(self.anchor_rect, anchor);
            let (x, y) = apply_gravity(p, w, h, gravity);
            (x + self.offset.0, y + self.offset.1)
        };
        let (mut x, mut y) = at(self.anchor, self.gravity);
        let out_x = |x: i32, w: i32| x < bounds.x || x + w > bounds.x + bounds.w;
        let out_y = |y: i32, h: i32| y < bounds.y || y + h > bounds.y + bounds.h;
        let c = self.constraint;
        if c & 4 != 0 && out_x(x, w) {
            let (fx, _) = at(flip_x(self.anchor), flip_x(self.gravity));
            if !out_x(fx, w) {
                x = fx;
            }
        }
        if c & 8 != 0 && out_y(y, h) {
            let (_, fy) = at(flip_y(self.anchor), flip_y(self.gravity));
            if !out_y(fy, h) {
                y = fy;
            }
        }
        if c & 1 != 0 && out_x(x, w) {
            x = x.min(bounds.x + bounds.w - w).max(bounds.x);
        }
        if c & 2 != 0 && out_y(y, h) {
            y = y.min(bounds.y + bounds.h - h).max(bounds.y);
        }
        let (mut w, mut h) = (w, h);
        if c & 16 != 0 && out_x(x, w) {
            let l = x.max(bounds.x);
            w = (x + w).min(bounds.x + bounds.w) - l;
            x = l;
        }
        if c & 32 != 0 && out_y(y, h) {
            let t = y.max(bounds.y);
            h = (y + h).min(bounds.y + bounds.h) - t;
            y = t;
        }
        Rect { x, y, w: w.max(1), h: h.max(1) }
    }
}

// ---- xdg_wm_base -------------------------------------------------------

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<xdg_wm_base::XdgWmBase>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &xdg_wm_base::XdgWmBase,
        req: xdg_wm_base::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("xdg_wm_base", client, dh, || match req {
            xdg_wm_base::Request::CreatePositioner { id } => {
                let p = init.init(id, ());
                st.positioners.insert(p.id(), Positioner::default());
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let xs = init.init(id, ());
                let sid = surface.id();
                let Some(s) = st.surfaces.get_mut(&sid) else { return };
                if !matches!(s.role, Role::None) || s.texture.is_some() {
                    res.post_error(xdg_wm_base::Error::Role, "surface has a role or a buffer");
                    return;
                }
                s.role = Role::Xdg;
                s.xdg = Some(xs.id());
                st.xdg_surfaces.insert(
                    xs.id(),
                    XdgSurf { surface, res: xs, role: XdgRole::None, geometry: None, pending_geometry: None, configured: false },
                );
            }
            _ => {}
        });
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for State {
    fn request(
        st: &mut State,
        _: &Client,
        res: &xdg_positioner::XdgPositioner,
        req: xdg_positioner::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        let Some(p) = st.positioners.get_mut(&res.id()) else { return };
        match req {
            xdg_positioner::Request::SetSize { width, height } => p.size = (width, height),
            xdg_positioner::Request::SetAnchorRect { x, y, width, height } => {
                p.anchor_rect = Rect { x, y, w: width, h: height }
            }
            xdg_positioner::Request::SetAnchor { anchor } => p.anchor = wenum(anchor),
            xdg_positioner::Request::SetGravity { gravity } => p.gravity = wenum(gravity),
            xdg_positioner::Request::SetConstraintAdjustment { constraint_adjustment } => {
                p.constraint = match constraint_adjustment {
                    WEnum::Value(v) => v.bits(),
                    WEnum::Unknown(u) => u,
                }
            }
            xdg_positioner::Request::SetOffset { x, y } => p.offset = (x, y),
            _ => {}
        }
    }

    fn destroyed(st: &mut State, _: ClientId, res: &xdg_positioner::XdgPositioner, _: &()) {
        st.positioners.remove(&res.id());
    }
}

// ---- xdg_surface -------------------------------------------------------

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &xdg_surface::XdgSurface,
        req: xdg_surface::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("xdg_surface", client, dh, || {
            let xid = res.id();
            match req {
                xdg_surface::Request::GetToplevel { id } => {
                    let tl = init.init(id, xid.clone());
                    let Some(xs) = st.xdg_surfaces.get_mut(&xid) else { return };
                    if xs.role != XdgRole::None {
                        res.post_error(xdg_surface::Error::AlreadyConstructed, "role already set");
                        return;
                    }
                    let win = st.next_window;
                    st.next_window += 1;
                    xs.role = XdgRole::Toplevel(win);
                    let surface = xs.surface.clone();
                    st.windows.insert(
                        win,
                        Window {
                            id: win,
                            surface,
                            xdg_surface: res.clone(),
                            toplevel: tl,
                            decoration: None,
                            title: String::new(),
                            app_id: String::new(),
                            mapped: false,
                            px: (0, 0),
                            display: st.outputs.default_display(),
                            configured: None,
                            focused: false,
                            popups: Vec::new(),
                            dirty: false,
                            caption: Default::default(),
                            caption_light: None,
                            exclusion: Vec::new(),
                        },
                    );
                    log::info!("toplevel {win} created");
                }
                xdg_surface::Request::GetPopup { id, parent, positioner } => {
                    let popup = init.init(id, xid.clone());
                    let pos = st.positioners.get(&positioner.id()).copied().unwrap_or_default();
                    let Some(parent) = parent.and_then(|p| st.xdg_surfaces.get(&p.id())) else {
                        // Parentless popups (layer-shell style) are unsupported.
                        popup.popup_done();
                        return;
                    };
                    let parent_sid = parent.surface.id();
                    let Some(window) = st.window_of(&parent_sid) else {
                        popup.popup_done();
                        return;
                    };
                    let Some(xs) = st.xdg_surfaces.get_mut(&xid) else { return };
                    if xs.role != XdgRole::None {
                        res.post_error(xdg_surface::Error::AlreadyConstructed, "role already set");
                        return;
                    }
                    xs.role = XdgRole::Popup;
                    let surface = xs.surface.clone();
                    let sid = surface.id();
                    let rect = place_popup(st, &parent_sid, window, &pos);
                    st.popups.insert(
                        sid.clone(),
                        Popup { surface, xdg_surface: res.clone(), popup, parent: parent_sid, window, rect, grab: false },
                    );
                    if let Some(w) = st.windows.get_mut(&window) {
                        w.popups.push(sid);
                    }
                }
                xdg_surface::Request::SetWindowGeometry { x, y, width, height } => {
                    if let Some(xs) = st.xdg_surfaces.get_mut(&xid) {
                        if width > 0 && height > 0 {
                            xs.pending_geometry = Some(Rect { x, y, w: width, h: height });
                        }
                    }
                }
                xdg_surface::Request::AckConfigure { .. } => {}
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &xdg_surface::XdgSurface, _: &()) {
        if let Some(xs) = st.xdg_surfaces.remove(&res.id()) {
            if let Some(s) = st.surfaces.get_mut(&xs.surface.id()) {
                s.xdg = None;
            }
        }
    }
}

/// Positions a popup inside its window (popups cannot leave the Android
/// window yet, so the constraint bounds are the window itself).
fn place_popup(st: &State, parent_sid: &ObjectId, window: u32, pos: &Positioner) -> Rect {
    let (ox, oy) = st.geometry_origin(parent_sid);
    let (ww, wh) = st
        .windows
        .get(&window)
        .map(|w| match w.configured {
            Some((cw, ch, _)) if cw > 0 => (cw, ch),
            _ => st.xdg_of(&w.surface.id()).and_then(|x| x.geometry).map(|g| (g.w, g.h)).unwrap_or((10000, 10000)),
        })
        .unwrap_or((10000, 10000));
    let bounds = Rect { x: -ox, y: -oy, w: ww, h: wh };
    // Always allow sliding so menus stay reachable inside the window.
    let mut p = *pos;
    p.constraint |= 1 | 2;
    p.place(bounds)
}

/// Role-specific handling after a wl_surface commit on an xdg surface.
pub fn xdg_committed(st: &mut State, xid: &ObjectId) {
    let Some(xs) = st.xdg_surfaces.get_mut(xid) else { return };
    if let Some(g) = xs.pending_geometry.take() {
        xs.geometry = Some(g);
    }
    let role = xs.role;
    let first = !xs.configured;
    xs.configured = true;
    let sid = xs.surface.id();
    let has_buffer = st.surfaces.get(&sid).is_some_and(|s| s.texture.is_some());
    match role {
        XdgRole::Toplevel(win) => {
            if first {
                if let Some(w) = st.windows.get(&win) {
                    if w.toplevel.version() >= 5 {
                        // Android draws minimise/maximise/close itself.
                        w.toplevel.wm_capabilities(Vec::new());
                    }
                }
                st.configure_window(win);
                return;
            }
            let Some(w) = st.windows.get_mut(&win) else { return };
            if has_buffer && !w.mapped {
                w.mapped = true;
                let size = st
                    .xdg_surfaces
                    .get(xid)
                    .and_then(|x| x.geometry)
                    .map(|g| (g.w, g.h))
                    .or_else(|| st.surfaces.get(&sid).and_then(|s| s.logical_size()))
                    .unwrap_or((640, 480));
                let (title, app_id) = (w.title.clone(), w.app_id.clone());
                log::info!("toplevel {win} mapped {}x{} '{title}' ({app_id})", size.0, size.1);
                st.host.map(win, size.0, size.1, &title, &app_id);
            } else if !has_buffer && w.mapped {
                w.mapped = false;
                if let Some(xs) = st.xdg_surfaces.get_mut(xid) {
                    xs.configured = false;
                }
                st.host.unmap(win);
            }
        }
        XdgRole::Popup => {
            if first {
                let display = crate::output::display_of(st, &sid);
                crate::output::ensure_scale(st, &sid, display);
                if let Some(p) = st.popups.get(&sid) {
                    let r = p.rect;
                    p.popup.configure(r.x, r.y, r.w, r.h);
                }
                let serial = st.next_serial();
                if let Some(xs) = st.xdg_surfaces.get(xid) {
                    xs.res.configure(serial);
                }
            }
        }
        XdgRole::None => {}
    }
}

// ---- xdg_toplevel ------------------------------------------------------

impl Dispatch<xdg_toplevel::XdgToplevel, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &xdg_toplevel::XdgToplevel,
        req: xdg_toplevel::Request,
        xid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("xdg_toplevel", client, dh, || {
            let Some(XdgRole::Toplevel(win)) = st.xdg_surfaces.get(xid).map(|x| x.role) else { return };
            match req {
                xdg_toplevel::Request::SetTitle { title } => {
                    if let Some(w) = st.windows.get_mut(&win) {
                        if w.title != title {
                            w.title = title.clone();
                            if w.mapped {
                                st.host.title(win, &title);
                            }
                        }
                    }
                }
                xdg_toplevel::Request::SetAppId { app_id } => {
                    if let Some(w) = st.windows.get_mut(&win) {
                        w.app_id = app_id;
                    }
                }
                xdg_toplevel::Request::Move { .. } => {
                    log::info!("toplevel {win}: move requested");
                    st.host.move_request(win);
                }
                xdg_toplevel::Request::Resize { edges, .. } => {
                    log::info!("toplevel {win}: resize requested (edges {})", wenum(edges));
                }
                xdg_toplevel::Request::SetMinimized => st.host.minimize(win),
                xdg_toplevel::Request::SetMaximized
                | xdg_toplevel::Request::UnsetMaximized
                | xdg_toplevel::Request::SetFullscreen { .. }
                | xdg_toplevel::Request::UnsetFullscreen => {
                    // No app API to change desktop window state yet; the
                    // protocol still requires a configure in response.
                    if st.windows.get(&win).is_some_and(|w| w.configured.is_some()) {
                        st.configure_window(win);
                    }
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &xdg_toplevel::XdgToplevel, xid: &ObjectId) {
        // On disconnect objects die in any order, so the xdg_surface may be
        // gone already: find the window by its toplevel object.
        let Some(win) = st.windows.values().find(|w| w.toplevel == *res).map(|w| w.id) else { return };
        if let Some(xs) = st.xdg_surfaces.get_mut(xid) {
            xs.role = XdgRole::None;
        }
        if let Some(w) = st.windows.remove(&win) {
            for p in &w.popups {
                if let Some(p) = st.popups.remove(p) {
                    p.popup.popup_done();
                }
            }
            log::info!("toplevel {win} destroyed");
            st.host.unmap(win);
        }
        st.seat.window_gone(win);
        if let Some(slot) = st.shared.slots.lock().ok().and_then(|mut s| s.remove(&win)) {
            slot.detach();
        }
    }
}

// ---- xdg_popup ---------------------------------------------------------

impl Dispatch<xdg_popup::XdgPopup, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &xdg_popup::XdgPopup,
        req: xdg_popup::Request,
        xid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("xdg_popup", client, dh, || {
            let Some(sid) = st.xdg_surfaces.get(xid).map(|x| x.surface.id()) else { return };
            match req {
                xdg_popup::Request::Grab { .. } => {
                    if let Some(p) = st.popups.get_mut(&sid) {
                        p.grab = true;
                    }
                    crate::seat::popup_grabbed(st, &sid);
                }
                xdg_popup::Request::Reposition { positioner, token } => {
                    let pos = st.positioners.get(&positioner.id()).copied().unwrap_or_default();
                    let Some((parent, window)) = st.popups.get(&sid).map(|p| (p.parent.clone(), p.window)) else { return };
                    let rect = place_popup(st, &parent, window, &pos);
                    if let Some(p) = st.popups.get_mut(&sid) {
                        p.rect = rect;
                    }
                    res.repositioned(token);
                    res.configure(rect.x, rect.y, rect.w, rect.h);
                    let serial = st.next_serial();
                    if let Some(xs) = st.xdg_surfaces.get(xid) {
                        xs.res.configure(serial);
                    }
                    st.mark_dirty(&sid);
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &xdg_popup::XdgPopup, xid: &ObjectId) {
        let Some(sid) = st.popups.iter().find(|(_, p)| p.popup == *res).map(|(s, _)| s.clone()) else { return };
        st.mark_dirty(&sid);
        if let Some(p) = st.popups.remove(&sid) {
            if let Some(w) = st.windows.get_mut(&p.window) {
                w.popups.retain(|x| *x != sid);
            }
        }
        if let Some(xs) = st.xdg_surfaces.get_mut(xid) {
            xs.role = XdgRole::None;
        }
        st.seat.surface_gone(&sid);
        crate::seat::refocus_keyboard(st);
    }
}

/// Dismisses every popup of a window (top first), e.g. on an outside click.
pub fn dismiss_popups(st: &mut State, win: u32) {
    let Some(w) = st.windows.get(&win) else { return };
    for sid in w.popups.iter().rev() {
        if let Some(p) = st.popups.get(sid) {
            p.popup.popup_done();
        }
    }
}

// ---- xdg-decoration ----------------------------------------------------

impl GlobalDispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for State {
    fn bind(
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        res: New<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
        _: &(),
        init: &mut DataInit<'_, State>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        req: zxdg_decoration_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zxdg_decoration_manager_v1", client, dh, || {
            if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, toplevel } = req {
                let xid: ObjectId = toplevel.data::<ObjectId>().cloned().unwrap_or(toplevel.id());
                let deco = init.init(id, xid.clone());
                if let Some(XdgRole::Toplevel(win)) = st.xdg_surfaces.get(&xid).map(|x| x.role) {
                    if let Some(w) = st.windows.get_mut(&win) {
                        w.decoration = Some(deco);
                        if w.configured.is_some() {
                            st.configure_window(win);
                        }
                    }
                }
            }
        });
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        req: zxdg_toplevel_decoration_v1::Request,
        xid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("zxdg_toplevel_decoration_v1", client, dh, || match req {
            zxdg_toplevel_decoration_v1::Request::SetMode { .. } | zxdg_toplevel_decoration_v1::Request::UnsetMode => {
                // We always answer client-side: the app draws its header
                // into Android's transparent caption bar.
                if let Some(XdgRole::Toplevel(win)) = st.xdg_surfaces.get(xid).map(|x| x.role) {
                    if st.windows.get(&win).is_some_and(|w| w.configured.is_some()) {
                        st.configure_window(win);
                    }
                }
            }
            _ => {}
        });
    }

    fn destroyed(st: &mut State, _: ClientId, _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, xid: &ObjectId) {
        if let Some(XdgRole::Toplevel(win)) = st.xdg_surfaces.get(xid).map(|x| x.role) {
            if let Some(w) = st.windows.get_mut(&win) {
                w.decoration = None;
            }
        }
    }
}
