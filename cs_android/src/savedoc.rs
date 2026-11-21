//! The phone's document screens for the 세이브 내보내기 / 불러오기 rows
//! (`cs_core::savexfer`): this asks `CsSave.java` to open them, and the
//! answer comes back through `nativeDocResult` in `lib.rs`.
//!
//! Like [`crate::pad`], the Android API is reached over JNI from the winit
//! thread; unlike it, the class is the APK's own, which the system loader
//! cannot see from a native thread — so it is loaded through the activity's
//! class loader once, and kept. A seam that will not resolve leaves the rows
//! answering "failed" instead of taking the game down.

use cs_core::savexfer::DocRequest;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};

const CLASS: &str = "kr.chronos.sword.CsSave";

pub struct SaveDoc {
    vm: JavaVM,
    class: GlobalRef,
    /// The phone's offset from UTC when the class was resolved.
    pub utc_offset_min: i32,
}

impl SaveDoc {
    /// Resolve the class. On the winit thread, after android-activity has
    /// registered the VM and the activity with `ndk_context`.
    pub fn new() -> Option<SaveDoc> {
        let ctx = ndk_context::android_context();
        let vm = match unsafe { JavaVM::from_raw(ctx.vm().cast()) } {
            Ok(vm) => vm,
            Err(e) => {
                log::warn!("savexfer: no JavaVM ({e}); the save rows will report failure");
                return None;
            }
        };
        let activity = ctx.context();
        if activity.is_null() {
            log::warn!("savexfer: no activity; the save rows will report failure");
            return None;
        }
        let resolve = |env: &mut JNIEnv| -> jni::errors::Result<(GlobalRef, i32)> {
            // SAFETY: android-activity owns the activity reference for the
            // life of the process; this borrows it for one call.
            let activity = unsafe { JObject::from_raw(activity.cast()) };
            let loader =
                env.call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
            let name = env.new_string(CLASS)?;
            let class = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&name)],
                )?
                .l()?;
            let class = env.new_global_ref(class)?;
            let offset = env.call_static_method(&class, "utcOffsetMinutes", "()I", &[])?.i()?;
            Ok((class, offset))
        };
        // Permanently, like the pad poll: this thread outlives every use.
        let Ok(mut env) = vm.attach_current_thread_permanently() else {
            log::warn!("savexfer: cannot attach to the JavaVM; the save rows will report failure");
            return None;
        };
        match resolve(&mut env) {
            Ok((class, utc_offset_min)) => Some(SaveDoc { vm, class, utc_offset_min }),
            Err(e) => {
                let _ = env.exception_clear();
                log::warn!("savexfer: {CLASS} unavailable ({e}); the save rows will report failure");
                None
            }
        }
    }

    /// Show the player a message about the save applied at the start
    /// (`cs_core::savebundle::Outcome`). `fatal` is a start that will not boot
    /// the guest: the dialog's one button closes the app. False when the call
    /// did not reach Java.
    pub fn notice(&self, title: &str, message: &str, fatal: bool) -> bool {
        let Ok(mut env) = self.vm.attach_current_thread_permanently() else {
            return false;
        };
        let r: jni::errors::Result<bool> = (|| {
            let title = env.new_string(title)?;
            let message = env.new_string(message)?;
            env.call_static_method(
                &self.class,
                "notice",
                "(Ljava/lang/String;Ljava/lang/String;Z)Z",
                &[JValue::Object(&title), JValue::Object(&message), JValue::Bool(fatal.into())],
            )?
            .z()
        })();
        match r {
            Ok(ok) => ok,
            Err(e) => {
                let _ = env.exception_clear();
                log::warn!("savexfer: {CLASS}.notice failed ({e})");
                false
            }
        }
    }

    /// Open the screen `req` asks for. False when the call did not reach
    /// Java; the caller then answers the request as failed itself.
    pub fn request(&self, req: &DocRequest) -> bool {
        let Ok(mut env) = self.vm.attach_current_thread_permanently() else {
            return false;
        };
        let r: jni::errors::Result<bool> = (|| match req {
            DocRequest::Export { id, file, name } => {
                let path = env.new_string(file.to_string_lossy())?;
                let name = env.new_string(name)?;
                env.call_static_method(
                    &self.class,
                    "export",
                    "(Ljava/lang/String;Ljava/lang/String;I)Z",
                    &[JValue::Object(&path), JValue::Object(&name), JValue::Int(*id as i32)],
                )?
                .z()
            }
            DocRequest::Import { id, file } => {
                let path = env.new_string(file.to_string_lossy())?;
                env.call_static_method(
                    &self.class,
                    "pick",
                    "(Ljava/lang/String;I)Z",
                    &[JValue::Object(&path), JValue::Int(*id as i32)],
                )?
                .z()
            }
        })();
        match r {
            Ok(ok) => ok,
            Err(e) => {
                // A pending Java exception poisons every later JNI call on
                // the thread — the frame loop's pad poll included.
                let _ = env.exception_clear();
                log::warn!("savexfer: {CLASS} call failed ({e})");
                false
            }
        }
    }
}
