//! The compositor thread: socket, globals and the event loop.

use std::os::fd::AsRawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;

use wayland_protocols::xdg::decoration::zv1::server::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use wayland_protocols::xdg::xdg_output::zv1::server::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_data_source, wl_output, wl_seat, wl_shm, wl_subcompositor,
};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New, Resource};

use crate::host::Host;
use crate::state::{State, guard};
use crate::{Cmd, Shared};

pub struct Config {
    pub socket_path: String,
    pub scale: i32,
    pub display_w: i32,
    pub display_h: i32,
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
    dh.create_global::<State, wl_output::WlOutput, ()>(4, ());
    dh.create_global::<State, zxdg_output_manager_v1::ZxdgOutputManagerV1, ()>(3, ());
    dh.create_global::<State, wl_data_device_manager::WlDataDeviceManager, ()>(3, ());
    dh.create_global::<State, XdgWmBase, ()>(6, ());
    dh.create_global::<State, ZxdgDecorationManagerV1, ()>(1, ());

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
    log::info!("listening on {} (scale {}, display {}x{})", path.display(), cfg.scale, cfg.display_w, cfg.display_h);

    let mut st = State::new(dh.clone(), shared.clone(), host, cfg.scale, (cfg.display_w, cfg.display_h));

    loop {
        let mut fds = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: display.backend().poll_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: shared.wake_fd, events: libc::POLLIN, revents: 0 },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
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
            let (lw, lh) = (pw / st.scale, (ph - w.caption.top) / st.scale);
            if w.configured.map(|(cw, ch, _)| (cw, ch)) != Some((lw, lh)) {
                st.configure_window(win);
            }
        }
        Cmd::Focus(win, focused) => crate::seat::focus_window(st, win, focused),
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

// ---- outputs -----------------------------------------------------------

impl GlobalDispatch<wl_output::WlOutput, ()> for State {
    fn bind(st: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_output::WlOutput>, _: &(), init: &mut DataInit<'_, State>) {
        let o = init.init(res, ());
        let (w, h) = st.display_px;
        o.geometry(0, 0, 302, 189, wl_output::Subpixel::Unknown, "Android".into(), "awb".into(), wl_output::Transform::Normal);
        o.mode(wl_output::Mode::Current | wl_output::Mode::Preferred, w, h, 60_000);
        if o.version() >= 2 {
            o.scale(st.scale);
        }
        if o.version() >= 4 {
            o.name("AWB-1".into());
            o.description("Android display".into());
        }
        if o.version() >= 2 {
            o.done();
        }
        st.outputs.push(o);
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_output::WlOutput, _: wl_output::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}

    fn destroyed(st: &mut State, _: ClientId, o: &wl_output::WlOutput, _: &()) {
        st.outputs.retain(|x| x != o);
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
                let (w, h) = st.display_px;
                xo.logical_position(0, 0);
                xo.logical_size(w / st.scale, h / st.scale);
                if xo.version() >= 2 {
                    xo.name("AWB-1".into());
                    xo.description("Android display".into());
                }
                if xo.version() >= 3 {
                    if output.version() >= 2 {
                        output.done();
                    }
                } else {
                    xo.done();
                }
            }
        });
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn request(_: &mut State, _: &Client, _: &zxdg_output_v1::ZxdgOutputV1, _: zxdg_output_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
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

