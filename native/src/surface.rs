//! wl_compositor, wl_surface, wl_region, wl_subcompositor: double-buffered
//! surface state, subsurface sync semantics and commit handling.

use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_v1;
use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::shm::{Texture, next_texture_serial};
use crate::syncobj::{CommitSync, Point, Release};
use crate::state::{Buffer, State, guard};

#[derive(Debug, Clone, Copy)]
pub enum RegionOp {
    Add(i32, i32, i32, i32),
    Sub(i32, i32, i32, i32),
}

fn region_contains(ops: &[RegionOp], x: i32, y: i32) -> bool {
    let inside = |rx: i32, ry: i32, rw: i32, rh: i32| x >= rx && y >= ry && x < rx.saturating_add(rw) && y < ry.saturating_add(rh);
    let mut r = false;
    for op in ops {
        match *op {
            RegionOp::Add(a, b, c, d) if inside(a, b, c, d) => r = true,
            RegionOp::Sub(a, b, c, d) if inside(a, b, c, d) => r = false,
            _ => {}
        }
    }
    r
}

/// Double-buffered state (pending or cached for synchronized subsurfaces).
#[derive(Default)]
pub struct SurfState {
    /// None = no attach; Some(None) = attached null (unmap).
    pub buffer: Option<Option<wl_buffer::WlBuffer>>,
    pub scale: Option<i32>,
    /// None = unchanged; Some(None) = infinite.
    pub input: Option<Option<Vec<RegionOp>>>,
    pub frame_cbs: Vec<wl_callback::WlCallback>,
    /// wp_viewport source (x, y, w, h in surface units); Some(None) = unset.
    pub vp_src: Option<Option<[f64; 4]>>,
    /// wp_viewport destination size; Some(None) = unset.
    pub vp_dst: Option<Option<(i32, i32)>>,
    /// Explicit sync points for the attached buffer.
    pub acquire: Option<Point>,
    pub release: Option<Point>,
}

impl SurfState {
    fn merge(&mut self, newer: SurfState) {
        if newer.buffer.is_some() {
            // The older cached buffer is never shown: give it back.
            if let Some(Some(old)) = &self.buffer {
                if newer.buffer.as_ref().is_none_or(|b| b.as_ref() != Some(old)) && old.is_alive() {
                    old.release();
                }
            }
            if let Some(r) = self.release.take() {
                r.signal();
            }
            self.buffer = newer.buffer;
            self.acquire = newer.acquire;
            self.release = newer.release;
        }
        if newer.scale.is_some() {
            self.scale = newer.scale;
        }
        if newer.input.is_some() {
            self.input = newer.input;
        }
        if newer.vp_src.is_some() {
            self.vp_src = newer.vp_src;
        }
        if newer.vp_dst.is_some() {
            self.vp_dst = newer.vp_dst;
        }
        self.frame_cbs.extend(newer.frame_cbs);
    }
}

pub struct SubInfo {
    pub parent: ObjectId,
    pub pos: (i32, i32),
    pub pending_pos: Option<(i32, i32)>,
    pub sync: bool,
}

pub enum Role {
    None,
    Xdg,
    Sub,
    Cursor,
    DndIcon,
}

pub struct Surface {
    pub res: wl_surface::WlSurface,
    pub pending: SurfState,
    /// State committed on a synchronized subsurface, applied with the parent.
    pub cached: Option<SurfState>,
    pub texture: Option<Texture>,
    pub buffer_scale: i32,
    pub input: Option<Vec<RegionOp>>,
    pub frame_cbs: Vec<wl_callback::WlCallback>,
    pub role: Role,
    /// The xdg_surface object, if one was created for this surface.
    pub xdg: Option<ObjectId>,
    pub sub: Option<SubInfo>,
    pub above: Vec<ObjectId>,
    pub below: Vec<ObjectId>,
    pub has_viewport: bool,
    pub vp_src: Option<[f64; 4]>,
    pub vp_dst: Option<(i32, i32)>,
    pub fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// Display and scale (120ths) last announced to the client.
    pub entered: Option<(i32, u32)>,
    /// Explicit sync extension, if the client created one.
    pub syncobj: Option<WpLinuxDrmSyncobjSurfaceV1>,
}

impl Surface {
    /// Size in surface-local (logical) px: the viewport destination, else
    /// the viewport source size, else the buffer size over its scale.
    pub fn logical_size(&self) -> Option<(i32, i32)> {
        let t = self.texture.as_ref()?;
        if let Some(d) = self.vp_dst {
            return Some(d);
        }
        if let Some(src) = self.vp_src {
            return Some((src[2] as i32, src[3] as i32));
        }
        let s = self.buffer_scale.max(1);
        Some((t.width / s, t.height / s))
    }

    /// The part of the texture shown, in texture px (x, y, w, h), clamped
    /// to the texture.
    pub fn source_rect(&self) -> Option<[f64; 4]> {
        let t = self.texture.as_ref()?;
        let (tw, th) = (t.width as f64, t.height as f64);
        Some(match self.vp_src {
            Some([x, y, w, h]) => {
                let s = self.buffer_scale.max(1) as f64;
                let (x, y) = ((x * s).min(tw), (y * s).min(th));
                [x, y, (w * s).min(tw - x), (h * s).min(th - y)]
            }
            None => [0.0, 0.0, tw, th],
        })
    }

    pub fn accepts_input(&self, x: i32, y: i32) -> bool {
        match &self.input {
            None => true,
            Some(ops) => region_contains(ops, x, y),
        }
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_compositor::WlCompositor>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_compositor::WlCompositor,
        req: wl_compositor::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_compositor", client, dh, || match req {
            wl_compositor::Request::CreateSurface { id } => {
                let s = init.init(id, ());
                if s.version() >= 6 {
                    s.preferred_buffer_transform(wl_output::Transform::Normal);
                }
                st.surfaces.insert(
                    s.id(),
                    Surface {
                        res: s,
                        pending: SurfState::default(),
                        cached: None,
                        texture: None,
                        buffer_scale: 1,
                        input: None,
                        frame_cbs: Vec::new(),
                        role: Role::None,
                        xdg: None,
                        sub: None,
                        above: Vec::new(),
                        below: Vec::new(),
                        has_viewport: false,
                        vp_src: None,
                        vp_dst: None,
                        fractional: None,
                        entered: None,
                        syncobj: None,
                    },
                );
            }
            wl_compositor::Request::CreateRegion { id } => {
                let r = init.init(id, ());
                st.regions.insert(r.id(), Vec::new());
            }
            _ => {}
        });
    }
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn request(
        st: &mut State,
        _: &Client,
        r: &wl_region::WlRegion,
        req: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        let Some(ops) = st.regions.get_mut(&r.id()) else { return };
        match req {
            wl_region::Request::Add { x, y, width, height } => ops.push(RegionOp::Add(x, y, width, height)),
            wl_region::Request::Subtract { x, y, width, height } => ops.push(RegionOp::Sub(x, y, width, height)),
            _ => {}
        }
        // Bound the op list so a client cannot grow it forever.
        if ops.len() > 4096 {
            ops.drain(..2048);
        }
    }

    fn destroyed(st: &mut State, _: ClientId, r: &wl_region::WlRegion, _: &()) {
        st.regions.remove(&r.id());
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_callback::WlCallback, _: wl_callback::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &wl_surface::WlSurface,
        req: wl_surface::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_surface", client, dh, || {
            let sid = res.id();
            match req {
                wl_surface::Request::Frame { callback } => {
                    let cb = init.init(callback, ());
                    if let Some(s) = st.surfaces.get_mut(&sid) {
                        s.pending.frame_cbs.push(cb);
                    }
                }
                wl_surface::Request::Commit => commit(st, &sid),
                _ => {
                    let Some(s) = st.surfaces.get_mut(&sid) else { return };
                    match req {
                        wl_surface::Request::Attach { buffer, .. } => s.pending.buffer = Some(buffer),
                        wl_surface::Request::SetBufferScale { scale } => {
                            if scale < 1 {
                                res.post_error(wl_surface::Error::InvalidScale, "scale must be positive");
                            } else {
                                s.pending.scale = Some(scale);
                            }
                        }
                        wl_surface::Request::SetInputRegion { region } => {
                            let ops = region.and_then(|r| st.regions.get(&r.id()).cloned());
                            s.pending.input = Some(ops);
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &wl_surface::WlSurface, _: &()) {
        let sid = res.id();
        destroy_surface(st, &sid);
    }
}

fn destroy_surface(st: &mut State, sid: &ObjectId) {
    st.mark_dirty(sid);
    let Some(s) = st.surfaces.remove(sid) else { return };
    // Detach from the parent and orphan the children.
    if let Some(sub) = &s.sub {
        if let Some(p) = st.surfaces.get_mut(&sub.parent) {
            p.above.retain(|c| c != sid);
            p.below.retain(|c| c != sid);
        }
    }
    for c in s.above.iter().chain(s.below.iter()) {
        if let Some(cs) = st.surfaces.get_mut(c) {
            cs.sub = None;
        }
    }
    st.seat.surface_gone(sid);
    release_texture(st, s.texture, None);
    if let Some(r) = &mut st.gpu {
        r.forget_surface(sid);
    }
}

/// Gives a replaced dmabuf back to its client once the GPU no longer reads
/// it: its commit's release point, and the wl_buffer unless it is still the
/// current buffer (`current`).
fn release_texture(st: &mut State, old: Option<Texture>, current: Option<&ObjectId>) {
    let Some(t) = old else { return };
    let Some((buffer, d)) = t.dmabuf else { return };
    let buffer = (current != Some(&buffer.id()) && buffer.is_alive()).then_some(buffer);
    let release = t.sync.map(|s| s.release);
    match &mut st.gpu {
        Some(r) => r.release(buffer, release, &d),
        // Dropping `release` signals it.
        None => buffer.into_iter().for_each(|b| b.release()),
    }
}

fn effectively_sync(st: &State, sid: &ObjectId) -> bool {
    let mut cur = sid.clone();
    for _ in 0..64 {
        let Some(sub) = st.surfaces.get(&cur).and_then(|s| s.sub.as_ref()) else { return false };
        if sub.sync {
            return true;
        }
        cur = sub.parent.clone();
    }
    false
}

fn commit(st: &mut State, sid: &ObjectId) {
    if !crate::syncobj::validate(st, sid) {
        return;
    }
    let Some(s) = st.surfaces.get_mut(sid) else { return };
    let pending = std::mem::take(&mut s.pending);
    if effectively_sync(st, sid) {
        let s = st.surfaces.get_mut(sid).unwrap();
        match &mut s.cached {
            Some(c) => c.merge(pending),
            None => s.cached = Some(pending),
        }
        return;
    }
    apply(st, sid, pending);
}

/// Makes `state` current on the surface, then applies cached state of
/// synchronized children and runs role-specific commit logic.
fn apply(st: &mut State, sid: &ObjectId, mut state: SurfState) {
    let sync = match (state.acquire.take(), state.release.take()) {
        (Some(acquire), Some(release)) => Some(CommitSync { acquire, release: Release(release) }),
        _ => None,
    };
    let mut new_texture: Option<Option<Texture>> = None;
    if let Some(buffer) = &state.buffer {
        new_texture = Some(match buffer {
            Some(b) => {
                let (tex, copied) = match st.buffers.get(&b.id()) {
                    Some(Buffer::Shm(info)) => (info.read(), true),
                    Some(Buffer::Dmabuf(d)) => (
                        Some(Texture {
                            width: d.width,
                            height: d.height,
                            data: Vec::new(),
                            opaque: false,
                            dmabuf: Some((b.clone(), d.clone())),
                            sync,
                            serial: next_texture_serial(),
                        }),
                        false,
                    ),
                    None => (None, false),
                };
                if tex.is_none() {
                    log::warn!("client buffer unreadable; disconnecting client");
                    if let Ok(client) = st.dh.get_client(b.id()) {
                        client.kill(
                            &st.dh,
                            wayland_server::backend::protocol::ProtocolError {
                                code: 0,
                                object_id: b.id().protocol_id(),
                                object_interface: "wl_buffer".into(),
                                message: "buffer memory is not accessible".into(),
                            },
                        );
                    }
                    return;
                }
                // Shm contents are copied, so the client may reuse the buffer
                // now. A dmabuf is released once it is replaced and drawn.
                if copied {
                    b.release();
                }
                tex
            }
            None => None,
        });
    }
    let Some(s) = st.surfaces.get_mut(sid) else { return };
    let mut replaced = None;
    if let Some(t) = new_texture {
        replaced = std::mem::replace(&mut s.texture, t);
    }
    let current = s.texture.as_ref().and_then(|t| t.dmabuf.as_ref()).map(|(b, _)| b.id());
    let s = {
        release_texture(st, replaced, current.as_ref());
        st.surfaces.get_mut(sid).unwrap()
    };
    if let Some(scale) = state.scale {
        s.buffer_scale = scale;
    }
    if let Some(input) = state.input {
        s.input = input;
    }
    if let Some(src) = state.vp_src {
        s.vp_src = src;
    }
    if let Some(dst) = state.vp_dst {
        s.vp_dst = dst;
    }
    s.frame_cbs.extend(state.frame_cbs);
    let children: Vec<ObjectId> = s.below.iter().chain(s.above.iter()).cloned().collect();
    let is_cursor = matches!(s.role, Role::Cursor);
    let xdg = s.xdg.clone();

    for c in children {
        let Some(cs) = st.surfaces.get_mut(&c) else { continue };
        if let Some(sub) = cs.sub.as_mut() {
            if let Some(p) = sub.pending_pos.take() {
                sub.pos = p;
            }
        }
        if let Some(cached) = cs.cached.take() {
            apply(st, &c, cached);
        }
    }

    if is_cursor {
        crate::seat::cursor_committed(st, sid);
    } else if let Some(xs) = xdg {
        crate::shell::xdg_committed(st, &xs);
    }
    st.mark_dirty(sid);
    // Surfaces not shown in any window would wait forever for a frame.
    if st.window_of(sid).is_none() {
        let now = st.now_ms();
        if let Some(s) = st.surfaces.get_mut(sid) {
            for cb in s.frame_cbs.drain(..) {
                cb.done(now);
            }
        }
    }
}

// ---- subsurfaces -------------------------------------------------------

impl GlobalDispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_subcompositor::WlSubcompositor>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &wl_subcompositor::WlSubcompositor,
        req: wl_subcompositor::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_subcompositor", client, dh, || {
            if let wl_subcompositor::Request::GetSubsurface { id, surface, parent } = req {
                let sub = init.init(id, surface.id());
                let (sid, pid) = (surface.id(), parent.id());
                if sid == pid || st.surfaces.get(&sid).is_none_or(|s| !matches!(s.role, Role::None)) {
                    res.post_error(wl_subcompositor::Error::BadSurface, "surface already has a role");
                    return;
                }
                // Reject cycles: the parent must not be a descendant.
                let mut cur = pid.clone();
                for _ in 0..64 {
                    match st.surfaces.get(&cur).and_then(|s| s.sub.as_ref()) {
                        Some(si) if si.parent == sid => {
                            res.post_error(wl_subcompositor::Error::BadParent, "parent is a descendant");
                            return;
                        }
                        Some(si) => cur = si.parent.clone(),
                        None => break,
                    }
                }
                let _ = sub;
                if let Some(s) = st.surfaces.get_mut(&sid) {
                    s.role = Role::Sub;
                    s.sub = Some(SubInfo { parent: pid.clone(), pos: (0, 0), pending_pos: None, sync: true });
                }
                if let Some(p) = st.surfaces.get_mut(&pid) {
                    p.above.push(sid);
                }
            }
        });
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_subsurface::WlSubsurface,
        req: wl_subsurface::Request,
        sid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wl_subsurface", client, dh, || {
            let Some(parent) = st.surfaces.get(sid).and_then(|s| s.sub.as_ref()).map(|s| s.parent.clone()) else {
                return;
            };
            match req {
                wl_subsurface::Request::SetPosition { x, y } => {
                    if let Some(sub) = st.surfaces.get_mut(sid).and_then(|s| s.sub.as_mut()) {
                        sub.pending_pos = Some((x, y));
                    }
                }
                wl_subsurface::Request::PlaceAbove { sibling } => restack(st, sid, &parent, &sibling.id(), true),
                wl_subsurface::Request::PlaceBelow { sibling } => restack(st, sid, &parent, &sibling.id(), false),
                wl_subsurface::Request::SetSync => {
                    if let Some(sub) = st.surfaces.get_mut(sid).and_then(|s| s.sub.as_mut()) {
                        sub.sync = true;
                    }
                }
                wl_subsurface::Request::SetDesync => {
                    if let Some(sub) = st.surfaces.get_mut(sid).and_then(|s| s.sub.as_mut()) {
                        sub.sync = false;
                    }
                    if !effectively_sync(st, sid) {
                        if let Some(cached) = st.surfaces.get_mut(sid).and_then(|s| s.cached.take()) {
                            apply(st, sid, cached);
                        }
                    }
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, _: &wl_subsurface::WlSubsurface, sid: &ObjectId) {
        st.mark_dirty(sid);
        let Some(s) = st.surfaces.get_mut(sid) else { return };
        let Some(sub) = s.sub.take() else { return };
        s.role = Role::None;
        s.cached = None;
        if let Some(p) = st.surfaces.get_mut(&sub.parent) {
            p.above.retain(|c| c != sid);
            p.below.retain(|c| c != sid);
        }
    }
}

/// Places `sid` directly above/below `sibling` (which may be the parent).
fn restack(st: &mut State, sid: &ObjectId, parent: &ObjectId, sibling: &ObjectId, above: bool) {
    let Some(p) = st.surfaces.get_mut(parent) else { return };
    p.above.retain(|c| c != sid);
    p.below.retain(|c| c != sid);
    if sibling == parent {
        if above {
            p.above.insert(0, sid.clone());
        } else {
            p.below.push(sid.clone());
        }
        return;
    }
    for list in [&mut p.below, &mut p.above] {
        if let Some(i) = list.iter().position(|c| c == sibling) {
            list.insert(if above { i + 1 } else { i }, sid.clone());
            return;
        }
    }
    p.above.push(sid.clone());
}
