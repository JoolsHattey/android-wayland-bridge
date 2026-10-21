//! Calls from the compositor thread into Kotlin (`dev.awb.NativeCallbacks`).
//! Kotlin posts each call to the main thread, so none of these block on UI.

use jni::objects::{GlobalRef, JIntArray, JObject, JValue};
use jni::JavaVM;

use crate::seat::Image;

fn int_array<'a>(env: &mut jni::JNIEnv<'a>, values: &[i32]) -> jni::errors::Result<JIntArray<'a>> {
    let arr = env.new_int_array(values.len() as i32)?;
    env.set_int_array_region(&arr, 0, values)?;
    Ok(arr)
}

pub struct Host {
    vm: JavaVM,
    cb: GlobalRef,
}

impl Host {
    pub fn new(vm: JavaVM, cb: GlobalRef) -> Self {
        Host { vm, cb }
    }

    fn with_env(&self, f: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<()>) {
        let mut env = match self.vm.attach_current_thread_permanently() {
            Ok(e) => e,
            Err(e) => {
                log::error!("JNI attach failed: {e}");
                return;
            }
        };
        let cb = self.cb.as_obj();
        let r = env.with_local_frame(16, |env| f(env, cb));
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        if let Err(e) = r {
            log::warn!("JNI callback failed: {e}");
        }
    }

    /// `buffer` as a Java HardwareBuffer that outlives this call, for a
    /// synced redraw's reply (`Cmd::RedrawSync`).
    pub fn hardware_buffer(&self, buffer: *mut crate::direct::AHardwareBuffer) -> Option<GlobalRef> {
        let mut env = self.vm.attach_current_thread_permanently().ok()?;
        let r = env.with_local_frame(4, |env| -> jni::errors::Result<_> {
            let obj = crate::direct::to_java(env, buffer);
            if obj.is_null() {
                return Ok(None);
            }
            Ok(Some(env.new_global_ref(&obj)?))
        });
        r.unwrap_or_else(|e| {
            log::warn!("hardware buffer: {e}");
            None
        })
    }

    /// Shows a direct-mode frame on the window's own layer (a window that
    /// owns it, see `Cmd::OwnsLayer`), or with None clears the layer. The
    /// window reports it on screen with `Native.presented(win, token)`.
    pub fn present(&self, win: u32, frame: Option<(*mut crate::direct::AHardwareBuffer, [i32; 4], bool)>, dst: [i32; 4], token: u64) {
        self.with_env(|env, cb| {
            let (buffer, src, opaque) = match frame {
                Some((b, src, opaque)) => (crate::direct::to_java(env, b), src, opaque),
                None => (JObject::null(), [0; 4], false),
            };
            let src = int_array(env, &src)?;
            let dst = int_array(env, &dst)?;
            env.call_method(
                cb,
                "onPresent",
                "(ILandroid/hardware/HardwareBuffer;[I[IZJ)V",
                &[
                    JValue::Int(win as i32),
                    JValue::Object(&buffer),
                    JValue::Object(&src),
                    JValue::Object(&dst),
                    JValue::Bool(opaque as u8),
                    JValue::Long(token as i64),
                ],
            )?;
            Ok(())
        });
    }

    /// A toplevel got its first buffer: open an Android window for `w`x`h`
    /// logical px (Kotlin applies the target display's density).
    /// `ssd`: the window wants server-side decorations (see `decorations`).
    pub fn map(&self, win: u32, w: i32, h: i32, title: &str, app_id: &str, ssd: bool) {
        self.with_env(|env, cb| {
            let t = env.new_string(title)?;
            let a = env.new_string(app_id)?;
            env.call_method(
                cb,
                "onMap",
                "(IIILjava/lang/String;Ljava/lang/String;Z)V",
                &[
                    JValue::Int(win as i32),
                    JValue::Int(w),
                    JValue::Int(h),
                    JValue::Object(&t),
                    JValue::Object(&a),
                    JValue::Bool(ssd as u8),
                ],
            )?;
            Ok(())
        });
    }

    /// The window switched between client-side decorations (its header in
    /// Android's transparent caption) and server-side (an opaque caption).
    pub fn decorations(&self, win: u32, ssd: bool) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onDecorations", "(IZ)V", &[JValue::Int(win as i32), JValue::Bool(ssd as u8)])?;
            Ok(())
        });
    }

    pub fn unmap(&self, win: u32) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onUnmap", "(I)V", &[JValue::Int(win as i32)])?;
            Ok(())
        });
    }

    pub fn title(&self, win: u32, title: &str) {
        self.with_env(|env, cb| {
            let t = env.new_string(title)?;
            env.call_method(cb, "onTitle", "(ILjava/lang/String;)V", &[JValue::Int(win as i32), JValue::Object(&t)])?;
            Ok(())
        });
    }

    pub fn minimize(&self, win: u32) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onMinimize", "(I)V", &[JValue::Int(win as i32)])?;
            Ok(())
        });
    }

    /// The client asked for an interactive move (`edges` 0, xdg_toplevel.move)
    /// or resize (xdg_toplevel.resize edges).
    pub fn move_request(&self, win: u32, edges: u32) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onMoveRequest", "(II)V", &[JValue::Int(win as i32), JValue::Int(edges as i32)])?;
            Ok(())
        });
    }

    /// Whether the header under the caption controls is light (so the
    /// controls should be drawn dark).
    pub fn caption_light(&self, win: u32, light: bool) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onCaptionLight", "(IZ)V", &[JValue::Int(win as i32), JValue::Bool(light as u8)])?;
            Ok(())
        });
    }

    /// Header areas the app must receive input for (x, y, w, h px); the
    /// rest of the caption is left to the system for window dragging.
    pub fn exclusion(&self, win: u32, rects: &[[i32; 4]]) {
        let flat: Vec<i32> = rects.iter().flatten().copied().collect();
        self.with_env(|env, cb| {
            let arr = env.new_int_array(flat.len() as i32)?;
            env.set_int_array_region(&arr, 0, &flat)?;
            env.call_method(cb, "onExclusion", "(I[I)V", &[JValue::Int(win as i32), JValue::Object(&arr)])?;
            Ok(())
        });
    }

    /// Client cursor image, or `None` to hide the pointer.
    pub fn cursor(&self, win: u32, image: Option<&Image>) {
        self.with_env(|env, cb| {
            let (arr, w, h, hx, hy, scale) = match image {
                Some(i) => (int_array(env, &i.argb)?, i.width, i.height, i.hot_x, i.hot_y, i.scale),
                None => (JObject::null().into(), 0, 0, 0, 0, 1.0),
            };
            env.call_method(
                cb,
                "onCursor",
                "(I[IIIIIF)V",
                &[
                    JValue::Int(win as i32),
                    JValue::Object(&arr),
                    JValue::Int(w),
                    JValue::Int(h),
                    JValue::Int(hx),
                    JValue::Int(hy),
                    JValue::Float(scale),
                ],
            )?;
            Ok(())
        });
    }

    /// A client copied text: put it on Android's clipboard.
    pub fn clipboard(&self, text: &str) {
        self.with_env(|env, cb| {
            let t = env.new_string(text)?;
            env.call_method(cb, "onClipboard", "(Ljava/lang/String;)V", &[JValue::Object(&t)])?;
            Ok(())
        });
    }

    /// A client started a drag in window `win`: start Android's, carrying
    /// `text` (for Android apps) and showing `icon`.
    pub fn start_drag(&self, win: u32, text: Option<&str>, icon: Option<Image>) {
        self.with_env(|env, cb| {
            let t: JObject = match text {
                Some(t) => env.new_string(t)?.into(),
                None => JObject::null(),
            };
            let (arr, w, h, scale) = match &icon {
                Some(i) => (int_array(env, &i.argb)?, i.width, i.height, i.scale),
                None => (JObject::null().into(), 0, 0, 1.0),
            };
            env.call_method(
                cb,
                "onStartDrag",
                "(ILjava/lang/String;[IIIF)V",
                &[JValue::Int(win as i32), JValue::Object(&t), JValue::Object(&arr), JValue::Int(w), JValue::Int(h), JValue::Float(scale)],
            )?;
            Ok(())
        });
    }

    /// The focused text input of window `win` changed; None: no text input.
    pub fn text_input(&self, win: u32, state: Option<&crate::text_input::ImeState>) {
        self.with_env(|env, cb| {
            let (enabled, purpose, hint) = state.map_or((false, 0, 0), |s| (true, s.purpose as i32, s.hint as i32));
            let (text, cursor, anchor): (JObject, i32, i32) = match state.and_then(|s| s.text.as_ref()) {
                Some((t, c, a)) => (env.new_string(t)?.into(), *c, *a),
                None => (JObject::null(), -1, -1),
            };
            let rect: JObject = match state.and_then(|s| s.rect) {
                Some(r) => int_array(env, &r)?.into(),
                None => JObject::null(),
            };
            env.call_method(
                cb,
                "onTextInput",
                "(IZIILjava/lang/String;II[I)V",
                &[
                    JValue::Int(win as i32),
                    JValue::Bool(enabled as u8),
                    JValue::Int(purpose),
                    JValue::Int(hint),
                    JValue::Object(&text),
                    JValue::Int(cursor),
                    JValue::Int(anchor),
                    JValue::Object(&rect),
                ],
            )?;
            Ok(())
        });
    }
}
