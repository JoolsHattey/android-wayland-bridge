//! Thin NDK bindings: logcat logging and the Android surfaces windows draw
//! into (CPU locking, or a Vulkan swapchain).

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::Mutex;

use jni::JNIEnv;
use jni::objects::JObject;

#[repr(C)]
pub struct ANativeWindow {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
pub struct ANativeWindowBuffer {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
    pub bits: *mut c_void,
    pub reserved: [u32; 6],
}

pub const WINDOW_FORMAT_RGBA_8888: i32 = 1;

#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(env: *mut jni::sys::JNIEnv, surface: jni::sys::jobject) -> *mut ANativeWindow;
    fn ANativeWindow_release(window: *mut ANativeWindow);
    fn ANativeWindow_setBuffersGeometry(window: *mut ANativeWindow, w: i32, h: i32, format: i32) -> i32;
    fn ANativeWindow_lock(window: *mut ANativeWindow, out: *mut ANativeWindowBuffer, dirty: *mut ARect) -> i32;
    fn ANativeWindow_unlockAndPost(window: *mut ANativeWindow) -> i32;
}

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

struct Logcat;

impl log::Log for Logcat {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &log::Record) {
        let prio = match r.level() {
            log::Level::Error => 6,
            log::Level::Warn => 5,
            log::Level::Info => 4,
            log::Level::Debug => 3,
            log::Level::Trace => 2,
        };
        let msg = CString::new(format!("{}", r.args()).replace('\0', "?")).unwrap_or_default();
        unsafe { __android_log_write(prio, c"awb".as_ptr(), msg.as_ptr()) };
    }

    fn flush(&self) {}
}

pub fn init_logging() {
    if log::set_logger(&Logcat).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
        std::panic::set_hook(Box::new(|info| {
            log::error!("panic: {info}");
        }));
    }
}

struct Win {
    ptr: *mut ANativeWindow,
    /// Buffer geometry last set, so it is only changed on resize.
    geometry: (i32, i32),
    /// The GPU path's swapchain on this window.
    target: Option<crate::gpu::Target>,
}

// The pointer is only used under the slot's mutex.
unsafe impl Send for Win {}

impl Drop for Win {
    fn drop(&mut self) {
        // The swapchain goes first: it must not outlive its window.
        self.target = None;
        unsafe { ANativeWindow_release(self.ptr) };
    }
}

/// One Android surface (an Activity's SurfaceView), shared between the UI
/// thread (attach/detach) and the compositor thread (draw).
#[derive(Default)]
pub struct NativeSlot {
    win: Mutex<Option<Win>>,
}

/// A locked frame to draw into. Rows are `stride` u32 pixels in RGBA byte order.
pub struct Frame<'a> {
    pub pixels: &'a mut [u32],
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

impl NativeSlot {
    pub fn attach(&self, env: &JNIEnv, surface: &JObject) {
        let ptr = unsafe { ANativeWindow_fromSurface(env.get_raw(), surface.as_raw()) };
        let mut g = self.win.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
        if !ptr.is_null() {
            *g = Some(Win { ptr, geometry: (0, 0), target: None });
        }
    }

    pub fn detach(&self) {
        *self.win.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Runs `f` on the attached window and its swapchain slot, under the
    /// lock. None if no surface is attached.
    pub fn with_target<R>(&self, f: impl FnOnce(*mut ANativeWindow, &mut Option<crate::gpu::Target>) -> R) -> Option<R> {
        let mut g = self.win.lock().unwrap_or_else(|e| e.into_inner());
        let win = g.as_mut()?;
        Some(f(win.ptr, &mut win.target))
    }

    pub fn is_attached(&self) -> bool {
        self.win.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Locks a `w`x`h` buffer, lets `draw` fill it and posts it. Returns false
    /// if no surface is attached or locking failed.
    pub fn draw(&self, w: i32, h: i32, draw: impl FnOnce(&mut Frame)) -> bool {
        let mut g = self.win.lock().unwrap_or_else(|e| e.into_inner());
        let Some(win) = g.as_mut() else { return false };
        if w <= 0 || h <= 0 {
            return false;
        }
        unsafe {
            if win.geometry != (w, h) {
                ANativeWindow_setBuffersGeometry(win.ptr, w, h, WINDOW_FORMAT_RGBA_8888);
                win.geometry = (w, h);
            }
            let mut buf: ANativeWindowBuffer = std::mem::zeroed();
            if ANativeWindow_lock(win.ptr, &mut buf, std::ptr::null_mut()) != 0 {
                return false;
            }
            if !buf.bits.is_null() && buf.format == WINDOW_FORMAT_RGBA_8888 && buf.stride >= buf.width {
                let len = buf.stride as usize * buf.height as usize;
                let pixels = std::slice::from_raw_parts_mut(buf.bits as *mut u32, len);
                draw(&mut Frame { pixels, width: buf.width, height: buf.height, stride: buf.stride });
            } else {
                log::warn!("unexpected window buffer format {} stride {}", buf.format, buf.stride);
            }
            ANativeWindow_unlockAndPost(win.ptr);
        }
        true
    }
}

impl Drop for NativeSlot {
    fn drop(&mut self) {
        self.detach();
    }
}
