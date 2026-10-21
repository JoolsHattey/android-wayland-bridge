//! wl_data_device: the clipboard, kept in step with Android's, and drag and
//! drop. A drag runs as an Android drag (`View.startDragAndDrop`), so it can
//! cross into other windows (each its own Android window) and other apps;
//! Android's drag events then drive the Wayland side.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::protocol::wl_data_device_manager::DndAction;
use wayland_server::protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_surface};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::state::{State, guard};
use crate::surface::Role;
use crate::{Cmd, DragEvent};

/// Text types, best first. Android text is offered as all of them.
const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Larger selections are not copied to Android.
const MAX_TEXT: usize = 8 << 20;

#[derive(Default)]
pub struct Data {
    pub devices: Vec<wl_data_device::WlDataDevice>,
    /// Mime types and drag actions of each data source.
    sources: HashMap<ObjectId, SourceInfo>,
    offers: HashMap<ObjectId, Offer>,
    selection: Option<Content>,
    /// Bumped with every selection, so a late read can tell it is stale.
    selection_gen: u64,
    /// Text last copied to Android from a client's selection: Android hands
    /// it back as a clipboard change, which must not replace the source.
    exported: Option<String>,
    drag: Option<Drag>,
    next_read: u64,
    /// Source reads in flight (see `read_text`).
    reads: HashMap<u64, ReadFor>,
}

impl Data {
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

#[derive(Default)]
struct SourceInfo {
    mimes: Vec<String>,
    actions: u32,
}

/// What an offer (or the selection) hands out.
#[derive(Clone)]
enum Content {
    Client(wl_data_source::WlDataSource),
    /// From Android: the clipboard, or a drop (None until the drop delivers it).
    Text(Option<Arc<str>>, Vec<String>),
}

struct Offer {
    content: Content,
    dnd: bool,
    accepted: Option<String>,
    action: u32,
}

struct Drag {
    /// None for a client's drag without a data source (within that client).
    content: Option<Content>,
    client: Option<ClientId>,
    icon: Option<ObjectId>,
    /// Surface under the drag, its window and the offer made to it.
    focus: Option<(ObjectId, u32, Option<wl_data_offer::WlDataOffer>)>,
    dropped: bool,
    /// `receive` calls on an Android drop that has not delivered its text yet.
    waiting: Vec<OwnedFd>,
}

enum ReadFor {
    Clipboard(u64),
    Drag(u32),
}

fn is_text(mime: &str) -> bool {
    TEXT_MIMES.contains(&mime)
}

fn text_mime(mimes: &[String]) -> Option<String> {
    TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)).map(|t| t.to_string())
}

/// Writes `text` to a client's fd off the compositor thread.
fn write_text(fd: OwnedFd, text: Arc<str>) {
    let _ = std::thread::Builder::new().name("awb-data".into()).spawn(move || {
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let _ = std::fs::File::from(fd).write_all(text.as_bytes());
    });
}

/// Asks a client's source for its text and reads it off the compositor
/// thread; the result comes back as `Cmd::SourceRead`.
fn read_text(st: &mut State, source: &wl_data_source::WlDataSource, mime: &str, purpose: ReadFor) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    source.send(mime.into(), w.as_fd());
    drop(w);
    st.data.next_read += 1;
    let id = st.data.next_read;
    st.data.reads.insert(id, purpose);
    let shared = st.shared.clone();
    let spawned = std::thread::Builder::new().name("awb-data".into()).spawn(move || {
        let mut file = std::fs::File::from(r);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        let ok = loop {
            // A client that never writes or closes must not hold the thread.
            let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 3000) } <= 0 {
                break false;
            }
            match file.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) if buf.len() + n <= MAX_TEXT => buf.extend_from_slice(&chunk[..n]),
                Ok(_) => break false,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break false,
            }
        };
        let text = ok.then(|| String::from_utf8_lossy(&buf).into_owned());
        shared.push(Cmd::SourceRead { id, text });
    });
    if spawned.is_err() {
        st.data.reads.remove(&id);
    }
}

pub fn source_read(st: &mut State, id: u64, text: Option<String>) {
    match st.data.reads.remove(&id) {
        Some(ReadFor::Clipboard(generation)) => {
            let Some(text) = text.filter(|t| !t.is_empty()) else { return };
            if generation == st.data.selection_gen && st.data.exported.as_ref() != Some(&text) {
                st.host.clipboard(&text);
                st.data.exported = Some(text);
            }
        }
        Some(ReadFor::Drag(win)) => start_android_drag(st, win, text),
        None => {}
    }
}

fn devices_of(st: &State, client: &ClientId) -> Vec<wl_data_device::WlDataDevice> {
    st.data.devices.iter().filter(|d| d.client().is_some_and(|c| c.id() == *client)).cloned().collect()
}

fn mimes_of(st: &State, content: &Content) -> Vec<String> {
    match content {
        Content::Client(s) => st.data.sources.get(&s.id()).map(|i| i.mimes.clone()).unwrap_or_default(),
        Content::Text(_, mimes) => mimes.clone(),
    }
}

/// Creates an offer of `content` on `device` and announces its types.
fn make_offer(st: &mut State, device: &wl_data_device::WlDataDevice, content: &Content, dnd: bool) -> Option<wl_data_offer::WlDataOffer> {
    let client = device.client()?;
    let offer = client.create_resource::<wl_data_offer::WlDataOffer, (), State>(&st.dh, device.version(), ()).ok()?;
    device.data_offer(&offer);
    for m in mimes_of(st, content) {
        offer.offer(m);
    }
    if dnd && offer.version() >= 3 {
        let actions = match content {
            Content::Client(s) => st.data.sources.get(&s.id()).map_or(0, |i| i.actions),
            Content::Text(..) => DndAction::Copy.bits(),
        };
        offer.source_actions(DndAction::from_bits_truncate(actions));
    }
    st.data.offers.insert(offer.id(), Offer { content: content.clone(), dnd, accepted: None, action: 0 });
    Some(offer)
}

/// Sends the current selection to `client` (the keyboard focus).
pub fn offer_selection(st: &mut State, client: &ClientId) {
    for device in devices_of(st, client) {
        match st.data.selection.clone() {
            Some(content) => {
                let offer = make_offer(st, &device, &content, false);
                device.selection(offer.as_ref());
            }
            None => device.selection(None),
        }
    }
}

fn focused_client(st: &State) -> Option<ClientId> {
    let sid = st.seat.kb_focus.as_ref()?;
    st.surfaces.get(sid)?.res.client().map(|c| c.id())
}

fn set_selection(st: &mut State, content: Option<Content>) {
    if let Some(Content::Client(old)) = &st.data.selection {
        if content.as_ref().is_none_or(|c| !matches!(c, Content::Client(s) if s == old)) && old.is_alive() {
            old.cancelled();
        }
    }
    st.data.selection = content;
    st.data.selection_gen += 1;
    if let Some(client) = focused_client(st) {
        offer_selection(st, &client);
    }
}

/// Android's clipboard changed (read by the focused window's activity).
pub fn android_clipboard(st: &mut State, text: String) {
    if st.data.exported.as_ref() == Some(&text) {
        return;
    }
    let mimes = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
    st.data.exported = None;
    set_selection(st, Some(Content::Text(Some(text.into()), mimes)));
}

fn client_selection(st: &mut State, source: Option<wl_data_source::WlDataSource>) {
    let Some(source) = source else {
        set_selection(st, None);
        return;
    };
    set_selection(st, Some(Content::Client(source.clone())));
    let mimes = st.data.sources.get(&source.id()).map(|i| i.mimes.clone()).unwrap_or_default();
    if let Some(mime) = text_mime(&mimes) {
        let generation = st.data.selection_gen;
        read_text(st, &source, &mime, ReadFor::Clipboard(generation));
    }
}

// ---- drag and drop -------------------------------------------------------

fn start_client_drag(
    st: &mut State,
    client: &Client,
    source: Option<wl_data_source::WlDataSource>,
    origin: &wl_surface::WlSurface,
    icon: Option<wl_surface::WlSurface>,
) {
    let win = st.window_of(&origin.id());
    // Only while a button is held on that window, as the protocol's serial implies.
    let pressed = st.seat.buttons != 0 && st.seat.ptr_focus.as_ref().is_some_and(|(_, w)| Some(*w) == win);
    let Some(win) = win.filter(|_| pressed && st.data.drag.is_none()) else {
        if let Some(s) = source {
            s.cancelled();
        }
        return;
    };
    if let Some(icon) = &icon {
        if let Some(s) = st.surfaces.get_mut(&icon.id()) {
            if matches!(s.role, Role::None | Role::DndIcon) {
                s.role = Role::DndIcon;
            }
        }
    }
    log::info!("win {win}: drag started");
    st.data.drag = Some(Drag {
        content: source.clone().map(Content::Client),
        client: Some(client.id()),
        icon: icon.map(|i| i.id()),
        focus: None,
        dropped: false,
        waiting: Vec::new(),
    });
    // The drag owns the pointer now.
    crate::seat::pointer_grabbed(st);
    // Android apps get the text, so read it before the Android drag starts.
    let mime = source.as_ref().and_then(|s| text_mime(&st.data.sources.get(&s.id()).map(|i| i.mimes.clone()).unwrap_or_default()));
    match (source, mime) {
        (Some(s), Some(mime)) => read_text(st, &s, &mime, ReadFor::Drag(win)),
        _ => start_android_drag(st, win, None),
    }
}

fn start_android_drag(st: &mut State, win: u32, text: Option<String>) {
    let Some(drag) = &st.data.drag else { return };
    let icon = drag.icon.clone().and_then(|sid| crate::seat::surface_image(st, &sid, win));
    st.host.start_drag(win, text.as_deref(), icon);
}

fn leave_focus(st: &mut State) {
    let Some(drag) = &mut st.data.drag else { return };
    let Some((sid, _, _)) = drag.focus.take() else { return };
    if let Some(client) = st.surfaces.get(&sid).and_then(|s| s.res.client()) {
        for d in devices_of(st, &client.id()) {
            d.leave();
        }
    }
}

/// Moves the drag over (`win`, logical x, y), entering whatever is there.
fn drag_motion(st: &mut State, win: u32, x: f64, y: f64, time: u32) {
    let target = st.windows.get(&win).and_then(|w| st.surface_at(w, x, y));
    let Some(drag) = &st.data.drag else { return };
    let current = drag.focus.as_ref().map(|(s, _, _)| s.clone());
    if target.as_ref().map(|(s, _, _)| s) != current.as_ref() {
        leave_focus(st);
        let Some((sid, lx, ly)) = target.clone() else { return };
        let Some(surface) = st.surfaces.get(&sid).map(|s| s.res.clone()) else { return };
        let Some(client) = surface.client().map(|c| c.id()) else { return };
        let drag = st.data.drag.as_ref().unwrap();
        // A drag without a data source stays within its client.
        if drag.content.is_none() && drag.client.as_ref() != Some(&client) {
            return;
        }
        let content = drag.content.clone();
        let serial = st.next_serial();
        let mut offer = None;
        for d in devices_of(st, &client) {
            let o = content.as_ref().and_then(|c| make_offer(st, &d, c, true));
            d.enter(serial, &surface, lx, ly, o.as_ref());
            offer = offer.or(o);
        }
        if let Some(drag) = &mut st.data.drag {
            drag.focus = Some((sid, win, offer));
        }
        return;
    }
    let Some((sid, lx, ly)) = target else { return };
    if let Some(client) = st.surfaces.get(&sid).and_then(|s| s.res.client()) {
        for d in devices_of(st, &client.id()) {
            d.motion(time, lx, ly);
        }
    }
}

fn drop_here(st: &mut State) -> bool {
    let Some(drag) = &mut st.data.drag else { return false };
    let Some((sid, _, offer)) = drag.focus.clone() else { return false };
    // Version 3 offers also need an action both sides agreed on.
    let accepted = offer.as_ref().is_some_and(|o| {
        st.data.offers.get(&o.id()).is_some_and(|info| info.accepted.is_some() && (o.version() < 3 || info.action != 0))
    });
    let source_only = drag.content.is_none();
    if !accepted && !source_only {
        leave_focus(st);
        return false;
    }
    if let Some(client) = st.surfaces.get(&sid).and_then(|s| s.res.client()) {
        for d in devices_of(st, &client.id()) {
            d.drop();
        }
    }
    let drag = st.data.drag.as_mut().unwrap();
    drag.dropped = true;
    if let Some(Content::Client(s)) = &drag.content {
        if s.version() >= 3 {
            s.dnd_drop_performed();
        }
    }
    true
}

fn end_drag(st: &mut State) {
    let Some(mut drag) = st.data.drag.take() else { return };
    if !drag.dropped {
        if let Some((sid, _, _)) = drag.focus.take() {
            if let Some(client) = st.surfaces.get(&sid).and_then(|s| s.res.client()) {
                for d in devices_of(st, &client.id()) {
                    d.leave();
                }
            }
        }
        if let Some(Content::Client(s)) = &drag.content {
            if s.is_alive() {
                s.cancelled();
            }
        }
    }
    // Receives on a drop that never delivered get EOF.
    drag.waiting.clear();
    if let Some(icon) = drag.icon {
        if let Some(s) = st.surfaces.get_mut(&icon) {
            s.role = Role::None;
        }
    }
    log::info!("drag ended ({})", if drag.dropped { "dropped" } else { "cancelled" });
}

/// An Android drag event on window `ev.win` (see `DragEvent`).
pub fn android_drag(st: &mut State, ev: DragEvent) {
    // A drag from elsewhere (another app, or Android's text) becomes a drag
    // of its text; ours keeps its Wayland source.
    if !ev.local && st.data.drag.is_none() && matches!(ev.action, DragEvent::LOCATION | DragEvent::DROP) {
        if !ev.mimes.iter().any(|m| m.starts_with("text/")) {
            return;
        }
        let mimes = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
        st.data.drag = Some(Drag {
            content: Some(Content::Text(None, mimes)),
            client: None,
            icon: None,
            focus: None,
            dropped: false,
            waiting: Vec::new(),
        });
    }
    if st.data.drag.is_none() {
        return;
    }
    let (x, y) = crate::seat::window_point(st, ev.win, ev.x, ev.y);
    let time = st.now_ms();
    match ev.action {
        DragEvent::LOCATION => drag_motion(st, ev.win, x, y, time),
        DragEvent::EXITED => {
            if st.data.drag.as_ref().and_then(|d| d.focus.as_ref()).is_some_and(|(_, w, _)| *w == ev.win) {
                leave_focus(st);
            }
        }
        DragEvent::DROP => {
            drag_motion(st, ev.win, x, y, time);
            if let (Some(text), Some(drag)) = (ev.text, &mut st.data.drag) {
                if let Some(Content::Text(t, _)) = &mut drag.content {
                    let text: Arc<str> = text.into();
                    *t = Some(text.clone());
                    for fd in drag.waiting.drain(..) {
                        write_text(fd, text.clone());
                    }
                    // Offers made before the text arrived hand it out too.
                    for o in st.data.offers.values_mut() {
                        if let Content::Text(ot @ None, _) = &mut o.content {
                            if o.dnd {
                                *ot = Some(text.clone());
                            }
                        }
                    }
                }
            }
            drop_here(st);
            // A drop of foreign text is complete once handed over; ours waits
            // for Android's end event (it may land in another app instead).
            if !ev.local {
                end_drag(st);
            }
        }
        DragEvent::ENDED => {
            // Ours, dropped into an Android app: done, as far as the source knows.
            if ev.local && ev.result {
                if let Some(drag) = &mut st.data.drag {
                    if !drag.dropped {
                        leave_focus(st);
                        let drag = st.data.drag.as_mut().unwrap();
                        drag.dropped = true;
                        if let Some(Content::Client(s)) = &drag.content {
                            if s.is_alive() && s.version() >= 3 {
                                s.dnd_drop_performed();
                                s.dnd_finished();
                            }
                        }
                    }
                }
            }
            end_drag(st)
        }
        _ => {}
    }
}

/// Picks the drag action from what the source offers and the target takes.
fn choose_action(source: u32, target: u32, preferred: u32) -> u32 {
    let both = source & target;
    if both & preferred != 0 {
        return preferred;
    }
    [DndAction::Copy, DndAction::Move, DndAction::Ask].into_iter().map(|a| a.bits()).find(|a| both & a != 0).unwrap_or(0)
}

fn offer_request(st: &mut State, offer: &wl_data_offer::WlDataOffer, req: wl_data_offer::Request) {
    let id = offer.id();
    match req {
        wl_data_offer::Request::Accept { mime_type, .. } => {
            let Some(o) = st.data.offers.get_mut(&id) else { return };
            o.accepted = mime_type.clone();
            if let Content::Client(s) = &o.content {
                if o.dnd && s.is_alive() {
                    s.target(mime_type);
                }
            }
        }
        wl_data_offer::Request::Receive { mime_type, fd } => {
            let Some(o) = st.data.offers.get(&id) else { return };
            match &o.content {
                Content::Client(s) => {
                    if s.is_alive() {
                        s.send(mime_type, fd.as_fd());
                    }
                }
                Content::Text(Some(text), _) => {
                    if is_text(&mime_type) {
                        write_text(fd, text.clone());
                    }
                }
                // An Android drop's text arrives with the drop.
                Content::Text(None, _) => {
                    if let Some(d) = &mut st.data.drag {
                        d.waiting.push(fd);
                    }
                }
            }
        }
        wl_data_offer::Request::Finish => {
            let Some(o) = st.data.offers.get(&id) else { return };
            if let Content::Client(s) = &o.content {
                if o.dnd && s.is_alive() && s.version() >= 3 {
                    s.dnd_finished();
                }
            }
        }
        wl_data_offer::Request::SetActions { dnd_actions, preferred_action } => {
            let bits = |a: WEnum<DndAction>| match a {
                WEnum::Value(v) => v.bits(),
                WEnum::Unknown(_) => 0,
            };
            let Some(o) = st.data.offers.get(&id) else { return };
            let source_actions = match &o.content {
                Content::Client(s) => st.data.sources.get(&s.id()).map_or(0, |i| i.actions),
                Content::Text(..) => DndAction::Copy.bits(),
            };
            let action = choose_action(source_actions, bits(dnd_actions), bits(preferred_action));
            let o = st.data.offers.get_mut(&id).unwrap();
            o.action = action;
            offer.action(DndAction::from_bits_truncate(action));
            if let Content::Client(s) = &o.content {
                if s.is_alive() && s.version() >= 3 {
                    s.action(DndAction::from_bits_truncate(action));
                }
            }
        }
        _ => {}
    }
}

// ---- protocol ------------------------------------------------------------

impl GlobalDispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_data_device_manager::WlDataDeviceManager>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        req: wl_data_device_manager::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_data_device_manager", client, dh, || match req {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                let s = init.init(id, ());
                st.data.sources.insert(s.id(), SourceInfo::default());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let d = init.init(id, ());
                st.data.devices.push(d);
                // A client focused before it asked still gets the selection.
                if focused_client(st).as_ref() == Some(&client.id()) {
                    offer_selection(st, &client.id());
                }
            }
            _ => {}
        });
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        s: &wl_data_source::WlDataSource,
        req: wl_data_source::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wl_data_source", client, dh, || {
            let Some(info) = st.data.sources.get_mut(&s.id()) else { return };
            match req {
                wl_data_source::Request::Offer { mime_type } => {
                    if info.mimes.len() < 256 {
                        info.mimes.push(mime_type);
                    }
                }
                wl_data_source::Request::SetActions { dnd_actions } => {
                    if let WEnum::Value(a) = dnd_actions {
                        info.actions = a.bits();
                    }
                }
                _ => {}
            }
        });
    }

    fn destroyed(st: &mut State, _: ClientId, s: &wl_data_source::WlDataSource, _: &()) {
        st.data.sources.remove(&s.id());
        if matches!(&st.data.selection, Some(Content::Client(c)) if c == s) {
            st.data.selection = None;
            st.data.selection_gen += 1;
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &wl_data_device::WlDataDevice,
        req: wl_data_device::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wl_data_device", client, dh, || match req {
            wl_data_device::Request::SetSelection { source, .. } => client_selection(st, source),
            wl_data_device::Request::StartDrag { source, origin, icon, .. } => start_client_drag(st, client, source, &origin, icon),
            _ => {}
        });
    }

    fn destroyed(st: &mut State, _: ClientId, d: &wl_data_device::WlDataDevice, _: &()) {
        st.data.devices.retain(|x| x != d);
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        offer: &wl_data_offer::WlDataOffer,
        req: wl_data_offer::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        guard("wl_data_offer", client, dh, || offer_request(st, offer, req));
    }

    fn destroyed(st: &mut State, _: ClientId, offer: &wl_data_offer::WlDataOffer, _: &()) {
        st.data.offers.remove(&offer.id());
    }
}
