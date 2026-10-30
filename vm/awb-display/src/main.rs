//! dev.awb's guest agent (see dev.awb.VmDisplay and dev.awb.VmAgent).
//!
//! The display: each Linux app runs under `waypipe server` (awb-run), which
//! connects to $XDG_RUNTIME_DIR/awb-display.sock here. The Android app
//! cannot accept connections from the VM, so it keeps a spare vsock
//! connection open to port 7005 instead; each waypipe server is paired with
//! the next spare, and the app hands it to its waypipe client, which talks
//! to the compositor.
//!
//! The rest (agent.rs) gives the VM's apps what the chroot's get from the
//! root side: Android's appearance (portal.rs), the caption CSS, links out
//! to Android's browser and launcher icons (apps.rs), over a control
//! connection the app keeps open to port 7006. `awb-display open <url>`, or
//! the binary run as `awb-open`, hands links to it.

mod agent;
mod apps;
mod portal;
mod setup;

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

const PORT: u32 = 7005;

fn main() -> io::Result<()> {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let first = args.next();
    if argv0.rsplit('/').next() == Some("awb-open") {
        std::process::exit(agent::open(first.into_iter().chain(args)));
    }
    if first.as_deref() == Some("open") {
        std::process::exit(agent::open(args));
    }
    agent::start();
    display()
}

/// $XDG_RUNTIME_DIR, where the sockets for the guest's own processes live.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{}", unsafe { libc::getuid() }).into())
}

fn display() -> io::Result<()> {
    let path = runtime_dir().join("awb-display.sock");
    let _ = std::fs::remove_file(&path);
    let apps = UnixListener::bind(&path)?;

    let vsock = vsock_listen(PORT)?;
    let (tx, spares) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match vsock_accept(&vsock) {
                Ok(spare) => {
                    if tx.send(spare).is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("awb-display: vsock accept: {e}"),
            }
        }
    });

    for app in apps.incoming() {
        let app = match app {
            Ok(app) => app,
            Err(e) => {
                eprintln!("awb-display: accept: {e}");
                continue;
            }
        };
        let Some(spare) = next_spare(&spares) else { break };
        relay(app, spare)?;
    }
    Ok(())
}

/// The next spare the app has not closed (say, on restart).
fn next_spare(spares: &Receiver<OwnedFd>) -> Option<OwnedFd> {
    spares.iter().find(|fd| alive(fd))
}

/// A closed spare reads as EOF; an open one has nothing to read yet.
fn alive(fd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut pfd, 1, 0) } <= 0 {
        return true;
    }
    let mut byte = 0u8;
    let n = unsafe { libc::recv(fd.as_raw_fd(), (&raw mut byte).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n > 0
}

fn relay(app: UnixStream, spare: OwnedFd) -> io::Result<()> {
    let spare = File::from(spare);
    let (app_in, spare_out) = (app.try_clone()?, spare.try_clone()?);
    thread::spawn(move || pump(app_in, spare_out));
    thread::spawn(move || pump(spare, app));
    Ok(())
}

/// Copies until either side closes, then shuts both down so the other pump ends too.
fn pump(mut from: impl Read + AsRawFd, mut to: impl Write + AsRawFd) {
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    for fd in [from.as_raw_fd(), to.as_raw_fd()] {
        unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
    }
}

pub fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    addr.svm_cid = libc::VMADDR_CID_ANY;
    let len = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    if unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) } < 0
        || unsafe { libc::listen(fd.as_raw_fd(), 16) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

pub fn vsock_accept(listener: &OwnedFd) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
