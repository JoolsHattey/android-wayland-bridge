//! zwp_text_input_v3: Android's IME for Linux apps. The focused text input's
//! state (enabled, content type, surrounding text, cursor rectangle) goes to
//! the window's view, whose InputConnection sends the IME's edits back.

use std::collections::HashMap;

use wayland_protocols::wp::text_input::zv3::server::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::output::to_px;
use crate::state::{State, guard};

/// What Android needs to know about the focused text input.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct ImeState {
    pub purpose: u32,
    pub hint: u32,
    /// Surrounding text with cursor and anchor in UTF-16 units (Android's).
    pub text: Option<(String, i32, i32)>,
    /// Cursor rectangle in window px (x, y, w, h).
    pub rect: Option<[i32; 4]>,
}

#[derive(Clone, Default)]
struct Pending {
    enabled: bool,
    purpose: u32,
    hint: u32,
    /// Surrounding text with cursor and anchor in bytes (the protocol's).
    text: Option<(String, i32, i32)>,
    /// Surface-local logical rect.
    rect: Option<[i32; 4]>,
}

struct TextInput {
    res: zwp_text_input_v3::ZwpTextInputV3,
    /// Surface it was told it entered.
    focus: Option<ObjectId>,
    pending: Pending,
    current: Pending,
    /// Commit requests so far: the serial of each `done`.
    commits: u32,
}

#[derive(Default)]
pub struct TextInputs {
    inputs: HashMap<ObjectId, TextInput>,
    /// What Android was last told, and for which window.
    sent: Option<(u32, ImeState)>,
    /// The IME's composing text and cursor (bytes), re-sent with each edit
    /// since every `done` clears it.
    preedit: Option<(String, i32)>,
}

/// An edit from Android's IME (`Cmd::Ime`). Lengths are UTF-16 units.
pub enum ImeEdit {
    Commit(String),
    Preedit(String, i32),
    Delete(i32, i32),
    /// Turns the `before` units before the cursor back into composing text.
    Recompose(String, i32),
}

/// Byte length of the last `units` UTF-16 units of `s` (or the first, from the front).
fn bytes_for_units(s: &str, units: i32, from_end: bool) -> u32 {
    let mut left = units.max(0) as usize;
    let mut bytes = 0;
    let mut step = |c: char| {
        if left == 0 {
            return false;
        }
        left = left.saturating_sub(c.len_utf16());
        bytes += c.len_utf8();
        true
    };
    if from_end {
        for c in s.chars().rev() {
            if !step(c) {
                break;
            }
        }
    } else {
        for c in s.chars() {
            if !step(c) {
                break;
            }
        }
    }
    bytes as u32
}

fn units_before(s: &str, byte: i32) -> i32 {
    s.get(..byte.clamp(0, s.len() as i32) as usize).map_or(0, |p| p.encode_utf16().count() as i32)
}

fn byte_of_unit(s: &str, unit: i32) -> i32 {
    bytes_for_units(s, unit, false) as i32
}

/// The enabled text input on the keyboard focus, as Android should see it.
fn active(st: &State) -> Option<(u32, &TextInput)> {
    let win = st.seat.kb_window?;
    let focus = st.seat.kb_focus.as_ref()?;
    let ti = st.text_inputs.inputs.values().find(|t| t.focus.as_ref() == Some(focus) && t.current.enabled)?;
    Some((win, ti))
}

fn ime_state(st: &State, win: u32, ti: &TextInput) -> ImeState {
    let c = &ti.current;
    let text = c.text.as_ref().map(|(t, cursor, anchor)| (t.clone(), units_before(t, *cursor), units_before(t, *anchor)));
    let rect = c.rect.and_then(|[x, y, w, h]| {
        let sid = ti.focus.as_ref()?;
        let (ox, oy) = crate::seat::surface_origin(st, win, sid)?;
        let scale = st.window_scale(win);
        let top = st.windows.get(&win)?.caption.top;
        let px = |v: i32| to_px(v as f64, scale);
        Some([px(ox + x), px(oy + y) + top, px(w).max(1), px(h).max(1)])
    });
    ImeState { purpose: c.purpose, hint: c.hint, text, rect }
}

/// Tells Android about a change in the focused text input, if any.
pub fn update(st: &mut State) {
    let desired = active(st).map(|(win, ti)| (win, ime_state(st, win, ti)));
    if desired == st.text_inputs.sent {
        return;
    }
    if let Some((old, _)) = &st.text_inputs.sent {
        if desired.as_ref().is_none_or(|(w, _)| w != old) {
            st.host.text_input(*old, None);
        }
    }
    if desired.as_ref().map(|(w, _)| *w) != st.text_inputs.sent.as_ref().map(|(w, _)| *w) {
        st.text_inputs.preedit = None;
    }
    if let Some((win, state)) = &desired {
        st.host.text_input(*win, Some(state));
    }
    st.text_inputs.sent = desired;
}

/// Keyboard focus moved to `sid` (or nowhere): text inputs follow it.
pub fn focus_changed(st: &mut State, sid: Option<&ObjectId>) {
    let client = sid.and_then(|s| st.surfaces.get(s)).and_then(|s| s.res.client()).map(|c| c.id());
    let surface = sid.and_then(|s| st.surfaces.get(s)).map(|s| s.res.clone());
    for ti in st.text_inputs.inputs.values_mut() {
        let mine = client.is_some() && ti.res.client().map(|c| c.id()) == client;
        let target = if mine { sid.cloned() } else { None };
        if ti.focus == target {
            continue;
        }
        if let Some(old) = ti.focus.take() {
            if let Some(s) = st.surfaces.get(&old) {
                ti.res.leave(&s.res);
            }
        }
        if let (Some(t), Some(s)) = (target, &surface) {
            ti.res.enter(s);
            ti.focus = Some(t);
        }
    }
    update(st);
}

/// Applies an IME edit to the focused text input.
pub fn edit(st: &mut State, win: u32, edit: ImeEdit) {
    let Some((w, ti)) = active(st) else { return };
    if w != win {
        return;
    }
    let (res, serial) = (ti.res.clone(), ti.commits);
    let text = ti.current.text.clone();
    let preedit = &mut st.text_inputs.preedit;
    match edit {
        ImeEdit::Commit(t) => {
            *preedit = None;
            res.commit_string(Some(t));
        }
        ImeEdit::Preedit(t, cursor) => {
            let c = byte_of_unit(&t, cursor);
            *preedit = (!t.is_empty()).then(|| (t, c));
        }
        ImeEdit::Delete(before, after) => {
            let (b, a) = match &text {
                Some((s, cursor, _)) => {
                    let cursor = (*cursor).clamp(0, s.len() as i32) as usize;
                    match (s.get(..cursor), s.get(cursor..)) {
                        (Some(pre), Some(post)) => (bytes_for_units(pre, before, true), bytes_for_units(post, after, false)),
                        _ => (before.max(0) as u32, after.max(0) as u32),
                    }
                }
                // Without surrounding text, assume one byte per unit.
                None => (before.max(0) as u32, after.max(0) as u32),
            };
            res.delete_surrounding_text(b, a);
        }
        ImeEdit::Recompose(t, before) => {
            let b = match &text {
                Some((s, cursor, _)) => s.get(..(*cursor).clamp(0, s.len() as i32) as usize).map_or(t.len() as u32, |pre| bytes_for_units(pre, before, true)),
                None => t.len() as u32,
            };
            res.delete_surrounding_text(b, 0);
            let c = t.len() as i32;
            *preedit = (!t.is_empty()).then(|| (t, c));
        }
    }
    if let Some((t, c)) = preedit.clone() {
        res.preedit_string(Some(t), c, c);
    }
    res.done(serial);
}

// ---- protocol ------------------------------------------------------------

impl GlobalDispatch<zwp_text_input_manager_v3::ZwpTextInputManagerV3, ()> for State {
    fn bind(
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        res: New<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
        _: &(),
        init: &mut DataInit<'_, State>,
    ) {
        init.init(res, ());
    }
}

impl Dispatch<zwp_text_input_manager_v3::ZwpTextInputManagerV3, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        req: zwp_text_input_manager_v3::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zwp_text_input_manager_v3", client, dh, || {
            if let zwp_text_input_manager_v3::Request::GetTextInput { id, .. } = req {
                let res = init.init(id, ());
                st.text_inputs.inputs.insert(
                    res.id(),
                    TextInput { res, focus: None, pending: Pending::default(), current: Pending::default(), commits: 0 },
                );
                // Already focused: enter at once.
                let focus = st.seat.kb_focus.clone();
                if focus.as_ref().and_then(|s| st.surfaces.get(s)).and_then(|s| s.res.client()).is_some_and(|c| c.id() == client.id()) {
                    focus_changed(st, focus.as_ref());
                }
            }
        });
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &zwp_text_input_v3::ZwpTextInputV3,
        req: zwp_text_input_v3::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("zwp_text_input_v3", client, dh, || {
            let Some(ti) = st.text_inputs.inputs.get_mut(&res.id()) else { return };
            match req {
                // Enabling starts from a clean state.
                zwp_text_input_v3::Request::Enable => ti.pending = Pending { enabled: true, ..Pending::default() },
                zwp_text_input_v3::Request::Disable => ti.pending.enabled = false,
                zwp_text_input_v3::Request::SetSurroundingText { text, cursor, anchor } => {
                    if text.len() <= 64 << 10 {
                        ti.pending.text = Some((text, cursor, anchor));
                    }
                }
                zwp_text_input_v3::Request::SetContentType { hint, purpose } => {
                    ti.pending.hint = match hint {
                        WEnum::Value(h) => h.bits(),
                        WEnum::Unknown(u) => u,
                    };
                    ti.pending.purpose = match purpose {
                        WEnum::Value(p) => p.into(),
                        WEnum::Unknown(u) => u,
                    };
                }
                zwp_text_input_v3::Request::SetCursorRectangle { x, y, width, height } => {
                    ti.pending.rect = Some([x, y, width, height]);
                }
                zwp_text_input_v3::Request::Commit => {
                    ti.commits = ti.commits.wrapping_add(1);
                    ti.current = ti.pending.clone();
                    update(st);
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, res: &zwp_text_input_v3::ZwpTextInputV3, _: &()) {
        st.text_inputs.inputs.remove(&res.id());
        update(st);
    }
}
