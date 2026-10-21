//! awb: a Wayland compositor that shows each xdg_toplevel as its own Android
//! window. This file is the JNI surface: the Kotlin side calls in from the UI
//! thread, everything Wayland lives on one compositor thread (see `server`).

mod android;
mod data;
mod direct;
mod dmabuf;
mod gpu;
mod host;
mod output;
mod portal;
mod render;
mod seat;
mod server;
mod shell;
mod shm;
mod state;
mod surface;
mod syncobj;
mod text_input;

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};

use jni::JNIEnv;
use jni::objects::{JClass, JIntArray, JObject, JObjectArray, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};

use android::NativeSlot;

/// Requests from the Android side, drained by the compositor thread.
pub enum Cmd {
    /// A window's Android surface was attached; redraw it.
    Redraw(u32),
    /// The Android surface of a window changed size (pixels).
    Resize(u32, i32, i32),
    Focus(u32, bool),
    Pointer(PointerEvent),
    Key { win: u32, code: u32, down: bool, time: u32 },
    /// The user closed the Android window.
    Close(u32),
    /// Caption geometry of a window (see `Caption`).
    Caption(u32, Caption),
    /// An Android display appeared or changed.
    Display(DisplayInfo),
    DisplayRemoved(i32),
    /// The window is now on this Android display.
    WindowDisplay(u32, i32),
    /// SurfaceFlinger completed a direct-mode transaction (binder thread);
    /// `fence` releases the buffer it replaced (-1: already released).
    DirectDone { win: u32, token: u64, fence: i32 },
    /// Android's clipboard text changed.
    Clipboard(String),
    Drag(DragEvent),
    /// An edit from Android's IME in window `win`.
    Ime(u32, text_input::ImeEdit),
    /// A client's data source was read (None: failed or too large).
    SourceRead { id: u64, text: Option<String> },
}

/// An Android drag event on one of our windows (`View.onDragEvent`).
#[derive(Debug)]
pub struct DragEvent {
    pub win: u32,
    pub action: i32,
    /// View px.
    pub x: f32,
    pub y: f32,
    /// Our own drag (a client's); its data stays on the Wayland side.
    pub local: bool,
    /// For ENDED: whether some window or app took the drop.
    pub result: bool,
    pub mimes: Vec<String>,
    /// A drop's text, from another app.
    pub text: Option<String>,
}

impl DragEvent {
    // Android's DragEvent actions.
    pub const LOCATION: i32 = 2;
    pub const DROP: i32 = 3;
    pub const ENDED: i32 = 4;
    pub const EXITED: i32 = 6;
}

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: i32,
    pub width: i32,
    pub height: i32,
    /// Scale in 120ths (wp_fractional_scale units): Android density * 120.
    pub scale120: u32,
    pub refresh_mhz: i32,
    pub phys_mm: (i32, i32),
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Caption {
    /// Rows at the top that belong to the system (status bar when maximised).
    pub top: i32,
    /// Height of the caption band below `top`, in px.
    pub height: i32,
    /// System-owned rects in the band (app chip, window controls): x, y, w, h.
    pub controls: Vec<[i32; 4]>,
    /// Derive gesture-exclusion rects from the header's pixels.
    pub auto_exclusion: bool,
    /// The window spans the display: the client is configured maximised.
    pub maximized: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PointerEvent {
    pub win: u32,
    /// 0 = move/button change, 1 = scroll, 2 = left the window
    pub action: i32,
    pub x: f32,
    pub y: f32,
    /// Android MotionEvent button state bits
    pub buttons: i32,
    pub vscroll: f32,
    pub hscroll: f32,
    pub time: u32,
}

pub struct Shared {
    pub cmds: Mutex<VecDeque<Cmd>>,
    pub wake_fd: i32,
    /// Android surfaces by window id. The compositor thread holds the inner
    /// lock while drawing, so detaching waits for any in-flight frame.
    pub slots: Mutex<HashMap<u32, Arc<NativeSlot>>>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static PORTAL: OnceLock<Arc<portal::Portal>> = OnceLock::new();

impl Shared {
    pub fn push(&self, cmd: Cmd) {
        self.cmds.lock().unwrap_or_else(|e| e.into_inner()).push_back(cmd);
        let one: u64 = 1;
        unsafe { libc::write(self.wake_fd, &one as *const u64 as *const _, 8) };
    }

    pub fn slot(&self, win: u32) -> Arc<NativeSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.entry(win).or_insert_with(|| Arc::new(NativeSlot::default())).clone()
    }
}

fn push(cmd: Cmd) {
    if let Some(s) = SHARED.get() {
        s.push(cmd);
    }
}

/// Runs `f`, turning a panic into a log line instead of an abort across FFI.
fn ffi_guard<R>(what: &str, default: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            log::error!("panic in {what}: {}", panic_message(&e));
            default
        }
    }
}

pub fn panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic>".into()
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_start(
    mut env: JNIEnv,
    _cls: JClass,
    socket_path: JString,
    callbacks: JObject,
) -> jboolean {
    android::init_logging();
    ffi_guard("start", 0, || {
        if SHARED.get().is_some() {
            return 1;
        }
        let path: String = match env.get_string(&socket_path) {
            Ok(s) => s.into(),
            Err(_) => return 0,
        };
        let (vm, cb) = match (env.get_java_vm(), env.new_global_ref(callbacks)) {
            (Ok(vm), Ok(cb)) => (vm, cb),
            _ => return 0,
        };
        let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake_fd < 0 {
            return 0;
        }
        let shared = Arc::new(Shared {
            cmds: Mutex::new(VecDeque::new()),
            wake_fd,
            slots: Mutex::new(HashMap::new()),
        });
        let _ = SHARED.set(shared.clone());
        // The chroot's session bus lives next to the Wayland socket.
        let bus = std::path::Path::new(&path).with_file_name("bus");
        let portal = PORTAL.get_or_init(Default::default).clone();
        let address = format!("unix:path={}", bus.display());
        let _ = std::thread::Builder::new().name("awb-portal".into()).spawn(move || portal::run(address, portal));
        let cfg = server::Config { socket_path: path };
        std::thread::Builder::new()
            .name("awb-compositor".into())
            .spawn(move || server::run(cfg, shared, host::Host::new(vm, cb)))
            .map(|_| 1)
            .unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_attachSurface(
    env: JNIEnv,
    _cls: JClass,
    win: jint,
    surface: JObject,
) {
    ffi_guard("attachSurface", (), || {
        let Some(shared) = SHARED.get() else { return };
        let slot = shared.slot(win as u32);
        if surface.is_null() {
            slot.detach();
        } else {
            slot.attach(&env, &surface);
            shared.push(Cmd::Redraw(win as u32));
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_resize(_env: JNIEnv, _cls: JClass, win: jint, w: jint, h: jint) {
    push(Cmd::Resize(win as u32, w, h));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_focus(_env: JNIEnv, _cls: JClass, win: jint, focused: jboolean) {
    push(Cmd::Focus(win as u32, focused != 0));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_pointer(
    _env: JNIEnv,
    _cls: JClass,
    win: jint,
    action: jint,
    x: jfloat,
    y: jfloat,
    buttons: jint,
    vscroll: jfloat,
    hscroll: jfloat,
    time: jlong,
) {
    push(Cmd::Pointer(PointerEvent {
        win: win as u32,
        action,
        x,
        y,
        buttons,
        vscroll,
        hscroll,
        time: time as u32,
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_key(
    _env: JNIEnv,
    _cls: JClass,
    win: jint,
    code: jint,
    down: jboolean,
    time: jlong,
) {
    push(Cmd::Key { win: win as u32, code: code as u32, down: down != 0, time: time as u32 });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_close(_env: JNIEnv, _cls: JClass, win: jint) {
    push(Cmd::Close(win as u32));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_caption(
    env: JNIEnv,
    _cls: JClass,
    win: jint,
    top: jint,
    height: jint,
    rects: JIntArray,
    auto_exclusion: jboolean,
    maximized: jboolean,
) {
    ffi_guard("caption", (), || {
        let mut flat = Vec::new();
        if !rects.is_null() {
            let len = env.get_array_length(&rects).unwrap_or(0) as usize;
            flat = vec![0; len];
            let _ = env.get_int_array_region(&rects, 0, &mut flat);
        }
        let controls = flat.chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
        push(Cmd::Caption(win as u32, Caption { top, height, controls, auto_exclusion: auto_exclusion != 0, maximized: maximized != 0 }));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_display(
    mut env: JNIEnv,
    _cls: JClass,
    id: jint,
    width: jint,
    height: jint,
    scale120: jint,
    refresh_mhz: jint,
    width_mm: jint,
    height_mm: jint,
    name: JString,
) {
    ffi_guard("display", (), || {
        let name: String = env.get_string(&name).map(Into::into).unwrap_or_default();
        push(Cmd::Display(DisplayInfo {
            id,
            width,
            height,
            scale120: scale120.clamp(60, 960) as u32,
            refresh_mhz,
            phys_mm: (width_mm, height_mm),
            name,
        }));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_displayRemoved(_env: JNIEnv, _cls: JClass, id: jint) {
    push(Cmd::DisplayRemoved(id));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_windowDisplay(_env: JNIEnv, _cls: JClass, win: jint, display: jint) {
    push(Cmd::WindowDisplay(win as u32, display));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_appearance(_env: JNIEnv, _cls: JClass, dark: jboolean, accent: jint) {
    ffi_guard("appearance", (), || {
        // `accent` is Android ARGB; alpha 0 means none.
        let channel = |shift: u32| ((accent as u32 >> shift) & 0xff) as f64 / 255.0;
        let accent = ((accent as u32) >> 24 != 0).then(|| (channel(16), channel(8), channel(0)));
        PORTAL.get_or_init(Default::default).set(portal::Appearance { dark: dark != 0, accent });
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_clipboard(mut env: JNIEnv, _cls: JClass, text: JString) {
    ffi_guard("clipboard", (), || {
        if let Ok(t) = env.get_string(&text) {
            push(Cmd::Clipboard(t.into()));
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_drag(
    mut env: JNIEnv,
    _cls: JClass,
    win: jint,
    action: jint,
    x: jfloat,
    y: jfloat,
    local: jboolean,
    result: jboolean,
    mimes: JObjectArray,
    text: JString,
) {
    ffi_guard("drag", (), || {
        let mut list = Vec::new();
        if !mimes.is_null() {
            let n = env.get_array_length(&mimes).unwrap_or(0).min(64);
            for i in 0..n {
                let Ok(m) = env.get_object_array_element(&mimes, i) else { continue };
                if let Ok(m) = env.get_string(&JString::from(m)) {
                    list.push(m.into());
                }
            }
        }
        let text = if text.is_null() { None } else { env.get_string(&text).ok().map(Into::into) };
        push(Cmd::Drag(DragEvent { win: win as u32, action, x, y, local: local != 0, result: result != 0, mimes: list, text }));
    })
}

/// An IME edit: `kind` 0 = commit `text`, 1 = composing `text` with the cursor
/// at `a`, 2 = delete `a` units before and `b` after the cursor, 3 = the `a`
/// units before the cursor become composing `text` again.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_awb_Native_ime(mut env: JNIEnv, _cls: JClass, win: jint, kind: jint, text: JString, a: jint, b: jint) {
    ffi_guard("ime", (), || {
        let text: String = if text.is_null() { String::new() } else { env.get_string(&text).map(Into::into).unwrap_or_default() };
        let edit = match kind {
            0 => text_input::ImeEdit::Commit(text),
            1 => text_input::ImeEdit::Preedit(text, a),
            2 => text_input::ImeEdit::Delete(a, b),
            3 => text_input::ImeEdit::Recompose(text, a),
            _ => return,
        };
        push(Cmd::Ime(win as u32, edit));
    })
}
