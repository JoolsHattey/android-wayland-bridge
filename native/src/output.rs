//! Outputs and scaling: one wl_output per Android display, per-window scale
//! from the window's display, wp_viewporter and wp_fractional_scale_v1.
//!
//! Scales are kept in 120ths like wp_fractional_scale_v1 (Android density
//! 1.5 = 180). Window space is in logical px; `to_px` maps it to the Android
//! surface. Clients without fractional scaling get the next integer scale
//! and the renderer scales their buffers down.

use std::collections::HashMap;

use wayland_protocols::wp::fractional_scale::v1::server::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::viewporter::server::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::xdg_output::zv1::server::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_server::backend::{ClientId, GlobalId, ObjectId};
use wayland_server::protocol::wl_output;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::DisplayInfo;
use crate::state::{State, guard};

/// Logical px -> Android px at `scale120`, rounding half away from zero
/// (what wp_fractional_scale_v1 specifies for buffer sizes).
pub fn to_px(logical: f64, scale120: u32) -> i32 {
    (logical * scale120 as f64 / 120.0).round() as i32
}

pub fn to_logical(px: f64, scale120: u32) -> f64 {
    px * 120.0 / scale120 as f64
}

/// The integer scale for clients without fractional scaling.
pub fn int_scale(scale120: u32) -> i32 {
    scale120.div_ceil(120).max(1) as i32
}

pub struct Output {
    pub info: DisplayInfo,
    global: GlobalId,
    /// Bound wl_output resources (all clients).
    pub resources: Vec<wl_output::WlOutput>,
    xdg: Vec<(zxdg_output_v1::ZxdgOutputV1, wl_output::WlOutput)>,
    /// Logical x of this output (displays laid out left to right by id).
    x: i32,
}

#[derive(Default)]
pub struct Outputs {
    pub map: HashMap<i32, Output>,
    /// Display new windows are expected to open on (last focused window's).
    pub default: Option<i32>,
}

impl Outputs {
    pub fn scale120(&self, display: i32) -> u32 {
        self.map.get(&display).map_or(120, |o| o.info.scale120)
    }

    pub fn default_display(&self) -> i32 {
        self.default.filter(|d| self.map.contains_key(d)).or_else(|| self.map.keys().min().copied()).unwrap_or(0)
    }

    fn relayout(&mut self) {
        let mut ids: Vec<i32> = self.map.keys().copied().collect();
        ids.sort();
        let mut x = 0;
        for id in ids {
            let o = self.map.get_mut(&id).unwrap();
            o.x = x;
            x += to_logical(o.info.width as f64, o.info.scale120) as i32;
        }
    }
}

fn send_output(o: &wl_output::WlOutput, info: &DisplayInfo, x: i32) {
    o.geometry(
        x,
        0,
        info.phys_mm.0,
        info.phys_mm.1,
        wl_output::Subpixel::Unknown,
        "Android".into(),
        info.name.clone(),
        wl_output::Transform::Normal,
    );
    o.mode(wl_output::Mode::Current | wl_output::Mode::Preferred, info.width, info.height, info.refresh_mhz);
    if o.version() >= 2 {
        o.scale(int_scale(info.scale120));
    }
    if o.version() >= 4 {
        o.name(format!("AWB-{}", info.id));
        o.description(info.name.clone());
    }
}

fn send_xdg_output(xo: &zxdg_output_v1::ZxdgOutputV1, output: &wl_output::WlOutput, info: &DisplayInfo, x: i32) {
    xo.logical_position(x, 0);
    xo.logical_size(
        to_logical(info.width as f64, info.scale120) as i32,
        to_logical(info.height as f64, info.scale120) as i32,
    );
    if xo.version() >= 2 {
        xo.name(format!("AWB-{}", info.id));
        xo.description(info.name.clone());
    }
    if xo.version() >= 3 {
        if output.version() >= 2 {
            output.done();
        }
    } else {
        xo.done();
    }
}

/// A display appeared or changed (size, density).
pub fn display_changed(st: &mut State, info: DisplayInfo) {
    log::info!("display {}: {}x{} scale {}/120 '{}'", info.id, info.width, info.height, info.scale120, info.name);
    let id = info.id;
    let scale_changed = match st.outputs.map.get_mut(&id) {
        Some(o) => {
            let changed = o.info.scale120 != info.scale120;
            o.info = info;
            changed
        }
        None => {
            let global = st.dh.create_global::<State, wl_output::WlOutput, i32>(4, id);
            st.outputs.map.insert(id, Output { info, global, resources: Vec::new(), xdg: Vec::new(), x: 0 });
            false
        }
    };
    st.outputs.relayout();
    for o in st.outputs.map.values() {
        for r in &o.resources {
            send_output(r, &o.info, o.x);
        }
        for (xo, r) in &o.xdg {
            send_xdg_output(xo, r, &o.info, o.x);
        }
        for r in &o.resources {
            if r.version() >= 2 {
                r.done();
            }
        }
    }
    if scale_changed {
        let wins: Vec<u32> = st.windows.values().filter(|w| w.display == id).map(|w| w.id).collect();
        for w in wins {
            st.configure_window(w);
        }
    }
}

pub fn display_removed(st: &mut State, id: i32) {
    if let Some(o) = st.outputs.map.remove(&id) {
        log::info!("display {id} removed");
        st.dh.disable_global::<State>(o.global);
        st.outputs.relayout();
    }
}

/// Makes sure the surface knows the output and scale of `display`: sends
/// wl_surface.leave/enter, preferred_buffer_scale and the fractional
/// preferred_scale when they changed.
pub fn ensure_scale(st: &mut State, sid: &ObjectId, display: i32) {
    let scale120 = st.outputs.scale120(display);
    let Some(s) = st.surfaces.get(sid) else { return };
    if s.entered == Some((display, scale120)) {
        return;
    }
    let res = s.res.clone();
    let old = s.entered.map(|(d, _)| d);
    if old != Some(display) {
        let same_client = |o: &&wl_output::WlOutput| o.id().same_client_as(sid);
        if let Some(o) = old.and_then(|d| st.outputs.map.get(&d)) {
            for r in o.resources.iter().filter(same_client) {
                res.leave(r);
            }
        }
        if let Some(o) = st.outputs.map.get(&display) {
            for r in o.resources.iter().filter(same_client) {
                res.enter(r);
            }
        }
    }
    if res.version() >= 6 {
        res.preferred_buffer_scale(int_scale(scale120));
    }
    let s = st.surfaces.get_mut(sid).unwrap();
    if let Some(f) = &s.fractional {
        f.preferred_scale(scale120);
    }
    s.entered = Some((display, scale120));
}

/// Display of the window a surface is in, or the default display.
pub fn display_of(st: &State, sid: &ObjectId) -> i32 {
    st.window_of(sid)
        .and_then(|w| st.windows.get(&w))
        .map(|w| w.display)
        .unwrap_or_else(|| st.outputs.default_display())
}

// ---- wl_output / xdg_output -------------------------------------------

impl GlobalDispatch<wl_output::WlOutput, i32> for State {
    fn bind(st: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_output::WlOutput>, id: &i32, init: &mut DataInit<'_, State>) {
        let o = init.init(res, *id);
        let Some(out) = st.outputs.map.get_mut(id) else { return };
        send_output(&o, &out.info, out.x);
        if o.version() >= 2 {
            o.done();
        }
        out.resources.push(o);
    }
}

impl Dispatch<wl_output::WlOutput, i32> for State {
    fn request(_: &mut State, _: &Client, _: &wl_output::WlOutput, _: wl_output::Request, _: &i32, _: &DisplayHandle, _: &mut DataInit<'_, State>) {}

    fn destroyed(st: &mut State, _: ClientId, o: &wl_output::WlOutput, id: &i32) {
        if let Some(out) = st.outputs.map.get_mut(id) {
            out.resources.retain(|x| x != o);
            out.xdg.retain(|(_, x)| x != o);
        }
    }
}

impl GlobalDispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<zxdg_output_manager_v1::ZxdgOutputManagerV1>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        req: zxdg_output_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zxdg_output_manager_v1", client, dh, || {
            if let zxdg_output_manager_v1::Request::GetXdgOutput { id, output } = req {
                let xo = init.init(id, ());
                let Some(out) = output.data::<i32>().and_then(|d| st.outputs.map.get_mut(d)) else { return };
                send_xdg_output(&xo, &output, &out.info, out.x);
                out.xdg.push((xo, output));
            }
        });
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn request(_: &mut State, _: &Client, _: &zxdg_output_v1::ZxdgOutputV1, _: zxdg_output_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}

    fn destroyed(st: &mut State, _: ClientId, xo: &zxdg_output_v1::ZxdgOutputV1, _: &()) {
        for out in st.outputs.map.values_mut() {
            out.xdg.retain(|(x, _)| x != xo);
        }
    }
}

// ---- wp_viewporter -----------------------------------------------------

impl GlobalDispatch<wp_viewporter::WpViewporter, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wp_viewporter::WpViewporter>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &wp_viewporter::WpViewporter,
        req: wp_viewporter::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wp_viewporter", client, dh, || {
            if let wp_viewporter::Request::GetViewport { id, surface } = req {
                let sid = surface.id();
                init.init(id, sid.clone());
                let Some(s) = st.surfaces.get_mut(&sid) else { return };
                if s.has_viewport {
                    res.post_error(wp_viewporter::Error::ViewportExists, "surface already has a viewport");
                    return;
                }
                s.has_viewport = true;
            }
        });
    }
}

impl Dispatch<wp_viewport::WpViewport, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &wp_viewport::WpViewport,
        req: wp_viewport::Request,
        sid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wp_viewport", client, dh, || {
            let Some(s) = st.surfaces.get_mut(sid) else {
                res.post_error(wp_viewport::Error::NoSurface, "surface is gone");
                return;
            };
            match req {
                wp_viewport::Request::SetSource { x, y, width, height } => {
                    if x == -1.0 && y == -1.0 && width == -1.0 && height == -1.0 {
                        s.pending.vp_src = Some(None);
                    } else if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                        res.post_error(wp_viewport::Error::BadValue, "invalid source rectangle");
                    } else {
                        s.pending.vp_src = Some(Some([x, y, width, height]));
                    }
                }
                wp_viewport::Request::SetDestination { width, height } => {
                    if width == -1 && height == -1 {
                        s.pending.vp_dst = Some(None);
                    } else if width <= 0 || height <= 0 {
                        res.post_error(wp_viewport::Error::BadValue, "invalid destination size");
                    } else {
                        s.pending.vp_dst = Some(Some((width, height)));
                    }
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, _: &wp_viewport::WpViewport, sid: &ObjectId) {
        if let Some(s) = st.surfaces.get_mut(sid) {
            s.has_viewport = false;
            s.pending.vp_src = Some(None);
            s.pending.vp_dst = Some(None);
        }
    }
}

// ---- wp_fractional_scale_v1 --------------------------------------------

impl GlobalDispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for State {
    fn bind(
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        res: New<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
        _: &(),
        init: &mut DataInit<'_, State>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        req: wp_fractional_scale_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wp_fractional_scale_manager_v1", client, dh, || {
            if let wp_fractional_scale_manager_v1::Request::GetFractionalScale { id, surface } = req {
                let sid = surface.id();
                let f = init.init(id, sid.clone());
                let display = display_of(st, &sid);
                let Some(s) = st.surfaces.get_mut(&sid) else { return };
                if s.fractional.is_some() {
                    res.post_error(
                        wp_fractional_scale_manager_v1::Error::FractionalScaleExists,
                        "surface already has a fractional scale object",
                    );
                    return;
                }
                s.fractional = Some(f);
                // Resend everything so the new object gets its first scale.
                s.entered = s.entered.map(|(d, _)| (d, 0));
                ensure_scale(st, &sid, display);
            }
        });
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ObjectId> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        _: wp_fractional_scale_v1::Request,
        _: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
    }

    fn destroyed(st: &mut State, _: ClientId, _: &wp_fractional_scale_v1::WpFractionalScaleV1, sid: &ObjectId) {
        if let Some(s) = st.surfaces.get_mut(sid) {
            s.fractional = None;
        }
    }
}
