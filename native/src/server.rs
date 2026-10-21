//! The compositor thread: socket, globals and the event loop.

use std::os::fd::AsRawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;

use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::xdg::decoration::zv1::server::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::viewporter::server::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_manager_v1;
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_data_source, wl_seat, wl_shm, wl_subcompositor,
};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New};

use crate::host::Host;
use crate::state::State;
use crate::{Cmd, Shared};

pub struct Config {
    pub socket_path: String,
}

struct ClientState;

impl ClientData for ClientState {
    fn initialized(&self, id: ClientId) {
        log::info!("client connected: {id:?}");
    }

    fn disconnected(&self, id: ClientId, reason: DisconnectReason) {
        log::info!("client disconnected: {id:?} ({reason:?})");
    }
}

pub fn run(cfg: Config, shared: Arc<Shared>, host: Host) {
    let mut display: Display<State> = match Display::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("cannot create display: {e}");
            return;
        }
    };
    let dh = display.handle();
    dh.create_global::<State, wl_compositor::WlCompositor, ()>(6, ());
    dh.create_global::<State, wl_subcompositor::WlSubcompositor, ()>(1, ());
    dh.create_global::<State, wl_shm::WlShm, ()>(1, ());
    dh.create_global::<State, wl_seat::WlSeat, ()>(7, ());
    // wl_output globals come and go with Android displays (output.rs).
    dh.create_global::<State, zxdg_output_manager_v1::ZxdgOutputManagerV1, ()>(3, ());
    dh.create_global::<State, WpViewporter, ()>(1, ());
    dh.create_global::<State, WpFractionalScaleManagerV1, ()>(1, ());
    dh.create_global::<State, wl_data_device_manager::WlDataDeviceManager, ()>(3, ());
    dh.create_global::<State, XdgWmBase, ()>(6, ());
    dh.create_global::<State, ZxdgDecorationManagerV1, ()>(1, ());

    // `renderer=cpu` in awb.properties keeps the CPU compositor.
    let gpu = if setting(&cfg.socket_path, "renderer").as_deref() == Some("cpu") {
        log::info!("renderer: cpu (awb.properties)");
        None
    } else {
        match crate::gpu::Renderer::new() {
            Ok(r) => {
                log::info!("renderer: vulkan");
                Some(r)
            }
            Err(e) => {
                log::warn!("renderer: cpu ({e})");
                None
            }
        }
    };
    if gpu.is_some() {
        dh.create_global::<State, ZwpLinuxDmabufV1, ()>(4, ());
    }

    let path = PathBuf::from(&cfg.socket_path);
    let _ = std::fs::remove_file(&path);
    let listener = match ListeningSocket::bind_absolute(path.clone()) {
        Ok(l) => l,
        Err(e) => {
            log::error!("cannot bind {}: {e}", path.display());
            return;
        }
    };
    // Clients in the chroot run as other uids.
    unsafe {
        let c = std::ffi::CString::new(cfg.socket_path.clone()).unwrap_or_default();
        libc::chmod(c.as_ptr(), 0o777);
    }
    log::info!("listening on {}", path.display());

    let mut st = State::new(dh.clone(), shared.clone(), host);
    st.gpu = gpu;

    loop {
        let mut fds = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: display.backend().poll_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: shared.wake_fd, events: libc::POLLIN, revents: 0 },
        ];
        // While frames are in flight, check back for their completion.
        let timeout = if st.gpu.as_ref().is_some_and(|g| g.busy()) { 2 } else { -1 };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            log::error!("poll failed: {}", std::io::Error::last_os_error());
            return;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok(Some(stream)) = listener.accept() {
                if let Err(e) = display.handle().insert_client(stream, Arc::new(ClientState)) {
                    log::warn!("cannot add client: {e}");
                }
            }
        }
        if fds[2].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8];
            unsafe { libc::read(shared.wake_fd, buf.as_mut_ptr() as *mut _, 8) };
        }
        let cmds: Vec<Cmd> = shared.cmds.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
        for cmd in cmds {
            if let Err(e) = catch_unwind(AssertUnwindSafe(|| handle_cmd(&mut st, cmd))) {
                log::error!("panic handling command: {}", crate::panic_message(&e));
            }
        }
        // Handlers catch their own panics; this is the last line of defence.
        match catch_unwind(AssertUnwindSafe(|| display.dispatch_clients(&mut st))) {
            Ok(Err(e)) => log::warn!("dispatch error: {e}"),
            Err(e) => log::error!("panic in dispatch: {}", crate::panic_message(&e)),
            _ => {}
        }
        if let Err(e) = catch_unwind(AssertUnwindSafe(|| crate::render::render_dirty(&mut st))) {
            log::error!("panic while rendering: {}", crate::panic_message(&e));
        }
        let _ = display.flush_clients();
    }
}

/// A value from awb.properties, next to the socket.
fn setting(socket_path: &str, name: &str) -> Option<String> {
    let path = std::path::Path::new(socket_path).with_file_name("awb.properties");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

fn handle_cmd(st: &mut State, cmd: Cmd) {
    match cmd {
        Cmd::Redraw(win) => {
            if let Some(w) = st.windows.get_mut(&win) {
                w.dirty = true;
            }
        }
        Cmd::Resize(win, pw, ph) => {
            let Some(w) = st.windows.get_mut(&win) else { return };
            w.px = (pw, ph);
            w.dirty = true;
            let (lw, lh) = st.logical_window_size(win);
            if st.windows[&win].configured.map(|(cw, ch, _)| (cw, ch)) != Some((lw, lh)) {
                st.configure_window(win);
            }
        }
        Cmd::Focus(win, focused) => {
            if focused {
                st.outputs.default = st.windows.get(&win).map(|w| w.display);
            }
            crate::seat::focus_window(st, win, focused)
        }
        Cmd::Display(info) => crate::output::display_changed(st, info),
        Cmd::DisplayRemoved(id) => crate::output::display_removed(st, id),
        Cmd::WindowDisplay(win, display) => {
            let Some(w) = st.windows.get_mut(&win) else { return };
            if w.display != display {
                log::info!("toplevel {win}: now on display {display}");
                w.display = display;
                w.dirty = true;
                st.configure_window(win);
            }
        }
        Cmd::Pointer(ev) => crate::seat::handle_pointer(st, ev),
        Cmd::Key { win, code, down, time } => crate::seat::handle_key(st, win, code, down, time),
        Cmd::Close(win) => {
            if let Some(w) = st.windows.get_mut(&win) {
                log::info!("toplevel {win}: close from Android");
                w.toplevel.close();
                w.px = (0, 0);
            }
        }
        Cmd::Caption(win, caption) => {
            log::info!("toplevel {win}: {caption:?}");
            let Some(w) = st.windows.get_mut(&win) else { return };
            let top_changed = w.caption.top != caption.top;
            w.caption = caption;
            w.caption_light = None;
            w.exclusion.clear();
            w.dirty = true;
            if top_changed && w.configured.is_some() {
                st.configure_window(win);
            }
        }
    }
}

// ---- data device (inert until clipboard support) ----------------------

impl GlobalDispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_data_device_manager::WlDataDeviceManager>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        req: wl_data_device_manager::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        match req {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                init.init(id, ());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_data_source::WlDataSource, _: wl_data_source::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_data_device::WlDataDevice, _: wl_data_device::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

