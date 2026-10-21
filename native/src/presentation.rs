//! wp_presentation (presentation-time): when a commit reached the screen.
//!
//! Feedback rides with a commit like a frame callback: pending, cached for
//! synchronized subsurfaces, then current on the surface until a frame
//! draws it. A drawn frame owes its surfaces' callbacks and feedback
//! (`Owed`); the renderer pays them once it knows the frame was shown (see
//! `gpu.rs`). A commit that brings a new buffer before the last one was
//! drawn, or a surface destroyed first, gets `discarded`.

use std::os::fd::{AsRawFd, BorrowedFd};

use wayland_protocols::wp::presentation_time::server::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::{Kind, WpPresentationFeedback};
use wayland_server::backend::ObjectId;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::state::{State, guard};

/// Now on CLOCK_MONOTONIC (the clock we advertise), in ns.
pub fn now_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// When a frame was shown: CLOCK_MONOTONIC ns, the refresh period (0: the
/// display's), and how we know.
pub struct Shown {
    pub ns: u64,
    pub refresh_ns: u64,
    pub flags: Kind,
}

/// What a drawn frame owes the clients of its surfaces.
#[derive(Default)]
pub struct Owed {
    pub callbacks: Vec<WlCallback>,
    pub feedback: Vec<WpPresentationFeedback>,
}

impl Owed {
    /// Takes the frame callbacks and feedback of the surfaces in `list`.
    pub fn take(st: &mut State, list: &[(ObjectId, i32, i32)]) -> Owed {
        let mut owed = Owed::default();
        for (sid, _, _) in list {
            if let Some(s) = st.surfaces.get_mut(sid) {
                owed.callbacks.append(&mut s.frame_cbs);
                owed.feedback.append(&mut s.feedback);
            }
        }
        owed
    }

    /// Sends the frame callbacks (the feedback stays).
    pub fn done(&mut self, ms: u32) {
        for cb in self.callbacks.drain(..) {
            cb.done(ms);
        }
    }
}

/// The content of these commits was never shown.
pub fn discard(feedback: impl IntoIterator<Item = WpPresentationFeedback>) {
    for f in feedback {
        if f.is_alive() {
            f.discarded();
        }
    }
}

/// Refresh period of the display window `win` is on, in ns (0: unknown).
pub fn refresh_ns(st: &State, win: u32) -> u64 {
    let display = st.windows.get(&win).map_or_else(|| st.outputs.default_display(), |w| w.display);
    match st.outputs.map.get(&display).map(|o| o.info.refresh_mhz) {
        Some(mhz) if mhz > 0 => 1_000_000_000_000 / mhz as u64,
        _ => 0,
    }
}

/// The content of these commits, in window `win`, was shown.
pub fn presented(st: &State, win: u32, feedback: Vec<WpPresentationFeedback>, shown: Shown) {
    if feedback.is_empty() {
        return;
    }
    let refresh = if shown.refresh_ns > 0 { shown.refresh_ns } else { refresh_ns(st, win) };
    let display = st.windows.get(&win).map(|w| w.display);
    let outputs = display.and_then(|d| st.outputs.map.get(&d)).map(|o| o.resources.as_slice()).unwrap_or_default();
    let secs = shown.ns / 1_000_000_000;
    for f in feedback.into_iter().filter(|f| f.is_alive()) {
        for o in outputs.iter().filter(|o| o.id().same_client_as(&f.id())) {
            f.sync_output(o);
        }
        f.presented((secs >> 32) as u32, secs as u32, (shown.ns % 1_000_000_000) as u32, refresh.min(u32::MAX as u64) as u32, 0, 0, shown.flags);
    }
}

/// A sync_file's state: when it signalled (CLOCK_MONOTONIC ns), not yet,
/// or unknown (error).
pub enum Fence {
    Signalled(u64),
    Pending,
    Failed,
}

#[repr(C)]
struct SyncFenceInfo {
    obj_name: [u8; 32],
    driver_name: [u8; 32],
    status: i32,
    flags: u32,
    timestamp_ns: u64,
}

#[repr(C)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

/// `SYNC_IOC_FILE_INFO`: _IOWR('>', 4, struct sync_file_info).
const SYNC_IOC_FILE_INFO: u32 = 0xc000_0000 | ((size_of::<SyncFileInfo>() as u32) << 16) | (b'>' as u32) << 8 | 4;

/// Reads a sync_file's signal time: the latest of its fences', which is
/// when the whole file signalled.
pub fn fence_time(fd: BorrowedFd) -> Fence {
    const N: usize = 8;
    let mut fences: [SyncFenceInfo; N] = unsafe { std::mem::zeroed() };
    let mut info = SyncFileInfo {
        name: [0; 32],
        status: 0,
        flags: 0,
        num_fences: N as u32,
        pad: 0,
        sync_fence_info: fences.as_mut_ptr() as u64,
    };
    if unsafe { libc::ioctl(fd.as_raw_fd(), SYNC_IOC_FILE_INFO as _, &mut info) } != 0 {
        return Fence::Failed;
    }
    match info.status {
        1 => {
            let n = (info.num_fences as usize).min(N);
            match fences[..n].iter().map(|f| f.timestamp_ns).max() {
                Some(ns) if ns > 0 => Fence::Signalled(ns),
                _ => Fence::Failed,
            }
        }
        0 => Fence::Pending,
        _ => Fence::Failed,
    }
}

impl GlobalDispatch<WpPresentation, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<WpPresentation>, _: &(), init: &mut DataInit<'_, State>) {
        let p = init.init(res, ());
        p.clock_id(libc::CLOCK_MONOTONIC as u32);
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &WpPresentation,
        req: wp_presentation::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wp_presentation", client, dh, || {
            if let wp_presentation::Request::Feedback { surface, callback } = req {
                let f = init.init(callback, ());
                match st.surfaces.get_mut(&surface.id()) {
                    Some(s) => s.pending.feedback.push(f),
                    None => f.discarded(),
                }
            }
        });
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &WpPresentationFeedback,
        _: <WpPresentationFeedback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
    }
}
