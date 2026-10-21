//! zwp_relative_pointer_manager_v1 and zwp_pointer_constraints_v1, for games
//! and 3D apps. Relative motion comes with every pointer event. A lock asks
//! the window's view for Android pointer capture and is active once Android
//! grants it; a confinement clamps the pointer to its region while the
//! pointer is in the window (Android can't hold its cursor in place, see
//! docs/DESIGN.md, Input).

use std::collections::HashMap;

use wayland_protocols::wp::pointer_constraints::zv1::server::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::server::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1};
use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::protocol::{wl_pointer, wl_region, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::state::{State, guard};
use crate::surface::{RegionOp, region_contains};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Inactive,
    /// A lock waiting for Android's pointer capture.
    Requested,
    Active,
    /// A oneshot constraint that was active once, or one whose surface is gone.
    Dead,
}

enum Kind {
    Lock(zwp_locked_pointer_v1::ZwpLockedPointerV1),
    Confine(zwp_confined_pointer_v1::ZwpConfinedPointerV1),
}

struct Constraint {
    kind: Kind,
    surface: ObjectId,
    /// None: the whole surface.
    region: Option<Vec<RegionOp>>,
    persistent: bool,
    phase: Phase,
    /// The window whose capture a lock asked for.
    win: Option<u32>,
}

#[derive(Default)]
pub struct Constraints {
    /// By the locked/confined pointer object's id.
    list: HashMap<ObjectId, Constraint>,
    relative: Vec<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
}

/// Whether a lock is active on the surface with pointer focus: the pointer
/// stays put and only relative motion goes out.
pub fn locked(st: &State) -> bool {
    let Some((focus, _)) = &st.seat.ptr_focus else { return false };
    st.constraints.list.values().any(|c| matches!(c.kind, Kind::Lock(_)) && c.phase == Phase::Active && &c.surface == focus)
}

/// An active confinement on `sid`: the surface-local box the pointer is
/// clamped to (x0, y0, x1, y1).
pub fn confinement(st: &State, sid: &ObjectId) -> Option<(f64, f64, f64, f64)> {
    let c = st
        .constraints
        .list
        .values()
        .find(|c| matches!(c.kind, Kind::Confine(_)) && c.phase == Phase::Active && &c.surface == sid)?;
    let (w, h) = st.surfaces.get(sid)?.logical_size()?;
    let (mut x0, mut y0, mut x1, mut y1) = (0.0f64, 0.0f64, w as f64, h as f64);
    // The bounding box of the region's added rects (an L-shaped region
    // still lets the pointer into its gaps).
    if let Some(ops) = &c.region {
        let adds = ops.iter().filter_map(|op| match *op {
            RegionOp::Add(x, y, w, h) => Some((x as f64, y as f64, (x + w) as f64, (y + h) as f64)),
            RegionOp::Sub(..) => None,
        });
        let bb = adds.fold(None, |acc: Option<(f64, f64, f64, f64)>, r| {
            Some(acc.map_or(r, |a| (a.0.min(r.0), a.1.min(r.1), a.2.max(r.2), a.3.max(r.3))))
        });
        if let Some(b) = bb {
            (x0, y0, x1, y1) = (x0.max(b.0), y0.max(b.1), x1.min(b.2), y1.min(b.3));
        }
    }
    // Inside: the last whole pixel is the edge.
    Some((x0, y0, (x1 - 1.0).max(x0), (y1 - 1.0).max(y0)))
}

fn deactivate(st: &mut State, id: &ObjectId) {
    let Some(c) = st.constraints.list.get_mut(id) else { return };
    let was = c.phase;
    if !matches!(was, Phase::Requested | Phase::Active) {
        return;
    }
    c.phase = if was == Phase::Active && !c.persistent { Phase::Dead } else { Phase::Inactive };
    let win = c.win.take();
    match &c.kind {
        Kind::Lock(l) => {
            if was == Phase::Active {
                l.unlocked();
            }
            if let Some(win) = win {
                log::debug!("win {win}: pointer lock released");
                st.host.pointer_capture(win, false);
            }
        }
        Kind::Confine(cp) => {
            if was == Phase::Active {
                cp.unconfined();
            }
        }
    }
}

/// Pointer focus moved: constraints on other surfaces end.
pub fn focus_changed(st: &mut State) {
    let focus = st.seat.ptr_focus.as_ref().map(|(s, _)| s.clone());
    let ids: Vec<ObjectId> = st
        .constraints
        .list
        .iter()
        .filter(|(_, c)| Some(&c.surface) != focus.as_ref() && matches!(c.phase, Phase::Requested | Phase::Active))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        deactivate(st, &id);
    }
}

/// The pointer is at (x, y) on `sid`, which has pointer focus in window
/// `win`: activates an inactive constraint whose region it is in.
pub fn check(st: &mut State, sid: &ObjectId, win: u32, x: f64, y: f64) {
    let Some(c) = st.constraints.list.values_mut().find(|c| &c.surface == sid && c.phase == Phase::Inactive) else {
        return;
    };
    let inside = match &c.region {
        None => true,
        Some(ops) => region_contains(ops, x.floor() as i32, y.floor() as i32),
    };
    if !inside {
        return;
    }
    match &c.kind {
        Kind::Lock(_) => {
            // Locked once Android grants the capture (`captured`).
            c.phase = Phase::Requested;
            c.win = Some(win);
            log::debug!("win {win}: pointer lock requested");
            st.host.pointer_capture(win, true);
        }
        Kind::Confine(cp) => {
            c.phase = Phase::Active;
            cp.confined();
        }
    }
}

/// Android's pointer capture of window `win` began or ended.
pub fn captured(st: &mut State, win: u32, on: bool) {
    let id = st
        .constraints
        .list
        .iter()
        .find(|(_, c)| c.win == Some(win) && matches!(c.kind, Kind::Lock(_)) && matches!(c.phase, Phase::Requested | Phase::Active))
        .map(|(id, _)| id.clone());
    log::debug!("win {win}: pointer capture {}", if on { "on" } else { "off" });
    match id {
        Some(id) if on => {
            let Some(c) = st.constraints.list.get_mut(&id) else { return };
            if c.phase == Phase::Requested {
                c.phase = Phase::Active;
                if let Kind::Lock(l) = &c.kind {
                    l.locked();
                }
            }
        }
        // Lost (focus left the window): unlocked; a persistent lock asks
        // again on the next pointer event over its surface.
        Some(id) => deactivate(st, &id),
        // A capture nobody wants any more (the lock went meanwhile).
        None if on => st.host.pointer_capture(win, false),
        None => {}
    }
}

/// The surface is gone: its constraints stay as inert objects.
pub fn surface_gone(st: &mut State, sid: &ObjectId) {
    let ids: Vec<ObjectId> = st.constraints.list.iter().filter(|(_, c)| &c.surface == sid).map(|(id, _)| id.clone()).collect();
    for id in ids {
        deactivate(st, &id);
        if let Some(c) = st.constraints.list.get_mut(&id) {
            c.phase = Phase::Dead;
        }
    }
}

/// Relative motion (logical px; accelerated and not) for the client of `sid`.
pub fn relative_motion(st: &State, sid: &ObjectId, time_us: u64, d: (f64, f64), raw: (f64, f64)) {
    for r in st.constraints.relative.iter().filter(|r| r.id().same_client_as(sid)) {
        r.relative_motion((time_us >> 32) as u32, time_us as u32, d.0, d.1, raw.0, raw.1);
    }
}

// ---- protocol ----------------------------------------------------------

impl GlobalDispatch<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1, ()> for State {
    fn bind(
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        res: New<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
        _: &(),
        init: &mut DataInit<'_, State>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        req: zwp_relative_pointer_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zwp_relative_pointer_manager_v1", client, dh, || {
            if let zwp_relative_pointer_manager_v1::Request::GetRelativePointer { id, .. } = req {
                let r = init.init(id, ());
                st.constraints.relative.push(r);
            }
        });
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: zwp_relative_pointer_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
    }

    fn destroyed(st: &mut State, _: ClientId, r: &zwp_relative_pointer_v1::ZwpRelativePointerV1, _: &()) {
        st.constraints.relative.retain(|x| x != r);
    }
}

impl GlobalDispatch<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, ()> for State {
    fn bind(
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        res: New<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
        _: &(),
        init: &mut DataInit<'_, State>,
    ) {
        init.init(res, ());
    }
}

fn region_ops(st: &State, region: Option<wl_region::WlRegion>) -> Option<Vec<RegionOp>> {
    region.and_then(|r| st.regions.get(&r.id()).cloned())
}

fn add(
    st: &mut State,
    res: &zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    kind: Kind,
    surface: &wl_surface::WlSurface,
    _pointer: &wl_pointer::WlPointer,
    region: Option<wl_region::WlRegion>,
    lifetime: WEnum<zwp_pointer_constraints_v1::Lifetime>,
) {
    let sid = surface.id();
    let id = match &kind {
        Kind::Lock(l) => l.id(),
        Kind::Confine(c) => c.id(),
    };
    // One seat, so one constraint per surface.
    if st.constraints.list.values().any(|c| c.surface == sid && c.phase != Phase::Dead) {
        res.post_error(zwp_pointer_constraints_v1::Error::AlreadyConstrained, "surface already has a pointer constraint");
        return;
    }
    let persistent = matches!(lifetime, WEnum::Value(zwp_pointer_constraints_v1::Lifetime::Persistent));
    let region = region_ops(st, region);
    st.constraints.list.insert(id, Constraint { kind, surface: sid.clone(), region, persistent, phase: Phase::Inactive, win: None });
    // Already under the pointer: it may apply at once.
    if let Some((focus, win)) = st.seat.ptr_focus.clone() {
        if focus == sid {
            let (x, y) = st.seat.surface_pos;
            check(st, &sid, win, x, y);
        }
    }
}

impl Dispatch<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        req: zwp_pointer_constraints_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zwp_pointer_constraints_v1", client, dh, || match req {
            zwp_pointer_constraints_v1::Request::LockPointer { id, surface, pointer, region, lifetime } => {
                let l = init.init(id, ());
                add(st, res, Kind::Lock(l), &surface, &pointer, region, lifetime);
            }
            zwp_pointer_constraints_v1::Request::ConfinePointer { id, surface, pointer, region, lifetime } => {
                let c = init.init(id, ());
                add(st, res, Kind::Confine(c), &surface, &pointer, region, lifetime);
            }
            _ => {}
        });
    }
}

fn set_region(st: &mut State, id: &ObjectId, region: Option<wl_region::WlRegion>) {
    // Applied at once rather than with the surface's next commit.
    let ops = region_ops(st, region);
    if let Some(c) = st.constraints.list.get_mut(id) {
        c.region = ops;
    }
}

fn destroyed(st: &mut State, id: &ObjectId) {
    deactivate(st, id);
    st.constraints.list.remove(id);
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        req: zwp_locked_pointer_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("zwp_locked_pointer_v1", client, dh, || {
            // The cursor position hint is ignored: Android can't warp its pointer.
            if let zwp_locked_pointer_v1::Request::SetRegion { region } = req {
                set_region(st, &res.id(), region);
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &zwp_locked_pointer_v1::ZwpLockedPointerV1, _: &()) {
        destroyed(st, &res.id());
    }
}

impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        req: zwp_confined_pointer_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("zwp_confined_pointer_v1", client, dh, || {
            if let zwp_confined_pointer_v1::Request::SetRegion { region } = req {
                set_region(st, &res.id(), region);
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &zwp_confined_pointer_v1::ZwpConfinedPointerV1, _: &()) {
        destroyed(st, &res.id());
    }
}
