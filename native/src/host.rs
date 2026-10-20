//! Calls from the compositor thread into Kotlin (`dev.awb.NativeCallbacks`).
//! Kotlin posts each call to the main thread, so none of these block on UI.

use jni::objects::{GlobalRef, JValue};
use jni::JavaVM;

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

    /// A toplevel got its first buffer: open an Android window for `w`x`h`
    /// logical px (Kotlin applies the target display's density).
    pub fn map(&self, win: u32, w: i32, h: i32, title: &str, app_id: &str) {
        self.with_env(|env, cb| {
            let t = env.new_string(title)?;
            let a = env.new_string(app_id)?;
            env.call_method(
                cb,
                "onMap",
                "(IIILjava/lang/String;Ljava/lang/String;)V",
                &[JValue::Int(win as i32), JValue::Int(w), JValue::Int(h), JValue::Object(&t), JValue::Object(&a)],
            )?;
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

    /// The client asked for an interactive move (xdg_toplevel.move).
    pub fn move_request(&self, win: u32) {
        self.with_env(|env, cb| {
            env.call_method(cb, "onMoveRequest", "(I)V", &[JValue::Int(win as i32)])?;
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

    /// Client cursor image (RGBA as ARGB ints) or `None` to hide the pointer.
    /// `scale` maps image px to display px.
    pub fn cursor(&self, win: u32, image: Option<(&[i32], i32, i32, i32, i32, f32)>) {
        self.with_env(|env, cb| {
            match image {
                Some((argb, w, h, hx, hy, scale)) => {
                    let arr = env.new_int_array(argb.len() as i32)?;
                    env.set_int_array_region(&arr, 0, argb)?;
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
                }
                None => {
                    let null = jni::objects::JObject::null();
                    env.call_method(
                        cb,
                        "onCursor",
                        "(I[IIIIIF)V",
                        &[
                            JValue::Int(win as i32),
                            JValue::Object(&null),
                            JValue::Int(0),
                            JValue::Int(0),
                            JValue::Int(0),
                            JValue::Int(0),
                            JValue::Float(1.0),
                        ],
                    )?;
                }
            }
            Ok(())
        });
    }
}
