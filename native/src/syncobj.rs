//! wp_linux_drm_syncobj_v1: explicit sync for dmabuf surfaces. A client
//! commits each buffer with an acquire point (signalled when its rendering
//! is done) and a release point (which we signal once the GPU no longer
//! reads the buffer), both on DRM syncobj timelines it shares with us. We
//! check and signal them with the render node's syncobj ioctls; the frame
//! is only drawn once the acquire point has signalled, as with implicit
//! sync (see `Renderer::render_window`). A pending acquire point wakes the
//! event loop through an eventfd. A release point due while the GPU still
//! reads the buffer gets the GPU's fence for that frame, so it signals when
//! the GPU is done, without the compositor.

use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use wayland_protocols::wp::linux_drm_syncobj::v1::server::{
    wp_linux_drm_syncobj_manager_v1::{self, WpLinuxDrmSyncobjManagerV1},
    wp_linux_drm_syncobj_surface_v1::{self, WpLinuxDrmSyncobjSurfaceV1},
    wp_linux_drm_syncobj_timeline_v1::{self, WpLinuxDrmSyncobjTimelineV1},
};
use wayland_server::backend::ObjectId;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::state::{State, guard};

// ---- DRM syncobj ioctls ------------------------------------------------

#[repr(C)]
struct GetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
struct Handle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
struct Destroy {
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct Create {
    handle: u32,
    flags: u32,
}

#[repr(C)]
struct Transfer {
    src_handle: u32,
    dst_handle: u32,
    src_point: u64,
    dst_point: u64,
    flags: u32,
    pad: u32,
}

#[repr(C)]
struct EventFd {
    handle: u32,
    flags: u32,
    point: u64,
    fd: i32,
    pad: u32,
}

#[repr(C)]
struct TimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

const fn iowr<T>(nr: u32) -> libc::c_ulong {
    (3 << 30 | (std::mem::size_of::<T>() as u32) << 16 | (b'd' as u32) << 8 | nr) as libc::c_ulong
}

const DRM_IOCTL_GET_CAP: libc::c_ulong = iowr::<GetCap>(0x0c);
const DRM_IOCTL_SYNCOBJ_CREATE: libc::c_ulong = iowr::<Create>(0xbf);
const DRM_IOCTL_SYNCOBJ_DESTROY: libc::c_ulong = iowr::<Destroy>(0xc0);
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: libc::c_ulong = iowr::<Handle>(0xc2);
const DRM_IOCTL_SYNCOBJ_QUERY: libc::c_ulong = iowr::<TimelineArray>(0xcb);
const DRM_IOCTL_SYNCOBJ_TRANSFER: libc::c_ulong = iowr::<Transfer>(0xcc);
const DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL: libc::c_ulong = iowr::<TimelineArray>(0xcd);
const DRM_IOCTL_SYNCOBJ_EVENTFD: libc::c_ulong = iowr::<EventFd>(0xcf);
const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

/// The GPU's render node, opened for syncobj ioctls.
pub struct Drm {
    fd: OwnedFd,
}

impl Drm {
    /// Opens the render node with device number `dev` if it supports
    /// timeline syncobjs.
    pub fn open(dev: u64) -> Option<Arc<Drm>> {
        let dir = std::fs::read_dir("/dev/dri").ok()?;
        for entry in dir.flatten() {
            let path = entry.path();
            let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else { continue };
            unsafe {
                let mut st: libc::stat = std::mem::zeroed();
                if libc::stat(c.as_ptr(), &mut st) != 0 || st.st_rdev as u64 != dev {
                    continue;
                }
                let fd = libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                if fd < 0 {
                    log::warn!("syncobj: cannot open {}", path.display());
                    return None;
                }
                let drm = Drm { fd: OwnedFd::from_raw_fd(fd) };
                let mut cap = GetCap { capability: DRM_CAP_SYNCOBJ_TIMELINE, value: 0 };
                if drm.ioctl(DRM_IOCTL_GET_CAP, &mut cap) && cap.value != 0 {
                    return Some(Arc::new(drm));
                }
                log::info!("syncobj: {} has no timeline syncobjs", path.display());
                return None;
            }
        }
        None
    }

    fn ioctl<T>(&self, req: libc::c_ulong, arg: &mut T) -> bool {
        unsafe { libc::ioctl(self.fd.as_raw_fd(), req as _, arg as *mut T) == 0 }
    }
}

/// A client's syncobj timeline, imported into our DRM file.
pub struct Timeline {
    drm: Arc<Drm>,
    handle: u32,
}

impl Timeline {
    fn import(drm: &Arc<Drm>, fd: &OwnedFd) -> Option<Timeline> {
        let mut h = Handle { handle: 0, flags: 0, fd: fd.as_raw_fd(), pad: 0 };
        drm.ioctl(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut h).then(|| Timeline { drm: drm.clone(), handle: h.handle })
    }

    /// Whether `point` has signalled.
    pub fn signalled(&self, point: u64) -> bool {
        let (handle, mut value) = (self.handle, 0u64);
        let mut q = TimelineArray {
            handles: &handle as *const u32 as u64,
            points: &mut value as *mut u64 as u64,
            count_handles: 1,
            flags: 0,
        };
        self.drm.ioctl(DRM_IOCTL_SYNCOBJ_QUERY, &mut q) && value >= point
    }

    /// Makes `point` signal with `fence` (a sync_file): imported into a
    /// temporary binary syncobj, then transferred to the point.
    pub fn signal_after(&self, point: u64, fence: BorrowedFd) -> bool {
        let mut c = Create { handle: 0, flags: 0 };
        if !self.drm.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut c) {
            return false;
        }
        let mut h = Handle { handle: c.handle, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd: fence.as_raw_fd(), pad: 0 };
        let mut t = Transfer { src_handle: c.handle, dst_handle: self.handle, src_point: 0, dst_point: point, flags: 0, pad: 0 };
        let ok = self.drm.ioctl(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut h) && self.drm.ioctl(DRM_IOCTL_SYNCOBJ_TRANSFER, &mut t);
        if !ok {
            log::warn!("syncobj: cannot hand point {point} to a fence: {}", std::io::Error::last_os_error());
        }
        let mut d = Destroy { handle: c.handle, pad: 0 };
        self.drm.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut d);
        ok
    }

    /// An eventfd that becomes readable when `point` signals.
    pub fn eventfd(&self, point: u64) -> Option<OwnedFd> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut e = EventFd { handle: self.handle, flags: 0, point, fd: fd.as_raw_fd(), pad: 0 };
        self.drm.ioctl(DRM_IOCTL_SYNCOBJ_EVENTFD, &mut e).then_some(fd)
    }

    pub fn signal(&self, point: u64) {
        let handle = self.handle;
        let mut s = TimelineArray {
            handles: &handle as *const u32 as u64,
            points: &point as *const u64 as u64,
            count_handles: 1,
            flags: 0,
        };
        if !self.drm.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &mut s) {
            log::warn!("syncobj: cannot signal point {point}: {}", std::io::Error::last_os_error());
        }
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        let mut d = Destroy { handle: self.handle, pad: 0 };
        self.drm.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut d);
    }
}

/// A timeline point.
#[derive(Clone)]
pub struct Point(pub Arc<Timeline>, pub u64);

impl Point {
    pub fn signalled(&self) -> bool {
        self.0.signalled(self.1)
    }

    pub fn signal(&self) {
        self.0.signal(self.1)
    }

    pub fn eventfd(&self) -> Option<OwnedFd> {
        self.0.eventfd(self.1)
    }

    /// Identifies the point (its timeline and value).
    pub fn key(&self) -> (usize, u64) {
        (Arc::as_ptr(&self.0) as usize, self.1)
    }
}

/// Signals the release point of a commit when dropped, so every path that
/// lets go of a commit's buffer (replaced, surface gone, cached state
/// superseded) gives it back to the client.
pub struct Release(pub Point);

impl Release {
    /// Hands the point to the GPU: it signals with `fence`. Returns the
    /// release back (still to be signalled) if that fails.
    pub fn signal_after(self, fence: BorrowedFd) -> Option<Release> {
        if !self.0.0.signal_after(self.0.1, fence) {
            return Some(self);
        }
        // Done: drop the point without signalling it again.
        let this = ManuallyDrop::new(self);
        drop(unsafe { std::ptr::read(&this.0) });
        None
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        self.0.signal();
    }
}

/// A commit's points, attached to its texture.
pub struct CommitSync {
    pub acquire: Point,
    pub release: Release,
}

// ---- protocol ----------------------------------------------------------

impl GlobalDispatch<WpLinuxDrmSyncobjManagerV1, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<WpLinuxDrmSyncobjManagerV1>, _: &(), init: &mut DataInit<'_, State>) {
        init.init(res, ());
    }
}

impl Dispatch<WpLinuxDrmSyncobjManagerV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &WpLinuxDrmSyncobjManagerV1,
        req: wp_linux_drm_syncobj_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        use wp_linux_drm_syncobj_manager_v1::{Error, Request};
        guard("wp_linux_drm_syncobj_manager_v1", client, dh, || match req {
            Request::GetSurface { id, surface } => {
                let sid = surface.id();
                let obj = init.init(id, sid.clone());
                let Some(s) = st.surfaces.get_mut(&sid) else { return };
                if s.syncobj.as_ref().is_some_and(|o| o.is_alive()) {
                    res.post_error(Error::SurfaceExists, "surface already has a syncobj surface");
                    return;
                }
                log::debug!("syncobj: explicit sync for {sid:?}");
                s.syncobj = Some(obj);
            }
            Request::ImportTimeline { id, fd } => {
                let timeline = st.drm.as_ref().and_then(|d| Timeline::import(d, &fd)).map(Arc::new);
                let failed = timeline.is_none();
                init.init(id, timeline);
                if failed {
                    res.post_error(Error::InvalidTimeline, "cannot import the syncobj");
                }
            }
            _ => {}
        });
    }
}

impl Dispatch<WpLinuxDrmSyncobjTimelineV1, Option<Arc<Timeline>>> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &WpLinuxDrmSyncobjTimelineV1,
        _: wp_linux_drm_syncobj_timeline_v1::Request,
        _: &Option<Arc<Timeline>>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        // Points already set keep the timeline alive through their Arc.
    }
}

impl Dispatch<WpLinuxDrmSyncobjSurfaceV1, ObjectId> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &WpLinuxDrmSyncobjSurfaceV1,
        req: wp_linux_drm_syncobj_surface_v1::Request,
        sid: &ObjectId,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        use wp_linux_drm_syncobj_surface_v1::{Error, Request};
        guard("wp_linux_drm_syncobj_surface_v1", client, dh, || {
            let Some(s) = st.surfaces.get_mut(sid) else {
                if !matches!(req, Request::Destroy) {
                    res.post_error(Error::NoSurface, "the surface was destroyed");
                }
                return;
            };
            let point = |timeline: &WpLinuxDrmSyncobjTimelineV1, hi: u32, lo: u32| {
                let t = timeline.data::<Option<Arc<Timeline>>>()?.clone()?;
                Some(Point(t, (hi as u64) << 32 | lo as u64))
            };
            match req {
                Request::SetAcquirePoint { timeline, point_hi, point_lo } => {
                    s.pending.acquire = point(&timeline, point_hi, point_lo);
                }
                Request::SetReleasePoint { timeline, point_hi, point_lo } => {
                    s.pending.release = point(&timeline, point_hi, point_lo);
                }
                Request::Destroy => {
                    // Points set since the last commit may be discarded.
                    s.syncobj = None;
                    s.pending.acquire = None;
                    s.pending.release = None;
                }
                _ => {}
            }
        });
    }
}

/// Checks a commit's points against its buffer. Returns false (after
/// posting the protocol error) if the commit is invalid.
pub fn validate(st: &State, sid: &ObjectId) -> bool {
    use wp_linux_drm_syncobj_surface_v1::Error;
    let Some(s) = st.surfaces.get(sid) else { return true };
    let Some(obj) = s.syncobj.as_ref().filter(|o| o.is_alive()) else { return true };
    let p = &s.pending;
    let buffer = match &p.buffer {
        Some(Some(b)) => Some(b),
        _ => None,
    };
    let err = match (buffer, &p.acquire, &p.release) {
        (None, None, None) => None,
        (None, _, _) => Some((Error::NoBuffer, "points set without a buffer")),
        (Some(_), None, _) => Some((Error::NoAcquirePoint, "no acquire point")),
        (Some(_), _, None) => Some((Error::NoReleasePoint, "no release point")),
        (Some(b), Some(a), Some(r)) => {
            if !matches!(st.buffers.get(&b.id()), Some(crate::state::Buffer::Dmabuf(_))) {
                Some((Error::UnsupportedBuffer, "explicit sync needs a dmabuf"))
            } else if Arc::ptr_eq(&a.0, &r.0) && a.1 >= r.1 {
                Some((Error::ConflictingPoints, "acquire point not before release point"))
            } else {
                None
            }
        }
    };
    match err {
        Some((code, msg)) => {
            obj.post_error(code, msg);
            false
        }
        None => true,
    }
}
