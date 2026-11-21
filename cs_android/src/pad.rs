//! Is a gamepad attached right now?
//!
//! The NDK has no input-device enumeration — `InputManager` is Java-only — so
//! the question is asked over JNI: `InputDevice.getDeviceIds()`, then
//! `getDevice(id).getSources()` for each, looking for a device that claims
//! `SOURCE_GAMEPAD` or `SOURCE_JOYSTICK`. This is the same walk every
//! controller-aware Android game does in Java; the only difference is doing it
//! from the native thread.
//!
//! Polled rather than event-driven: the event-driven alternative,
//! `InputManager.InputDeviceListener`, needs a Java listener object and a
//! `Handler`, i.e. a Java class this APK does not have. A poll every half
//! second is a handful of JNI calls against an in-process registry, and it
//! bounds how stale the answer can be — the HUD this feeds appears within a
//! poll of plugging a pad in and disappears within a poll of pulling it.
//!
//! Errors all fold to "no pad": a JNI failure here must never take the frame
//! loop down, and the wrong answer merely hides (or shows) a legend.

use jni::objects::{JIntArray, JObject, JValue};
use jni::JNIEnv;

/// `InputDevice.SOURCE_GAMEPAD` — the ABXY/shoulder button class.
const SOURCE_GAMEPAD: i32 = 0x0000_0401;
/// `InputDevice.SOURCE_JOYSTICK` — the analogue axes. Checked as well because
/// some controllers report only this half until a button is pressed.
const SOURCE_JOYSTICK: i32 = 0x0100_0010;

/// True while any attached input device is a gamepad or joystick.
pub fn gamepad_connected() -> bool {
    let ctx = ndk_context::android_context();
    // android-activity registered the VM before android_main ran; a null here
    // would mean we are not on Android at all.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return false;
    };
    // Permanently: this runs on the winit thread twice a second for the life
    // of the process, and the thread outlives every use of the attachment.
    let Ok(mut env) = vm.attach_current_thread_permanently() else {
        return false;
    };
    match probe(&mut env) {
        Ok(found) => found,
        Err(_) => {
            // A pending Java exception poisons every later JNI call on the
            // thread — the frame loop's asset and audio paths included.
            let _ = env.exception_clear();
            false
        }
    }
}

fn probe(env: &mut JNIEnv) -> jni::errors::Result<bool> {
    let ids: JIntArray = env
        .call_static_method("android/view/InputDevice", "getDeviceIds", "()[I", &[])?
        .l()?
        .into();
    let n = env.get_array_length(&ids)?;
    let mut buf = vec![0i32; n.max(0) as usize];
    env.get_int_array_region(&ids, 0, &mut buf)?;
    for id in buf {
        let dev = env
            .call_static_method(
                "android/view/InputDevice",
                "getDevice",
                "(I)Landroid/view/InputDevice;",
                &[JValue::from(id)],
            )?
            .l()?;
        // A device can vanish between getDeviceIds and getDevice.
        if dev.is_null() {
            continue;
        }
        let sources = env.call_method(&dev, "getSources", "()I", &[])?.i()?;
        env.delete_local_ref(JObject::from(dev))?;
        // Class membership, not bit overlap: SOURCE_JOYSTICK shares its low
        // bits with every other SOURCE_CLASS_POSITION device, so `& != 0`
        // would call a touchpad a pad.
        if sources & SOURCE_GAMEPAD == SOURCE_GAMEPAD
            || sources & SOURCE_JOYSTICK == SOURCE_JOYSTICK
        {
            return Ok(true);
        }
    }
    Ok(false)
}
