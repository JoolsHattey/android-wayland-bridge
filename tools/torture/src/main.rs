//! A deliberately misbehaving Wayland client. Each mode does something that
//! has crashed compositors before; the compositor must survive all of them.
//!
//!   awb-torture abort      draw frames, then abort() mid-frame
//!   awb-torture sigbus     shrink the shm pool under an attached buffer
//!   awb-torture popup      open a popup, then exit without destroying it
//!   awb-torture churn N    map and destroy N toplevels as fast as possible

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct S {
    configured: bool,
    frame_done: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S {
    fn event(_: &mut S, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<S>) {}
}

macro_rules! ignore {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for S {
            fn event(_: &mut S, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {}
        }
    )*};
}
ignore!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_surface::WlSurface,
    xdg_toplevel::XdgToplevel, xdg_positioner::XdgPositioner, xdg_popup::XdgPopup);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for S {
    fn event(_: &mut S, wm: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for S {
    fn event(s: &mut S, xs: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {
        if let xdg_surface::Event::Configure { serial } = e {
            xs.ack_configure(serial);
            s.configured = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for S {
    fn event(s: &mut S, _: &wl_callback::WlCallback, _: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {
        s.frame_done = true;
    }
}

struct Shm {
    fd: OwnedFd,
    ptr: *mut u32,
    size: usize,
}

fn shm(size: usize) -> Shm {
    unsafe {
        let fd = libc::memfd_create(c"awb-torture".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0);
        libc::ftruncate(fd, size as i64);
        let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        Shm { fd: OwnedFd::from_raw_fd(fd), ptr: ptr as *mut u32, size }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("abort");
    let conn = Connection::connect_to_env().expect("connect");
    let (globals, mut q) = registry_queue_init::<S>(&conn).expect("registry");
    let qh = q.handle();
    let comp: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm_g: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let mut s = S::default();

    let rounds = if mode == "churn" { args.get(2).and_then(|n| n.parse().ok()).unwrap_or(20) } else { 1 };
    for round in 0..rounds {
        let (w, h) = (480i32, 320i32);
        let surface = comp.create_surface(&qh, ());
        let xs = wm.get_xdg_surface(&surface, &qh, ());
        let tl = xs.get_toplevel(&qh, ());
        tl.set_title(format!("torture: {mode} {round}"));
        surface.commit();
        s.configured = false;
        while !s.configured {
            q.blocking_dispatch(&mut s).unwrap();
        }
        let mem = shm((w * h * 4) as usize);
        let pool = shm_g.create_pool(mem.fd.as_fd(), mem.size as i32, &qh, ());
        let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
        let paint = |t: u32| unsafe {
            for i in 0..(w * h) as usize {
                *mem.ptr.add(i) = 0xff000000 | ((i as u32 + t * 7) & 0xff) << 16 | (t * 3 & 0xff);
            }
        };
        paint(0);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        match mode {
            "sigbus" => {
                q.roundtrip(&mut s).unwrap();
                // Shrink the pool behind the compositor's mapping, then make
                // it read the buffer again.
                unsafe { libc::ftruncate(mem.fd.as_fd().as_raw_fd_compat(), 0) };
                surface.attach(Some(&buf), 0, 0);
                surface.damage_buffer(0, 0, w, h);
                surface.commit();
                match q.roundtrip(&mut s) {
                    Ok(_) => println!("sigbus: no error (compositor did not read?)"),
                    Err(e) => println!("sigbus: disconnected as expected: {e}"),
                }
                return;
            }
            "abort" => {
                for t in 1..120u32 {
                    surface.frame(&qh, ());
                    paint(t);
                    surface.attach(Some(&buf), 0, 0);
                    surface.damage_buffer(0, 0, w, h);
                    surface.commit();
                    if t == 60 {
                        // Die with a commit in flight and the buffer attached.
                        let _ = conn.flush();
                        std::process::abort();
                    }
                    s.frame_done = false;
                    while !s.frame_done {
                        q.blocking_dispatch(&mut s).unwrap();
                    }
                }
            }
            "popup" => {
                q.roundtrip(&mut s).unwrap();
                let ps = comp.create_surface(&qh, ());
                let pxs = wm.get_xdg_surface(&ps, &qh, ());
                let pos = wm.create_positioner(&qh, ());
                pos.set_size(200, 150);
                pos.set_anchor_rect(400, 280, 20, 20);
                pos.set_anchor(xdg_positioner::Anchor::BottomRight);
                pos.set_gravity(xdg_positioner::Gravity::BottomRight);
                pos.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::all());
                let _popup = pxs.get_popup(Some(&xs), &pos, &qh, ());
                ps.commit();
                s.configured = false;
                while !s.configured {
                    q.blocking_dispatch(&mut s).unwrap();
                }
                let pmem = shm(200 * 150 * 4);
                let ppool = shm_g.create_pool(pmem.fd.as_fd(), pmem.size as i32, &qh, ());
                let pbuf = ppool.create_buffer(0, 200, 150, 800, wl_shm::Format::Argb8888, &qh, ());
                ps.attach(Some(&pbuf), 0, 0);
                ps.commit();
                q.roundtrip(&mut s).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(500));
                std::process::exit(0);
            }
            _ => {
                q.roundtrip(&mut s).unwrap();
                tl.destroy();
                xs.destroy();
                surface.destroy();
                buf.destroy();
                pool.destroy();
                q.roundtrip(&mut s).unwrap();
            }
        }
    }
}

trait RawFdCompat {
    fn as_raw_fd_compat(&self) -> i32;
}

impl RawFdCompat for std::os::fd::BorrowedFd<'_> {
    fn as_raw_fd_compat(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.as_raw_fd()
    }
}
