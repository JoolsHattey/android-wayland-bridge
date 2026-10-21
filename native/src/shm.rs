//! wl_shm: pools are mmapped client memory. A client can shrink the backing
//! file under us, which turns a read into SIGBUS; like libwayland we catch
//! that, map zero pages over the pool and disconnect the client instead of
//! dying. Buffers are copied once at commit time (into a `Texture`) and
//! released immediately, so client memory is never touched afterwards.

use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use wayland_server::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::state::{Buffer, State, guard};

pub struct Pool {
    ptr: *mut u8,
    size: usize,
    fd: OwnedFd,
}

impl Pool {
    fn map(fd: OwnedFd, size: usize) -> Option<Pool> {
        if size == 0 {
            return None;
        }
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(Pool { ptr: ptr as *mut u8, size, fd })
    }

    fn resize(&mut self, size: usize) -> bool {
        if size <= self.size {
            return size == self.size;
        }
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, self.fd.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return false;
        }
        unsafe { libc::munmap(self.ptr as *mut _, self.size) };
        self.ptr = ptr as *mut u8;
        self.size = size;
        true
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut _, self.size) };
    }
}

pub struct BufferInfo {
    pool: Rc<RefCell<Pool>>,
    offset: usize,
    pub width: i32,
    pub height: i32,
    stride: usize,
    format: wl_shm::Format,
}

/// A surface's committed pixels, converted to RGBA byte order (as u32 LE:
/// 0xAABBGGRR), premultiplied like wl_shm ARGB. A dmabuf texture has no
/// pixels here: the GPU samples the client's buffer, which is held (not
/// released) while the texture lives.
pub struct Texture {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u32>,
    pub opaque: bool,
    pub dmabuf: Option<(wl_buffer::WlBuffer, Rc<crate::gpu::Dmabuf>)>,
    /// Explicit sync points of the commit (dmabuf only).
    pub sync: Option<crate::syncobj::CommitSync>,
    /// Unique per commit, so the GPU path uploads each shm texture once.
    pub serial: u64,
}

static TEXTURE_SERIAL: AtomicU64 = AtomicU64::new(1);

pub fn next_texture_serial() -> u64 {
    TEXTURE_SERIAL.fetch_add(1, Ordering::Relaxed)
}

// ---- SIGBUS protection -------------------------------------------------

static GUARD_START: AtomicUsize = AtomicUsize::new(0);
static GUARD_LEN: AtomicUsize = AtomicUsize::new(0);
static FAULTED: AtomicBool = AtomicBool::new(false);
static mut OLD_ACTION: Option<libc::sigaction> = None;
static INSTALL: Once = Once::new();

extern "C" fn on_sigbus(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    let addr = unsafe { (*info).si_addr() } as usize;
    let start = GUARD_START.load(Ordering::SeqCst);
    let len = GUARD_LEN.load(Ordering::SeqCst);
    if start != 0 && addr >= start && addr < start + len {
        // Replace the pool with zero pages; the faulting read then succeeds.
        unsafe {
            libc::mmap(
                start as *mut _,
                len,
                libc::PROT_READ,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
        }
        FAULTED.store(true, Ordering::SeqCst);
        return;
    }
    // Not ours: hand over to whoever was installed before (ART's chain).
    unsafe {
        #[allow(static_mut_refs)]
        match OLD_ACTION {
            Some(old) if old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN => {
                if old.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                        std::mem::transmute(old.sa_sigaction);
                    f(sig, info, ctx);
                } else {
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(old.sa_sigaction);
                    f(sig);
                }
            }
            _ => {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
        }
    }
}

fn install_sigbus_handler() {
    INSTALL.call_once(|| unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigbus as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        libc::sigemptyset(&mut sa.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGBUS, &sa, &mut old) == 0 {
            OLD_ACTION = Some(old);
        }
    });
}

impl BufferInfo {
    /// Copies the buffer out of client memory. Returns None if the client's
    /// memory went away underneath us (the client is then misbehaving).
    pub fn read(&self) -> Option<Texture> {
        let pool = self.pool.borrow();
        let (w, h) = (self.width as usize, self.height as usize);
        let end = self.offset.checked_add(self.stride.checked_mul(h)?)?;
        if end > pool.size {
            return None;
        }
        let opaque = matches!(self.format, wl_shm::Format::Xrgb8888);
        let mut data = vec![0u32; w * h];
        GUARD_START.store(pool.ptr as usize, Ordering::SeqCst);
        GUARD_LEN.store(pool.size, Ordering::SeqCst);
        FAULTED.store(false, Ordering::SeqCst);
        for y in 0..h {
            let row = unsafe { pool.ptr.add(self.offset + y * self.stride) as *const u32 };
            let out = &mut data[y * w..(y + 1) * w];
            for (x, px) in out.iter_mut().enumerate() {
                // wl_shm ARGB8888 is 0xAARRGGBB in a little-endian u32; the
                // Android buffer wants bytes R,G,B,A, i.e. 0xAABBGGRR.
                let v = unsafe { row.add(x).read_unaligned() };
                let v = (v & 0xff00ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16);
                *px = if opaque { v | 0xff000000 } else { v };
            }
        }
        GUARD_START.store(0, Ordering::SeqCst);
        GUARD_LEN.store(0, Ordering::SeqCst);
        if FAULTED.swap(false, Ordering::SeqCst) {
            return None;
        }
        Some(Texture { width: self.width, height: self.height, data, opaque, dmabuf: None, sync: None, serial: next_texture_serial() })
    }
}

// ---- protocol ----------------------------------------------------------

impl GlobalDispatch<wl_shm::WlShm, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, res: New<wl_shm::WlShm>, _: &(), init: &mut DataInit<'_, State>) {
        install_sigbus_handler();
        let shm = init.init(res, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        shm: &wl_shm::WlShm,
        req: wl_shm::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_shm", client, dh, || {
            if let wl_shm::Request::CreatePool { id, fd, size } = req {
                let pool = init.init(id, ());
                match usize::try_from(size).ok().and_then(|s| Pool::map(fd, s)) {
                    Some(p) => {
                        st.pools.insert(pool.id(), Rc::new(RefCell::new(p)));
                    }
                    None => shm.post_error(wl_shm::Error::InvalidFd, "cannot map pool"),
                }
            }
        });
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        pool_res: &wl_shm_pool::WlShmPool,
        req: wl_shm_pool::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("wl_shm_pool", client, dh, || match req {
            wl_shm_pool::Request::CreateBuffer { id, offset, width, height, stride, format } => {
                let buf = init.init(id, ());
                let Some(pool) = st.pools.get(&pool_res.id()).cloned() else { return };
                let format = match format {
                    WEnum::Value(f @ (wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)) => f,
                    _ => {
                        pool_res.post_error(wl_shm::Error::InvalidFormat, "unsupported format");
                        return;
                    }
                };
                let size = pool.borrow().size as i64;
                let ok = offset >= 0
                    && width > 0
                    && height > 0
                    && stride as i64 >= width as i64 * 4
                    && offset as i64 + stride as i64 * height as i64 <= size;
                if !ok {
                    pool_res.post_error(wl_shm::Error::InvalidStride, "buffer outside pool");
                    return;
                }
                st.buffers.insert(
                    buf.id(),
                    Buffer::Shm(BufferInfo { pool, offset: offset as usize, width, height, stride: stride as usize, format }),
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                if let Some(pool) = st.pools.get(&pool_res.id()) {
                    let ok = usize::try_from(size).map(|s| pool.borrow_mut().resize(s)).unwrap_or(false);
                    if !ok {
                        pool_res.post_error(wl_shm::Error::InvalidFd, "cannot resize pool");
                    }
                }
            }
            _ => {}
        });
    }

    fn destroyed(st: &mut State, _: wayland_server::backend::ClientId, pool: &wl_shm_pool::WlShmPool, _: &()) {
        // Buffers keep the mapping alive through their Rc.
        st.pools.remove(&pool.id());
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_buffer::WlBuffer, _: wl_buffer::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}

    fn destroyed(st: &mut State, _: wayland_server::backend::ClientId, buf: &wl_buffer::WlBuffer, _: &()) {
        st.buffers.remove(&buf.id());
    }
}
