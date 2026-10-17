// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Android-specific entry points, re-exported as `gilrs_core::android` / `gilrs::android`.
//!
//! Most applications never need any of this: `Gilrs::new()` registers Activity lifecycle tracking
//! and installs a hook on the resumed activity's window callback. With an Application context,
//! initialize before the first Activity resume; an already-resumed Activity is discovered on its
//! next resume. Legacy Activity contexts are hooked immediately. The functions here cover a
//! `NativeActivity`, where input is consumed before reaching the Java window callback, or a
//! library embedded in an existing Java application.

use std::ffi::c_void;

use jni::objects::JObject;
use jni::{jni_sig, jni_str, Env, JValue};

use super::jni_glue::{self, SOURCE_CLASS_JOYSTICK};
use super::sink::{sink, RawEvent};

/// `KeyEvent.ACTION_DOWN` / `ACTION_UP`.
const ACTION_DOWN: i32 = 0;
const ACTION_UP: i32 = 1;

/// Supplies the `JavaVM` when nothing has initialized [`ndk_context`].
///
/// `android-activity` (and therefore winit and bevy) initializes it before `main`, so this is only
/// needed by hand-rolled JNI libraries. Call it before creating `Gilrs`.
///
/// # Safety
///
/// `vm` must be a valid `JavaVM` pointer that stays valid for as long as gilrs is used.
pub unsafe fn init_with_vm(vm: *mut c_void) {
    unsafe { jni_glue::set_vm(vm) }
}

/// Disables the automatic window-callback hook.
///
/// Only useful if the application intends to forward events itself, or does something to the
/// window callback that must not be wrapped. Call it before creating `Gilrs`.
pub fn set_auto_hook(enabled: bool) {
    jni_glue::set_auto_hook(enabled)
}

/// Feeds an `android.view.KeyEvent` to gilrs.
///
/// Returns whether the event came from a known gamepad. The event is only inspected, never
/// consumed — the caller remains free to handle it.
pub fn handle_key_event(env: &mut Env, event: &JObject) -> bool {
    match key_event(env, event) {
        Ok(handled) => handled,
        Err(_) => {
            clear_exception(env);
            false
        }
    }
}

/// Feeds an `android.view.MotionEvent` to gilrs.
///
/// Returns whether the event came from a known gamepad. The event is only inspected, never
/// consumed — the caller remains free to handle it.
pub fn handle_motion_event(env: &mut Env, event: &JObject) -> bool {
    match motion_event(env, event) {
        Ok(handled) => handled,
        Err(_) => {
            clear_exception(env);
            false
        }
    }
}

fn clear_exception(env: &mut Env) {
    if env.exception_check() {
        let _ = env.exception_clear();
    }
}

/// Keycodes a gamepad can plausibly produce, used only to keep diagnostics off the keyboard path.
fn is_gamepad_keycode(keycode: i32) -> bool {
    matches!(keycode, 4 | 19..=23 | 96..=110 | 188..=203)
}

fn key_event(env: &mut Env, event: &JObject) -> jni::errors::Result<bool> {
    let device_id = env
        .call_method(event, jni_str!("getDeviceId"), jni_sig!("()I"), &[])?
        .i()?;

    let action = env
        .call_method(event, jni_str!("getAction"), jni_sig!("()I"), &[])?
        .i()?;
    if action != ACTION_DOWN && action != ACTION_UP {
        return Ok(false);
    }

    let keycode = env
        .call_method(event, jni_str!("getKeyCode"), jni_sig!("()I"), &[])?
        .i()?;

    if !sink().is_known_device(device_id) {
        if is_gamepad_keycode(keycode) {
            debug!(
                "gilrs: dropping keycode {keycode} from device {device_id}, which is not a known \
                 gamepad"
            );
        }
        return Ok(false);
    }

    // Auto-repeat re-sends ACTION_DOWN; gilrs only wants edges.
    if action == ACTION_DOWN
        && env
            .call_method(event, jni_str!("getRepeatCount"), jni_sig!("()I"), &[])?
            .i()?
            > 0
    {
        return Ok(true);
    }

    sink().push(RawEvent::Key {
        device_id,
        keycode,
        down: action == ACTION_DOWN,
    });

    Ok(true)
}

fn motion_event(env: &mut Env, event: &JObject) -> jni::errors::Result<bool> {
    let device_id = env
        .call_method(event, jni_str!("getDeviceId"), jni_sig!("()I"), &[])?
        .i()?;

    let Some(axes) = sink().device_axes(device_id) else {
        // Possibly a controller that was plugged in since the last enumeration; ask the worker to
        // look again now rather than swallow input for up to a full poll interval.
        let source = env
            .call_method(event, jni_str!("getSource"), jni_sig!("()I"), &[])?
            .i()?;
        if source & SOURCE_CLASS_JOYSTICK != 0 {
            sink().request_poll();
        }
        return Ok(false);
    };

    let mut values = Vec::with_capacity(axes.len());
    for &axis in axes.iter() {
        let value = env
            .call_method(
                event,
                jni_str!("getAxisValue"),
                jni_sig!("(I)F"),
                &[JValue::Int(axis)],
            )?
            .f()?;
        values.push((axis, value));
    }

    sink().push(RawEvent::Motion { device_id, values });

    Ok(true)
}

/// Feeds an `android-activity` input event to gilrs.
///
/// Only needed with the `native-activity` backend, where the native input queue consumes events
/// before they reach the Java window callback that gilrs hooks. Under `game-activity` (what bevy
/// uses) the hook already sees everything and this is redundant.
#[cfg(feature = "android-activity")]
pub fn handle_input_event(event: &android_activity::input::InputEvent) -> bool {
    use android_activity::input::{Axis, InputEvent, KeyAction};

    match event {
        InputEvent::KeyEvent(key) => {
            let device_id = key.device_id();
            if !sink().is_known_device(device_id) {
                return false;
            }

            let down = match key.action() {
                KeyAction::Down => true,
                KeyAction::Up => false,
                _ => return false,
            };
            if down && key.repeat_count() > 0 {
                return true;
            }

            sink().push(RawEvent::Key {
                device_id,
                keycode: u32::from(key.key_code()) as i32,
                down,
            });
            true
        }
        InputEvent::MotionEvent(motion) => {
            let device_id = motion.device_id();
            let Some(axes) = sink().device_axes(device_id) else {
                sink().request_poll();
                return false;
            };
            let Some(pointer) = motion.pointers().next() else {
                return false;
            };

            let mut values = Vec::with_capacity(axes.len());
            for &axis in axes.iter() {
                values.push((axis, pointer.axis_value(Axis::from(axis as u32))));
            }

            sink().push(RawEvent::Motion { device_id, values });
            true
        }
        _ => false,
    }
}
