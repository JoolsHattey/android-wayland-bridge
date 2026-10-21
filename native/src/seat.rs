//! wl_seat: Android pointer and key events routed to Wayland surfaces.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::protocol::{wl_keyboard, wl_pointer, wl_seat, wl_touch};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::PointerEvent;
use crate::output::to_logical;
use crate::state::{State, guard};
use crate::surface::Role;

/// The us/pc105 keymap, compiled by xkbcli (libxkbcommon parses it client-side).
const KEYMAP: &str = include_str!("keymap_us.xkb");

#[derive(Default)]
pub struct Seat {
    pub pointers: Vec<wl_pointer::WlPointer>,
    pub keyboards: Vec<wl_keyboard::WlKeyboard>,
    /// Surface under the pointer and its window.
    pub ptr_focus: Option<(ObjectId, u32)>,
    /// Android button state of the last pointer event.
    pub buttons: i32,
    pub kb_window: Option<u32>,
    pub kb_focus: Option<ObjectId>,
    pub keys: Vec<u32>,
    pub depressed: u32,
    pub locked: u32,
    pub cursor: Option<(ObjectId, i32, i32)>,
    cursor_hash: u64,
    /// Where the pointer is: its window and logical position there.
    last: Option<(u32, f64, f64)>,
    /// The pointer's position on the focused surface.
    pub surface_pos: (f64, f64),
    /// Wheel scrolling in 120ths not yet sent as whole notches to
    /// wl_pointer v5-7 clients (axis_discrete), vertical and horizontal.
    discrete: [i32; 2],
}

impl Seat {
    pub fn surface_gone(&mut self, sid: &ObjectId) {
        if self.ptr_focus.as_ref().is_some_and(|(s, _)| s == sid) {
            self.ptr_focus = None;
        }
        if self.kb_focus.as_ref() == Some(sid) {
            self.kb_focus = None;
        }
        if self.cursor.as_ref().is_some_and(|(s, _, _)| s == sid) {
            self.cursor = None;
        }
    }

    pub fn window_gone(&mut self, win: u32) {
        if self.ptr_focus.as_ref().is_some_and(|(_, w)| *w == win) {
            self.ptr_focus = None;
        }
        if self.last.is_some_and(|(w, _, _)| w == win) {
            self.last = None;
        }
        if self.kb_window == Some(win) {
            self.kb_window = None;
            self.kb_focus = None;
        }
    }

    fn pointers_for(&self, sid: &ObjectId) -> Vec<wl_pointer::WlPointer> {
        self.pointers.iter().filter(|p| p.id().same_client_as(sid)).cloned().collect()
    }

    fn keyboards_for(&self, sid: &ObjectId) -> Vec<wl_keyboard::WlKeyboard> {
        self.keyboards.iter().filter(|k| k.id().same_client_as(sid)).cloned().collect()
    }
}

/// Android MotionEvent button bits -> Linux BTN_* codes.
const BUTTONS: [(i32, u32); 5] = [(1, 0x110), (2, 0x111), (4, 0x112), (8, 0x113), (16, 0x114)];

pub fn surface_origin(st: &State, win: u32, sid: &ObjectId) -> Option<(i32, i32)> {
    let w = st.windows.get(&win)?;
    st.paint_list(w).into_iter().find(|(s, _, _)| s == sid).map(|(_, x, y)| (x, y))
}

fn set_pointer_focus(st: &mut State, target: Option<(ObjectId, u32, f64, f64)>) {
    let cur = st.seat.ptr_focus.clone();
    if cur.as_ref().map(|(s, _)| s) == target.as_ref().map(|(s, _, _, _)| s) {
        return;
    }
    if let Some((old, _)) = cur {
        let serial = st.next_serial();
        if let Some(surf) = st.surfaces.get(&old).map(|s| s.res.clone()) {
            for p in st.seat.pointers_for(&old) {
                p.leave(serial, &surf);
                if p.version() >= 5 {
                    p.frame();
                }
            }
        }
    }
    st.seat.ptr_focus = None;
    st.seat.discrete = [0; 2];
    if let Some((sid, win, x, y)) = target {
        let serial = st.next_serial();
        if let Some(surf) = st.surfaces.get(&sid).map(|s| s.res.clone()) {
            for p in st.seat.pointers_for(&sid) {
                p.enter(serial, &surf, x, y);
            }
            st.seat.ptr_focus = Some((sid, win));
            st.seat.surface_pos = (x, y);
        }
    }
    crate::constraints::focus_changed(st);
}

/// Whether `sid` is (part of) one of the window's popups.
fn in_popup(st: &State, sid: &ObjectId) -> bool {
    let mut cur = sid.clone();
    for _ in 0..64 {
        if st.popups.contains_key(&cur) {
            return true;
        }
        match st.surfaces.get(&cur).and_then(|s| s.sub.as_ref()) {
            Some(sub) => cur = sub.parent.clone(),
            None => return false,
        }
    }
    false
}

/// Android px in window `win` -> logical px in its Wayland window space.
pub fn window_point(st: &State, win: u32, x: f32, y: f32) -> (f64, f64) {
    let scale = st.window_scale(win);
    let top = st.windows.get(&win).map_or(0, |w| w.caption.top) as f64;
    (to_logical(x as f64, scale), to_logical(y as f64 - top, scale))
}

/// A drag took the pointer: the surface under it gets a leave.
pub fn pointer_grabbed(st: &mut State) {
    set_pointer_focus(st, None);
}

pub fn handle_pointer(st: &mut State, ev: PointerEvent) {
    let win = ev.win;
    let buttons = if ev.buttons < 0 { st.seat.buttons } else { ev.buttons };
    if st.data.dragging() {
        // Android runs the drag; the pointer comes back once it ends.
        st.seat.buttons = buttons;
        return;
    }
    if ev.action == 2 {
        if st.seat.buttons == 0 {
            set_pointer_focus(st, None);
            st.seat.last = None;
        }
        return;
    }
    let Some(w) = st.windows.get(&win) else { return };

    // A locked pointer stays put, and so does one without a position
    // (captured, or a touchpad scroll).
    let scale = st.window_scale(win);
    let locked = crate::constraints::locked(st);
    let positioned = !ev.x.is_nan() && !ev.y.is_nan() && !locked;
    let last = st.seat.last.filter(|(lw, _, _)| *lw == win);
    let (x, y) = if positioned {
        window_point(st, win, ev.x, ev.y)
    } else if let Some((_, lx, ly)) = last {
        (lx, ly)
    } else {
        st.seat.buttons = buttons;
        return;
    };
    // Relative motion: Android's own (sub-pixel, and the only motion of a
    // captured pointer), else the change in position. Android accelerates
    // both, except under capture, so the two values sent are the same.
    // (Injected mouse events report none, as zero.)
    let rel = match (ev.rel.filter(|(dx, dy)| *dx != 0.0 || *dy != 0.0), last) {
        (Some((dx, dy)), _) => Some((to_logical(dx as f64, scale), to_logical(dy as f64, scale))),
        (None, Some((_, lx, ly))) if positioned => Some((x - lx, y - ly)),
        _ => None,
    }
    .filter(|(dx, dy)| *dx != 0.0 || *dy != 0.0);
    st.seat.last = Some((win, x, y));

    // While a button is held, the pressed surface keeps the pointer, as
    // does a locked or confined one, and one without a position.
    let focus = st.seat.ptr_focus.clone().filter(|(_, fw)| *fw == win).map(|(s, _)| s);
    let confined = focus.as_ref().and_then(|sid| crate::constraints::confinement(st, sid).map(|b| (sid.clone(), b)));
    let target = if let Some((sid, (x0, y0, x1, y1))) = confined {
        surface_origin(st, win, &sid).map(|(ox, oy)| (sid, (x - ox as f64).clamp(x0, x1), (y - oy as f64).clamp(y0, y1)))
    } else if let Some(sid) = focus.filter(|_| st.seat.buttons != 0 || locked || !positioned) {
        surface_origin(st, win, &sid).map(|(ox, oy)| (sid, x - ox as f64, y - oy as f64))
    } else {
        st.surface_at(w, x, y)
    };
    let time = ev.time;
    set_pointer_focus(st, target.clone().map(|(s, lx, ly)| (s, win, lx, ly)));
    let Some((sid, lx, ly)) = target else {
        st.seat.buttons = buttons;
        return;
    };
    if !locked {
        st.seat.surface_pos = (lx, ly);
    }
    crate::constraints::check(st, &sid, win, lx, ly);
    let pointers = st.seat.pointers_for(&sid);
    if positioned {
        for p in &pointers {
            p.motion(time, lx, ly);
        }
    }
    if let Some(d) = rel {
        crate::constraints::relative_motion(st, &sid, ev.time_us, d, d);
    }

    if ev.action == 1 {
        scroll(st, &pointers, &ev);
    }

    let changed = st.seat.buttons ^ buttons;
    st.seat.buttons = buttons;
    for (bit, code) in BUTTONS {
        if changed & bit == 0 {
            continue;
        }
        let pressed = buttons & bit != 0;
        if pressed {
            // A press outside the open popups dismisses them and is eaten.
            let has_grab = st.windows.get(&win).is_some_and(|w| w.popups.iter().any(|p| st.popups.get(p).is_some_and(|p| p.grab)));
            if has_grab && !in_popup(st, &sid) {
                crate::shell::dismiss_popups(st, win);
                continue;
            }
        }
        log::debug!("win {win}: button {code:#x} {} at ({lx:.0},{ly:.0}) on {sid:?}", if pressed { "down" } else { "up" });
        let serial = st.next_serial();
        let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        for p in &pointers {
            p.button(serial, time, code, state);
        }
    }
    for p in &pointers {
        if p.version() >= 5 {
            p.frame();
        }
    }
}

/// Logical px per wheel notch, as libinput reports it (15 degrees).
const NOTCH: f64 = 15.0;

/// A scroll's axis events: axis_source, then per axis the notches
/// (axis_value120 from v8, whole axis_discrete steps for v5-7) and the
/// distance, or axis_stop when touchpad fingers lift. The frame follows.
fn scroll(st: &mut State, pointers: &[wl_pointer::WlPointer], ev: &PointerEvent) {
    use crate::ScrollSource;
    let scale = st.window_scale(ev.win);
    let axes = [(wl_pointer::Axis::VerticalScroll, ev.vscroll), (wl_pointer::Axis::HorizontalScroll, ev.hscroll)];
    let v120 = |amount: f32| (amount as f64 * 120.0).round() as i32;
    // Whole notches for v5-7 clients; high-resolution wheels add up to them.
    let mut discrete = [0; 2];
    if ev.source == ScrollSource::Wheel {
        for (i, (_, amount)) in axes.iter().enumerate() {
            let acc = &mut st.seat.discrete[i];
            *acc += v120(*amount);
            discrete[i] = *acc / 120;
            *acc -= discrete[i] * 120;
        }
    }
    let source = match ev.source {
        ScrollSource::Wheel => wl_pointer::AxisSource::Wheel,
        ScrollSource::Finger => wl_pointer::AxisSource::Finger,
        ScrollSource::Continuous => wl_pointer::AxisSource::Continuous,
    };
    for p in pointers {
        let v = p.version();
        if v >= 5 {
            p.axis_source(source);
        }
        for (i, (axis, amount)) in axes.iter().enumerate() {
            if *amount != 0.0 {
                let distance = match ev.source {
                    ScrollSource::Finger => to_logical(*amount as f64, scale),
                    ScrollSource::Wheel | ScrollSource::Continuous => *amount as f64 * NOTCH,
                };
                if ev.source == ScrollSource::Wheel {
                    if v >= 8 {
                        p.axis_value120(*axis, v120(*amount));
                    } else if v >= 5 && discrete[i] != 0 {
                        p.axis_discrete(*axis, discrete[i]);
                    }
                }
                p.axis(ev.time, *axis, distance);
            } else if ev.stop && v >= 5 {
                p.axis_stop(ev.time, *axis);
            }
        }
    }
}

/// Linux key code -> (modifier mask, is lock key).
fn modifier(code: u32) -> Option<(u32, bool)> {
    match code {
        42 | 54 => Some((1, false)),   // Shift
        58 => Some((2, true)),         // Caps Lock
        29 | 97 => Some((4, false)),   // Control
        56 | 100 => Some((8, false)),  // Alt (Mod1)
        69 => Some((16, true)),        // Num Lock (Mod2)
        125 | 126 => Some((64, false)), // Super (Mod4)
        _ => None,
    }
}

fn send_modifiers(st: &mut State, sid: &ObjectId) {
    let serial = st.next_serial();
    let (d, l) = (st.seat.depressed, st.seat.locked);
    for k in st.seat.keyboards_for(sid) {
        k.modifiers(serial, d, 0, l, 0);
    }
}

pub fn handle_key(st: &mut State, win: u32, code: u32, down: bool, time: u32) {
    if st.seat.kb_window != Some(win) {
        focus_window(st, win, true);
    }
    if down {
        if !st.seat.keys.contains(&code) {
            st.seat.keys.push(code);
        }
    } else {
        st.seat.keys.retain(|k| *k != code);
    }
    let mut mods_changed = false;
    if let Some((mask, lock)) = modifier(code) {
        if lock {
            if down {
                st.seat.locked ^= mask;
                mods_changed = true;
            }
        } else {
            let held = st.seat.keys.iter().any(|k| modifier(*k).is_some_and(|(m, l)| m == mask && !l));
            let before = st.seat.depressed;
            if held {
                st.seat.depressed |= mask;
            } else {
                st.seat.depressed &= !mask;
            }
            mods_changed = before != st.seat.depressed;
        }
    }
    let Some(sid) = st.seat.kb_focus.clone() else { return };
    let serial = st.next_serial();
    let state = if down { wl_keyboard::KeyState::Pressed } else { wl_keyboard::KeyState::Released };
    for k in st.seat.keyboards_for(&sid) {
        k.key(serial, time, code, state);
    }
    if mods_changed {
        send_modifiers(st, &sid);
    }
}

/// Android window focus changed.
pub fn focus_window(st: &mut State, win: u32, focused: bool) {
    if focused {
        if let Some(old) = st.seat.kb_window.filter(|w| *w != win) {
            if let Some(w) = st.windows.get_mut(&old) {
                w.focused = false;
            }
            st.configure_window(old);
        }
        st.seat.kb_window = Some(win);
    } else if st.seat.kb_window == Some(win) {
        st.seat.kb_window = None;
        // Modifiers held while focus leaves would otherwise stick.
        st.seat.keys.clear();
        st.seat.depressed = 0;
    }
    if let Some(w) = st.windows.get_mut(&win) {
        if w.focused != focused {
            w.focused = focused;
            if w.configured.is_some() {
                st.configure_window(win);
            }
        }
    }
    refocus_keyboard(st);
}

/// Points the keyboard at the focused window's topmost grabbing popup, or
/// its toplevel.
pub fn refocus_keyboard(st: &mut State) {
    let target = st.seat.kb_window.and_then(|win| {
        let w = st.windows.get(&win)?;
        let popup = w.popups.iter().rev().find(|p| st.popups.get(*p).is_some_and(|p| p.grab)).cloned();
        Some(popup.unwrap_or_else(|| w.surface.id()))
    });
    if target == st.seat.kb_focus {
        return;
    }
    crate::text_input::focus_changed(st, target.as_ref());
    if let Some(old) = st.seat.kb_focus.take() {
        let serial = st.next_serial();
        if let Some(surf) = st.surfaces.get(&old).map(|s| s.res.clone()) {
            for k in st.seat.keyboards_for(&old) {
                k.leave(serial, &surf);
            }
        }
    }
    if let Some(sid) = target {
        let serial = st.next_serial();
        let keys: Vec<u8> = st.seat.keys.iter().flat_map(|k| k.to_ne_bytes()).collect();
        if let Some(surf) = st.surfaces.get(&sid).map(|s| s.res.clone()) {
            for k in st.seat.keyboards_for(&sid) {
                k.enter(serial, &surf, keys.clone());
            }
            st.seat.kb_focus = Some(sid.clone());
            send_modifiers(st, &sid);
            if let Some(client) = surf.client() {
                crate::data::offer_selection(st, &client.id());
            }
        }
    }
}

pub fn popup_grabbed(st: &mut State, _sid: &ObjectId) {
    refocus_keyboard(st);
}

/// Sends the client's cursor image to Android as the window's pointer icon.
pub fn cursor_committed(st: &mut State, sid: &ObjectId) {
    let Some((csid, hx, hy)) = st.seat.cursor.clone() else { return };
    if &csid != sid {
        return;
    }
    let Some((_, win)) = st.seat.ptr_focus.clone() else { return };
    let Some(s) = st.surfaces.get(sid) else { return };
    let Some(t) = s.texture.as_ref().filter(|t| t.dmabuf.is_none()) else { return };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (t.width, t.height, hx, hy, win).hash(&mut h);
    t.data.hash(&mut h);
    let hash = h.finish();
    if hash == st.seat.cursor_hash {
        return;
    }
    st.seat.cursor_hash = hash;
    let Some(mut image) = surface_image(st, sid, win) else { return };
    let bs = st.surfaces.get(sid).map_or(1, |s| s.buffer_scale.max(1));
    (image.hot_x, image.hot_y) = (hx * bs, hy * bs);
    st.host.cursor(win, Some(&image));
}

/// A surface's pixels for an Android bitmap (cursor, drag shadow).
pub struct Image {
    /// Non-premultiplied 0xAARRGGBB, Android's layout.
    pub argb: Vec<i32>,
    pub width: i32,
    pub height: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    /// Image px to display px.
    pub scale: f32,
}

/// The surface's current pixels, scaled for window `win`. A dmabuf has no
/// CPU pixels, so it gives none.
pub fn surface_image(st: &State, sid: &ObjectId, win: u32) -> Option<Image> {
    let s = st.surfaces.get(sid)?;
    let t = s.texture.as_ref().filter(|t| t.dmabuf.is_none())?;
    // RGBA premultiplied -> Android's non-premultiplied 0xAARRGGBB.
    let argb = t
        .data
        .iter()
        .map(|&v| {
            let (r, g, b, a) = (v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, v >> 24);
            let un = |c: u32| if a == 0 { 0 } else { (c * 255 / a).min(255) };
            ((a << 24) | (un(r) << 16) | (un(g) << 8) | un(b)) as i32
        })
        .collect();
    // Android shows the bitmap 1:1 in display px; scale it to the window.
    let scale = st.window_scale(win) as f32 / 120.0 / s.buffer_scale.max(1) as f32;
    Some(Image { argb, width: t.width, height: t.height, hot_x: 0, hot_y: 0, scale })
}

fn keymap_fd() -> Option<(OwnedFd, u32)> {
    let fd = unsafe { libc::memfd_create(c"awb-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(fd.try_clone().ok()?);
    file.write_all(KEYMAP.as_bytes()).ok()?;
    file.write_all(&[0]).ok()?;
    Some((fd, KEYMAP.len() as u32 + 1))
}

// ---- protocol ----------------------------------------------------------

impl GlobalDispatch<wl_seat::WlSeat, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_seat::WlSeat>, _: &(), init: &mut DataInit<'_, State>) {
        let seat = init.init(res, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard);
        if seat.version() >= 2 {
            seat.name("seat0".into());
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_seat::WlSeat,
        req: wl_seat::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_seat", client, dh, || match req {
            wl_seat::Request::GetPointer { id } => {
                let p = init.init(id, ());
                st.seat.pointers.push(p);
            }
            wl_seat::Request::GetKeyboard { id } => {
                let k = init.init(id, ());
                if let Some((fd, size)) = keymap_fd() {
                    k.keymap(wl_keyboard::KeymapFormat::XkbV1, fd.as_fd(), size);
                }
                if k.version() >= 4 {
                    k.repeat_info(30, 400);
                }
                st.seat.keyboards.push(k.clone());
                // If this client's surface already has focus, tell it now.
                if let Some(sid) = st.seat.kb_focus.clone() {
                    if k.id().same_client_as(&sid) {
                        st.seat.kb_focus = None;
                        refocus_keyboard(st);
                    }
                }
            }
            wl_seat::Request::GetTouch { id } => {
                init.init(id, ());
            }
            _ => {}
        });
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_pointer::WlPointer,
        req: wl_pointer::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wl_pointer", client, dh, || {
            if let wl_pointer::Request::SetCursor { surface, hotspot_x, hotspot_y, .. } = req {
                let win = st.seat.ptr_focus.as_ref().map(|(_, w)| *w);
                match surface {
                    Some(s) => {
                        let sid = s.id();
                        if let Some(surf) = st.surfaces.get_mut(&sid) {
                            if matches!(surf.role, Role::None | Role::Cursor) {
                                surf.role = Role::Cursor;
                            }
                        }
                        st.seat.cursor = Some((sid.clone(), hotspot_x, hotspot_y));
                        st.seat.cursor_hash = 0;
                        cursor_committed(st, &sid);
                    }
                    None => {
                        st.seat.cursor = None;
                        st.seat.cursor_hash = 0;
                        if let Some(win) = win {
                            st.host.cursor(win, None);
                        }
                    }
                }
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, p: &wl_pointer::WlPointer, _: &()) {
        st.seat.pointers.retain(|x| x != p);
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_keyboard::WlKeyboard, _: wl_keyboard::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}

    fn destroyed(st: &mut State, _: ClientId, k: &wl_keyboard::WlKeyboard, _: &()) {
        st.seat.keyboards.retain(|x| x != k);
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_touch::WlTouch, _: wl_touch::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}
