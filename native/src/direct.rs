//! Zero-copy presentation: a client's dmabuf goes to SurfaceFlinger as an
//! AHardwareBuffer on a child SurfaceControl of the window, so a window
//! that is one surface drawn 1:1 is never blitted (see `Renderer`).
//!
//! There is no public API to wrap a foreign dmabuf as an AHardwareBuffer.
//! `AHardwareBuffer_recvHandleFromUnixSocket` is public, though, and takes
//! GraphicBuffer's flattened form plus the fds, so we write that message
//! ourselves around minigbm's buffer handle (`cros_gralloc_handle`, as this
//! device's gralloc lays it out: native handle version 12). The mapper then
//! imports the dmabuf like any gralloc buffer. Each buffer also gets
//! minigbm's metadata region (dataspace, blend mode, ...), copied from a
//! real gralloc buffer: without one, every SurfaceFlinger metadata query
//! logs an error.

use std::ffi::{c_char, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::android::{ANativeWindow, ARect};
use crate::{Cmd, Shared};

#[repr(C)]
pub struct AHardwareBuffer {
    _private: [u8; 0],
}

#[repr(C)]
pub struct ASurfaceControl {
    _private: [u8; 0],
}

#[repr(C)]
pub struct ASurfaceTransaction {
    _private: [u8; 0],
}

#[repr(C)]
pub struct ASurfaceTransactionStats {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct AHardwareBufferDesc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}

type OnComplete = extern "C" fn(*mut c_void, *mut ASurfaceTransactionStats);

#[link(name = "android")]
unsafe extern "C" {
    fn AHardwareBuffer_recvHandleFromUnixSocket(fd: i32, out: *mut *mut AHardwareBuffer) -> i32;
    fn AHardwareBuffer_sendHandleToUnixSocket(buffer: *const AHardwareBuffer, fd: i32) -> i32;
    fn AHardwareBuffer_allocate(desc: *const AHardwareBufferDesc, out: *mut *mut AHardwareBuffer) -> i32;
    fn AHardwareBuffer_release(buffer: *mut AHardwareBuffer);
    fn ASurfaceControl_createFromWindow(parent: *mut ANativeWindow, name: *const c_char) -> *mut ASurfaceControl;
    fn ASurfaceControl_acquire(sc: *mut ASurfaceControl);
    fn ASurfaceControl_release(sc: *mut ASurfaceControl);
    fn ASurfaceTransaction_create() -> *mut ASurfaceTransaction;
    fn ASurfaceTransaction_delete(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_apply(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_setOnComplete(t: *mut ASurfaceTransaction, ctx: *mut c_void, f: OnComplete);
    fn ASurfaceTransaction_setBuffer(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, b: *mut AHardwareBuffer, fence: i32);
    fn ASurfaceTransaction_setGeometry(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        src: *const ARect,
        dst: *const ARect,
        transform: i32,
    );
    fn ASurfaceTransaction_setVisibility(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, visibility: i8);
    fn ASurfaceTransaction_setZOrder(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, z: i32);
    fn ASurfaceTransaction_setBufferTransparency(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, transparency: i8);
    fn ASurfaceTransactionStats_getPreviousReleaseFenceFd(stats: *mut ASurfaceTransactionStats, sc: *mut ASurfaceControl) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

/// GPU sampled | GPU framebuffer | composer overlay, as gralloc gives
/// buffers SurfaceFlinger composites.
const USAGE: u64 = 0x100 | 0x200 | 0x800;
/// minigbm's BO_USE flags for that usage, read from a gralloc buffer.
const BO_USE: u64 = 0x25;
const CROS_GRALLOC_MAGIC: u32 = 0xabcd_dcba;

/// The HAL pixel format for a DRM format we can hand over (8-bit RGB only;
/// X formats are marked opaque on the layer instead).
pub fn hal_format(fourcc_code: u32) -> Option<i32> {
    match fourcc_code {
        f if f == fourcc(b"AB24") => Some(1), // RGBA_8888
        f if f == fourcc(b"XB24") => Some(2), // RGBX_8888
        f if f == fourcc(b"AR24") || f == fourcc(b"XR24") => Some(5), // BGRA_8888
        _ => None,
    }
}

/// minigbm's i915 tiling for a modifier it can import, or None.
pub fn tiling(modifier: u64) -> Option<u32> {
    match modifier {
        0 => Some(0),                     // linear
        0x0100_0000_0000_0001 => Some(1), // X-tiled
        0x0100_0000_0000_0009 => Some(9), // 4-tiled
        _ => None,
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Ints of GraphicBuffer's flattened header, before the handle's ints.
const HEADER_INTS: usize = 13;
/// minigbm's fd slots (4 planes and the metadata region).
const MAX_FDS: usize = 5;

/// A fresh gralloc buffer's metadata region, as a template.
fn metadata_template() -> Option<&'static [u8]> {
    static TEMPLATE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let b = placeholder();
            if b.is_null() {
                return None;
            }
            let t = unsafe { read_metadata(b) };
            release(b);
            if t.is_none() {
                log::warn!("direct: no gralloc metadata template");
            }
            t
        })
        .as_deref()
}

/// Flattens a gralloc buffer through a socket and maps its metadata fd.
unsafe fn read_metadata(b: *mut AHardwareBuffer) -> Option<Vec<u8>> {
    unsafe {
        let mut sv = [0i32; 2];
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) != 0 {
            return None;
        }
        let (tx, rx) = (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1]));
        if AHardwareBuffer_sendHandleToUnixSocket(b, tx.as_raw_fd()) != 0 {
            return None;
        }
        let mut msg = [0i32; 256];
        let mut iov = libc::iovec { iov_base: msg.as_mut_ptr() as *mut _, iov_len: std::mem::size_of_val(&msg) };
        let space = libc::CMSG_SPACE((4 * MAX_FDS) as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut mh: libc::msghdr = std::mem::zeroed();
        mh.msg_iov = &mut iov;
        mh.msg_iovlen = 1;
        mh.msg_control = cbuf.as_mut_ptr() as *mut _;
        mh.msg_controllen = space as _;
        let n = libc::recvmsg(rx.as_raw_fd(), &mut mh, 0);
        let c = libc::CMSG_FIRSTHDR(&mh);
        if n < (HEADER_INTS * 4) as isize || c.is_null() || (*c).cmsg_type != libc::SCM_RIGHTS {
            return None;
        }
        let nfds = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
        let fds: Vec<OwnedFd> =
            (0..nfds).map(|i| OwnedFd::from_raw_fd(*(libc::CMSG_DATA(c) as *const i32).add(i))).collect();
        // One plane, then the metadata fd; its size is 30 ints into the
        // handle's ints for a two-fd handle (see `wrap`).
        if fds.len() != 2 || msg[10] != 2 {
            return None;
        }
        let at = HEADER_INTS + 30;
        let size = (msg[at] as u32 as u64 | (msg[at + 1] as u32 as u64) << 32) as usize;
        if size == 0 || size > 1 << 20 {
            return None;
        }
        let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fds[1].as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return None;
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        libc::munmap(p, size);
        Some(bytes)
    }
}

/// A new metadata region filled from the template.
fn metadata_region(template: &[u8]) -> Option<OwnedFd> {
    unsafe {
        let fd = libc::memfd_create(c"awb-gralloc-metadata".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut file = std::fs::File::from(fd);
        std::io::Write::write_all(&mut file, template).ok()?;
        Some(OwnedFd::from(file))
    }
}

/// A dmabuf's single plane, to be wrapped.
pub struct Plane<'a> {
    pub fd: &'a std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub size: u64,
}

/// Wraps a client dmabuf as an AHardwareBuffer (a new reference).
pub fn wrap(plane: &Plane, width: i32, height: i32, fourcc_code: u32, modifier: u64) -> Option<*mut AHardwareBuffer> {
    let hal = hal_format(fourcc_code)?;
    let tiling = tiling(modifier)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let metadata = metadata_template().and_then(metadata_region);
    let reserved = metadata.as_ref().map_or(0, |_| metadata_template().unwrap().len() as u64);
    let mut fds = vec![plane.fd.as_raw_fd()];
    fds.extend(metadata.as_ref().map(|m| m.as_raw_fd()));
    let total = plane.size + reserved;
    let lo = |v: u64| v as u32 as i32;
    let hi = |v: u64| (v >> 32) as u32 as i32;
    // cros_gralloc_handle after its fds: the unused fd slots, then strides,
    // offsets, sizes (4 planes each), id, size, format, tiling, modifier,
    // use flags, magic, pixel stride, HAL format, usage, planes, metadata
    // region size and total size.
    let mut ints: Vec<i32> = vec![-1; MAX_FDS - fds.len()];
    #[rustfmt::skip]
    ints.extend([
        plane.stride as i32, 0, 0, 0,
        plane.offset as i32, 0, 0, 0,
        lo(plane.size - plane.offset as u64), 0, 0, 0,
        id as i32, width, height, fourcc_code as i32, tiling as i32,
        lo(modifier), hi(modifier),
        lo(BO_USE), hi(BO_USE),
        CROS_GRALLOC_MAGIC as i32,
        (plane.stride / 4) as i32,
        hal,
        lo(USAGE), hi(USAGE),
        1,
        lo(reserved), hi(reserved),
        lo(total), hi(total),
    ]);
    // GraphicBuffer::flatten: magic 'GB01', size, stride (px), format,
    // layers, usage (low), id, generation, fds, ints, usage (high).
    let pid = unsafe { libc::getpid() } as i32;
    let mut msg: Vec<i32> = vec![
        0x4742_3031,
        width,
        height,
        (plane.stride / 4) as i32,
        hal,
        1,
        lo(USAGE),
        pid,
        id as i32,
        0,
        fds.len() as i32,
        ints.len() as i32,
        hi(USAGE),
    ];
    msg.extend(&ints);
    unsafe {
        let mut sv = [0i32; 2];
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) != 0 {
            return None;
        }
        let close = || {
            libc::close(sv[0]);
            libc::close(sv[1]);
        };
        let mut iov = libc::iovec { iov_base: msg.as_mut_ptr() as *mut _, iov_len: msg.len() * 4 };
        let bytes = (fds.len() * 4) as u32;
        let space = libc::CMSG_SPACE(bytes) as usize;
        let mut cbuf = vec![0u8; space];
        let mut mh: libc::msghdr = std::mem::zeroed();
        mh.msg_iov = &mut iov;
        mh.msg_iovlen = 1;
        mh.msg_control = cbuf.as_mut_ptr() as *mut _;
        mh.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&mh);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut i32, fds.len());
        if libc::sendmsg(sv[0], &mh, 0) < 0 {
            close();
            return None;
        }
        let mut out = std::ptr::null_mut();
        let r = AHardwareBuffer_recvHandleFromUnixSocket(sv[1], &mut out);
        close();
        if r != 0 || out.is_null() {
            log::warn!("direct: gralloc rejected a {width}x{height} {fourcc_code:#x}/{modifier:#x} buffer ({r})");
            return None;
        }
        Some(out)
    }
}

pub fn release(buffer: *mut AHardwareBuffer) {
    if !buffer.is_null() {
        unsafe { AHardwareBuffer_release(buffer) };
    }
}

/// A 1x1 buffer to swap in when a window leaves direct mode, so
/// SurfaceFlinger gives the client's buffer back.
pub fn placeholder() -> *mut AHardwareBuffer {
    let desc = AHardwareBufferDesc { width: 1, height: 1, layers: 1, format: 1, usage: USAGE, ..Default::default() };
    let mut out = std::ptr::null_mut();
    unsafe { AHardwareBuffer_allocate(&desc, &mut out) };
    out
}

/// A window's child layer, above its swapchain's.
pub struct Layer(*mut ASurfaceControl);

// Only used under the slot's mutex.
unsafe impl Send for Layer {}

impl Layer {
    pub fn new(win: *mut ANativeWindow) -> Option<Layer> {
        let sc = unsafe { ASurfaceControl_createFromWindow(win, c"awb-direct".as_ptr()) };
        (!sc.is_null()).then_some(Layer(sc))
    }

    /// Shows `buffer` (with its source crop, buffer px, and whether it is
    /// opaque) at `dst`, or hides the layer with `None`, swapping in the
    /// placeholder. Completion arrives as `Cmd::DirectDone`.
    pub fn present(
        &self,
        buffer: Option<(*mut AHardwareBuffer, [i32; 4], bool)>,
        placeholder: *mut AHardwareBuffer,
        dst: [i32; 4],
        shared: &Arc<Shared>,
        win: u32,
        token: u64,
    ) {
        unsafe {
            let t = ASurfaceTransaction_create();
            match buffer {
                Some((b, src, opaque)) => {
                    ASurfaceTransaction_setBuffer(t, self.0, b, -1);
                    let src = ARect { left: src[0], top: src[1], right: src[2], bottom: src[3] };
                    let dst = ARect { left: dst[0], top: dst[1], right: dst[2], bottom: dst[3] };
                    ASurfaceTransaction_setGeometry(t, self.0, &src, &dst, 0);
                    ASurfaceTransaction_setBufferTransparency(t, self.0, if opaque { 2 } else { 1 });
                    ASurfaceTransaction_setZOrder(t, self.0, 1);
                    ASurfaceTransaction_setVisibility(t, self.0, 1);
                }
                None => {
                    ASurfaceTransaction_setBuffer(t, self.0, placeholder, -1);
                    ASurfaceTransaction_setVisibility(t, self.0, 0);
                }
            }
            ASurfaceControl_acquire(self.0);
            let ctx = Box::new(Done { shared: shared.clone(), sc: self.0, win, token });
            ASurfaceTransaction_setOnComplete(t, Box::into_raw(ctx) as *mut c_void, on_complete);
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
        }
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        unsafe { ASurfaceControl_release(self.0) };
    }
}

struct Done {
    shared: Arc<Shared>,
    sc: *mut ASurfaceControl,
    win: u32,
    token: u64,
}

/// On a binder thread: hands the previous buffer's release fence to the
/// compositor thread.
extern "C" fn on_complete(ctx: *mut c_void, stats: *mut ASurfaceTransactionStats) {
    let ctx = unsafe { Box::from_raw(ctx as *mut Done) };
    let fence = unsafe { ASurfaceTransactionStats_getPreviousReleaseFenceFd(stats, ctx.sc) };
    unsafe { ASurfaceControl_release(ctx.sc) };
    ctx.shared.push(Cmd::DirectDone { win: ctx.win, token: ctx.token, fence });
}
