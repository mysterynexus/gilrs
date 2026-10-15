// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Everything that talks to the JVM.
//!
//! Hook initialization runs synchronously before the worker starts, so Activity lifecycle
//! observation is registered before the application's event loop can resume the Activity. Other
//! JNI work runs on the permanently attached worker, except input callbacks on the UI thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JBooleanArray, JClass, JIntArray, JObject, JString, JValue};
use jni::sys::{jclass, jobject};
use jni::{JNIEnv, JavaVM, NativeMethod};

use super::gamepad::native_ev_codes as nec;
use super::gamepad::EvCode;
use super::sink::{sink, AxisSpec, Command, DeviceInfo, RawEvent};
use crate::{AxisInfo, PowerInfo};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// `InputDevice.SOURCE_*`. The gamepad/joystick values carry extra class bits, so they have to be
/// tested with a masked equality rather than a plain `&`.
const SOURCE_GAMEPAD: i32 = 0x0000_0401;
const SOURCE_JOYSTICK: i32 = 0x0100_0010;
pub(super) const SOURCE_CLASS_JOYSTICK: i32 = 0x0000_0010;

mod keycode {
    pub const BACK: i32 = 4;
    pub const DPAD_UP: i32 = 19;
    pub const DPAD_DOWN: i32 = 20;
    pub const DPAD_LEFT: i32 = 21;
    pub const DPAD_RIGHT: i32 = 22;
    pub const DPAD_CENTER: i32 = 23;
    pub const BUTTON_A: i32 = 96;
    pub const BUTTON_B: i32 = 97;
    pub const BUTTON_C: i32 = 98;
    pub const BUTTON_X: i32 = 99;
    pub const BUTTON_Y: i32 = 100;
    pub const BUTTON_Z: i32 = 101;
    pub const BUTTON_L1: i32 = 102;
    pub const BUTTON_R1: i32 = 103;
    pub const BUTTON_L2: i32 = 104;
    pub const BUTTON_R2: i32 = 105;
    pub const BUTTON_THUMBL: i32 = 106;
    pub const BUTTON_THUMBR: i32 = 107;
    pub const BUTTON_START: i32 = 108;
    pub const BUTTON_SELECT: i32 = 109;
    pub const BUTTON_MODE: i32 = 110;
}

mod axis {
    pub const X: i32 = 0;
    pub const Y: i32 = 1;
    pub const Z: i32 = 11;
    pub const RX: i32 = 12;
    pub const RY: i32 = 13;
    pub const RZ: i32 = 14;
    pub const HAT_X: i32 = 15;
    pub const HAT_Y: i32 = 16;
    pub const LTRIGGER: i32 = 17;
    pub const RTRIGGER: i32 = 18;
    pub const GAS: i32 = 22;
    pub const BRAKE: i32 = 23;
}

/// Candidate buttons, in the order that defines `Gamepad::buttons()`.
///
/// The `optional` ones are only reported when `InputDevice.hasKeys` vouches for them. The rest are
/// always reported for a gamepad: Android guarantees the standard layout for `SOURCE_GAMEPAD`
/// devices, whereas `hasKeys` is advisory and answers `false` on plenty of devices for buttons
/// they then happily deliver events for. Believing it there leaves the pad with working sticks and
/// dead face buttons.
const BUTTONS: &[Button] = &[
    Button::always(keycode::BUTTON_A, nec::BTN_SOUTH),
    Button::always(keycode::BUTTON_B, nec::BTN_EAST),
    Button::always(keycode::BUTTON_X, nec::BTN_WEST),
    Button::always(keycode::BUTTON_Y, nec::BTN_NORTH),
    Button::optional(keycode::BUTTON_C, nec::BTN_C),
    Button::optional(keycode::BUTTON_Z, nec::BTN_Z),
    Button::always(keycode::BUTTON_L1, nec::BTN_LT),
    Button::always(keycode::BUTTON_R1, nec::BTN_RT),
    Button::always(keycode::BUTTON_L2, nec::BTN_LT2),
    Button::always(keycode::BUTTON_R2, nec::BTN_RT2),
    Button::always(keycode::BUTTON_THUMBL, nec::BTN_LTHUMB),
    Button::always(keycode::BUTTON_THUMBR, nec::BTN_RTHUMB),
    Button::always(keycode::BUTTON_SELECT, nec::BTN_SELECT),
    Button::always(keycode::BUTTON_START, nec::BTN_START),
    Button::always(keycode::BUTTON_MODE, nec::BTN_MODE),
    Button::always(keycode::DPAD_UP, nec::BTN_DPAD_UP),
    Button::always(keycode::DPAD_DOWN, nec::BTN_DPAD_DOWN),
    Button::always(keycode::DPAD_LEFT, nec::BTN_DPAD_LEFT),
    Button::always(keycode::DPAD_RIGHT, nec::BTN_DPAD_RIGHT),
];

struct Button {
    keycode: i32,
    code: EvCode,
    optional: bool,
}

impl Button {
    const fn always(keycode: i32, code: EvCode) -> Self {
        Button {
            keycode,
            code,
            optional: false,
        }
    }

    const fn optional(keycode: i32, code: EvCode) -> Self {
        Button {
            keycode,
            code,
            optional: true,
        }
    }
}

static DEX: &[u8] = include_bytes!("gilrs_hook.dex");

static VM: OnceLock<JavaVM> = OnceLock::new();
static AUTO_HOOK: AtomicBool = AtomicBool::new(true);
/// Set when the application supplied the VM itself, in which case `ndk_context` is never
/// consulted — it panics when uninitialized, and that would abort under `panic = "abort"`.
static MANUAL_VM: AtomicBool = AtomicBool::new(false);

pub(super) fn set_auto_hook(enabled: bool) {
    AUTO_HOOK.store(enabled, Ordering::Relaxed);
}

/// # Safety
///
/// `ptr` must be a valid `JavaVM*` that outlives the process' use of gilrs.
pub(super) unsafe fn set_vm(ptr: *mut c_void) {
    if unsafe { store_vm(ptr) } {
        MANUAL_VM.store(true, Ordering::Relaxed);
    }
}

/// Caches the VM without claiming the application supplied it.
///
/// Keeping this separate matters: the VM we pull out of `ndk_context` ourselves must not mark the
/// VM as application-supplied, or `android_context` would refuse to look up the activity from then
/// on and the input hook would never be installed.
///
/// # Safety
///
/// `ptr` must be a valid `JavaVM*` that outlives the process' use of gilrs.
unsafe fn store_vm(ptr: *mut c_void) -> bool {
    if ptr.is_null() {
        return false;
    }
    match unsafe { JavaVM::from_raw(ptr.cast()) } {
        Ok(vm) => VM.set(vm).is_ok(),
        Err(_) => false,
    }
}

/// `ndk_context::android_context` panics when nothing has initialized it. There is no fallible
/// accessor, so the call is wrapped; `android-activity` always initializes it before `main`, so
/// this only trips in hand-rolled JNI libraries, which should call `init_with_vm` instead.
fn android_context() -> Option<ndk_context::AndroidContext> {
    if MANUAL_VM.load(Ordering::Relaxed) {
        return None;
    }
    std::panic::catch_unwind(ndk_context::android_context).ok()
}

fn vm() -> Option<&'static JavaVM> {
    if VM.get().is_none() {
        if let Some(ctx) = android_context() {
            unsafe { store_vm(ctx.vm()) };
        }
    }
    VM.get()
}

pub(super) fn start() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();

    STARTED
        .get_or_init(|| {
            let vm = vm().ok_or_else(|| {
                "no JavaVM available; the Android context was not initialized and \
                 gilrs::android::init_with_vm() was not called"
                    .to_owned()
            })?;
            let mut worker = Worker {
                hook: None,
                known: HashMap::new(),
                battery: HashMap::new(),
                vibrators: HashMap::new(),
                context_ptr: 0,
                hook_attempts: 0,
                warned_no_context: false,
                passes: 0,
                sdk_int: 0,
            };

            if AUTO_HOOK.load(Ordering::Relaxed) {
                match vm.attach_current_thread() {
                    Ok(mut env) => {
                        let result = env.with_local_frame::<_, (), jni::errors::Error>(32, |env| {
                            worker.sdk_int = read_sdk_int(env);
                            maybe_install_hook(env, &mut worker);
                            Ok(())
                        });
                        if let Err(error) = result {
                            warn!("gilrs: failed to initialize the Android input hook: {error}");
                            clear_exception(&mut env);
                        }
                    }
                    Err(error) => {
                        warn!("gilrs: failed to attach for Android input hook setup: {error}");
                    }
                }
            }

            std::thread::Builder::new()
                .name("gilrs-android".to_owned())
                .spawn(move || thread_main(worker))
                .map(|_| ())
                .map_err(|e| format!("failed to spawn the gilrs Android thread: {e}"))
        })
        .clone()
}

/// Battery is polled every Nth pass; it changes far more slowly than the device list.
const BATTERY_EVERY: u32 = 8;

struct Worker {
    hook: Option<GlobalRef>,
    known: HashMap<i32, Arc<DeviceInfo>>,
    battery: HashMap<i32, PowerInfo>,
    /// Cached per device so that sustaining a rumble does not re-resolve the device and its
    /// vibrator on every tick.
    vibrators: HashMap<i32, GlobalRef>,
    /// Used only to notice context replacement in older android-activity releases. Activity
    /// references themselves belong to the Java lifecycle observer, which releases them on destroy.
    context_ptr: usize,
    hook_attempts: u32,
    warned_no_context: bool,
    passes: u32,
    sdk_int: i32,
}

const MAX_HOOK_ATTEMPTS: u32 = 3;

fn thread_main(mut worker: Worker) {
    let Some(vm) = vm() else { return };
    let mut env = match vm.attach_current_thread_permanently() {
        Ok(env) => env,
        Err(e) => {
            error!("gilrs: failed to attach the Android worker thread: {e}");
            return;
        }
    };

    if worker.sdk_int == 0 {
        worker.sdk_int = read_sdk_int(&mut env);
    }

    let mut last_poll = Instant::now() - POLL_INTERVAL;
    loop {
        let commands = sink().take_commands();
        let due = last_poll.elapsed() >= POLL_INTERVAL;

        // This thread is attached permanently and never returns to Java, so JNI local references
        // it creates are never reclaimed on their own. Without a frame per iteration the local
        // reference table fills up and the runtime aborts the process after a few minutes.
        let result = env.with_local_frame::<_, (), jni::errors::Error>(64, |env| {
            for command in &commands {
                if let Err(e) = run_command(env, &mut worker, command) {
                    warn!("gilrs: force feedback command failed: {e}");
                    clear_exception(env);
                }
            }

            if due {
                if let Err(e) = enumerate(env, &mut worker) {
                    warn!("gilrs: failed to enumerate Android input devices: {e}");
                    clear_exception(env);
                }
                refresh_context(env, &mut worker);
                maybe_install_hook(env, &mut worker);
                ensure_hook(env, &worker);
            }

            Ok(())
        });
        if let Err(e) = result {
            warn!("gilrs: Android worker iteration failed: {e}");
            clear_exception(&mut env);
        }

        if due {
            last_poll = Instant::now();
            sink().signal_first_pass();
        }

        sink().wait_poll(POLL_INTERVAL.saturating_sub(last_poll.elapsed()));
    }
}

/// Older android-activity releases replace their Activity context on recreation. New releases
/// retain an Application context, with Activity changes handled by the Java lifecycle observer.
fn refresh_context(env: &mut JNIEnv, worker: &mut Worker) {
    let Some(ctx) = android_context().filter(|ctx| !ctx.context().is_null()) else {
        return;
    };

    let ptr = ctx.context() as usize;
    if ptr == worker.context_ptr {
        return;
    }

    if let Some(hook) = &worker.hook {
        let context = unsafe { JObject::from_raw(ctx.context() as jobject) };
        let result = env
            .new_local_ref(&context)
            .and_then(|context| initialize_hook(env, hook, &context));
        if let Err(error) = result {
            warn!("gilrs: failed to update the Android input hook context: {error}");
            clear_exception(env);
            return;
        }
    }

    worker.context_ptr = ptr;
    worker.hook_attempts = 0;
}

fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

fn sdk_int(env: &mut JNIEnv) -> jni::errors::Result<i32> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
        .i()
}

fn read_sdk_int(env: &mut JNIEnv) -> i32 {
    match sdk_int(env) {
        Ok(version) => version,
        Err(error) => {
            warn!("gilrs: failed to read the Android API level: {error}");
            clear_exception(env);
            0
        }
    }
}

// -- device enumeration ---------------------------------------------------------------------

fn enumerate(env: &mut JNIEnv, worker: &mut Worker) -> jni::errors::Result<()> {
    let ids = env
        .call_static_method("android/view/InputDevice", "getDeviceIds", "()[I", &[])?
        .l()?;
    let ids = JIntArray::from(ids);
    let len = env.get_array_length(&ids)? as usize;
    let mut present = vec![0i32; len];
    env.get_int_array_region(&ids, 0, &mut present)?;

    let removed: Vec<i32> = worker
        .known
        .keys()
        .copied()
        .filter(|id| !present.contains(id))
        .collect();
    for id in removed {
        worker.known.remove(&id);
        worker.battery.remove(&id);
        worker.vibrators.remove(&id);
        sink().remove_device(id);
    }

    worker.passes = worker.passes.wrapping_add(1);
    let check_battery = worker.passes % BATTERY_EVERY == 1;

    for id in present {
        if worker.known.contains_key(&id) {
            if check_battery {
                match battery(env, worker.sdk_int, id) {
                    // Only report transitions; otherwise every poll would wake a caller blocked
                    // in next_event_blocking for no reason.
                    Ok(Some(info)) if worker.battery.get(&id) != Some(&info) => {
                        worker.battery.insert(id, info);
                        sink().push(RawEvent::Battery {
                            device_id: id,
                            info,
                        });
                    }
                    Ok(_) => (),
                    Err(_) => clear_exception(env),
                }
            }
            continue;
        }

        let device = env.with_local_frame(64, |env| read_device(env, worker.sdk_int, id))?;
        if let Some(device) = device {
            let device = Arc::new(device);
            info!(
                "Gamepad {} ({}) connected with {} buttons and {} axes.",
                id,
                device.name,
                device.buttons.len(),
                device.axes.len()
            );
            debug!(
                "Gamepad {}: vendor: {:?}, product: {:?}, ff_supported: {}, keycode map: {:?}, \
                 axes: {:?}",
                id,
                device.vendor_id,
                device.product_id,
                device.has_vibrator,
                device.key_map,
                device
                    .axes
                    .iter()
                    .map(|a| (a.android, a.code, a.min, a.signed))
                    .collect::<Vec<_>>()
            );
            worker.known.insert(id, device.clone());
            sink().add_device(device);
        }
    }

    Ok(())
}

fn read_device(env: &mut JNIEnv, sdk_int: i32, id: i32) -> jni::errors::Result<Option<DeviceInfo>> {
    let device = env
        .call_static_method(
            "android/view/InputDevice",
            "getDevice",
            "(I)Landroid/view/InputDevice;",
            &[JValue::Int(id)],
        )?
        .l()?;
    if device.is_null() {
        return Ok(None);
    }

    let sources = env.call_method(&device, "getSources", "()I", &[])?.i()?;
    if sources & SOURCE_GAMEPAD != SOURCE_GAMEPAD && sources & SOURCE_JOYSTICK != SOURCE_JOYSTICK {
        return Ok(None);
    }

    let name: JString = env
        .call_method(&device, "getName", "()Ljava/lang/String;", &[])?
        .l()?
        .into();
    let name = env
        .get_string(&name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let vendor_id = env.call_method(&device, "getVendorId", "()I", &[])?.i()?;
    let product_id = env.call_method(&device, "getProductId", "()I", &[])?.i()?;

    let axes = read_axes(env, &device)?;
    let (key_map, buttons) = read_buttons(env, &device, &axes)?;
    if buttons.is_empty() && axes.is_empty() {
        return Ok(None);
    }

    Ok(Some(DeviceInfo {
        device_id: id,
        name,
        vendor_id: (vendor_id != 0).then_some(vendor_id as u16),
        product_id: (product_id != 0).then_some(product_id as u16),
        key_map,
        buttons,
        axis_codes: axes.iter().map(|a| a.code).collect(),
        axes,
        has_vibrator: has_vibrator(env, &device, sdk_int).unwrap_or(false),
    }))
}

/// Reads the device's motion ranges and assigns them to gilrs axes, applying the fallbacks
/// documented by Android for controllers that report triggers or the right stick on alternate
/// axes.
fn read_axes(env: &mut JNIEnv, device: &JObject) -> jni::errors::Result<Vec<AxisSpec>> {
    let ranges = env
        .call_method(device, "getMotionRanges", "()Ljava/util/List;", &[])?
        .l()?;
    let count = env.call_method(&ranges, "size", "()I", &[])?.i()?;

    // android axis -> (min, max, flat)
    let mut available: HashMap<i32, (f32, f32, f32)> = HashMap::new();
    for i in 0..count {
        let range = env
            .call_method(&ranges, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
            .l()?;
        let source = env.call_method(&range, "getSource", "()I", &[])?.i()?;
        if source & SOURCE_CLASS_JOYSTICK == 0 {
            continue;
        }
        let axis = env.call_method(&range, "getAxis", "()I", &[])?.i()?;
        let min = env.call_method(&range, "getMin", "()F", &[])?.f()?;
        let max = env.call_method(&range, "getMax", "()F", &[])?.f()?;
        let flat = env.call_method(&range, "getFlat", "()F", &[])?.f()?;
        available.insert(axis, (min, max, flat));
    }

    let pick = |candidates: &[i32]| -> Option<i32> {
        candidates
            .iter()
            .copied()
            .find(|a| available.contains_key(a))
    };

    let assignment = [
        (pick(&[axis::X]), nec::AXIS_LSTICKX),
        (pick(&[axis::Y]), nec::AXIS_LSTICKY),
        (pick(&[axis::Z, axis::RX]), nec::AXIS_RSTICKX),
        (pick(&[axis::RZ, axis::RY]), nec::AXIS_RSTICKY),
        (pick(&[axis::LTRIGGER, axis::BRAKE]), nec::AXIS_LT2),
        (pick(&[axis::RTRIGGER, axis::GAS]), nec::AXIS_RT2),
        (pick(&[axis::HAT_X]), nec::AXIS_DPADX),
        (pick(&[axis::HAT_Y]), nec::AXIS_DPADY),
    ];

    let mut axes = Vec::new();
    for (android, code) in assignment {
        let Some(android) = android else { continue };
        let (min, max, flat) = available[&android];
        if max <= min {
            continue;
        }

        let inv_range = 1.0 / (max - min);
        let signed = min < 0.0;

        // `flat` is in device units. gilrs turns AxisInfo::deadzone back into a threshold with
        // `deadzone / (max - min) * 2`, so express it as a fraction of the axis' normalized span
        // and then undo that. Android commonly reports a flat of 0, in which case leaving this
        // as None lets gilrs apply its own default.
        let fraction = (flat * inv_range) as f64 * if signed { 2.0 } else { 1.0 };
        let min_scaled = if signed { i32::MIN } else { 0 };
        let deadzone = (fraction * (i32::MAX as f64 - min_scaled as f64) / 2.0) as u32;

        axes.push(AxisSpec {
            android,
            code,
            info: AxisInfo {
                min: min_scaled,
                max: i32::MAX,
                deadzone: (deadzone > 0).then_some(deadzone),
            },
            min,
            inv_range,
            signed,
        });
    }

    Ok(axes)
}

/// A device's keycode -> code table, and the deduplicated codes it can report.
type Buttons = (Vec<(i32, EvCode)>, Vec<EvCode>);

fn read_buttons(
    env: &mut JNIEnv,
    device: &JObject,
    axes: &[AxisSpec],
) -> jni::errors::Result<Buttons> {
    let codes: Vec<i32> = BUTTONS.iter().map(|b| b.keycode).collect();
    let query = env.new_int_array(codes.len() as i32)?;
    env.set_int_array_region(&query, 0, &codes)?;
    let result = env
        .call_method(device, "hasKeys", "([I)[Z", &[JValue::Object(&query)])?
        .l()?;
    let result = JBooleanArray::from(result);
    let mut has = vec![0u8; codes.len()];
    env.get_boolean_array_region(&result, 0, &mut has)?;

    // A device that reports a hat also has Android synthesize KEYCODE_DPAD_* for it. Keeping both
    // would fire every d-pad press twice, so let the hat axes win; gilrs' axis_dpad_to_button
    // filter turns them back into buttons. Both halves of the hat are required, otherwise
    // dropping the keys would lose a direction outright.
    let has_hat = axes.iter().any(|a| a.code == nec::AXIS_DPADX)
        && axes.iter().any(|a| a.code == nec::AXIS_DPADY);

    let is_dpad = |code: EvCode| {
        matches!(
            code,
            nec::BTN_DPAD_UP | nec::BTN_DPAD_DOWN | nec::BTN_DPAD_LEFT | nec::BTN_DPAD_RIGHT
        )
    };
    let reported = |keycode: i32| {
        BUTTONS
            .iter()
            .position(|b| b.keycode == keycode)
            .is_some_and(|i| has[i] != 0)
    };

    let mut key_map: Vec<(i32, EvCode)> = Vec::with_capacity(BUTTONS.len());
    for (index, button) in BUTTONS.iter().enumerate() {
        if has_hat && is_dpad(button.code) {
            continue;
        }
        if button.optional && has[index] == 0 {
            continue;
        }
        key_map.push((button.keycode, button.code));
    }

    // Documented Android quirks: controllers without a dedicated B report it as KEYCODE_BACK, and
    // TV-style remotes use KEYCODE_DPAD_CENTER where a gamepad would use A. Only worth applying
    // when hasKeys is credible enough to claim the real button is missing, which needs it to have
    // vouched for something.
    if has.iter().any(|&present| present != 0) {
        if !reported(keycode::BUTTON_B) {
            key_map.push((keycode::BACK, nec::BTN_EAST));
        }
        if !reported(keycode::BUTTON_A) {
            key_map.push((keycode::DPAD_CENTER, nec::BTN_SOUTH));
        }
    }

    let mut buttons: Vec<EvCode> = Vec::with_capacity(key_map.len());
    for (_, code) in &key_map {
        if !buttons.contains(code) {
            buttons.push(*code);
        }
    }

    Ok((key_map, buttons))
}

// -- battery and rumble ---------------------------------------------------------------------

fn battery(env: &mut JNIEnv, sdk_int: i32, id: i32) -> jni::errors::Result<Option<PowerInfo>> {
    if sdk_int < 31 {
        return Ok(None);
    }

    env.with_local_frame(16, |env| {
        let device = env
            .call_static_method(
                "android/view/InputDevice",
                "getDevice",
                "(I)Landroid/view/InputDevice;",
                &[JValue::Int(id)],
            )?
            .l()?;
        if device.is_null() {
            return Ok(None);
        }

        let state = env
            .call_method(
                &device,
                "getBatteryState",
                "()Landroid/hardware/BatteryState;",
                &[],
            )?
            .l()?;
        if state.is_null() {
            return Ok(None);
        }

        if !env.call_method(&state, "isPresent", "()Z", &[])?.z()? {
            return Ok(Some(PowerInfo::Wired));
        }

        let capacity = env.call_method(&state, "getCapacity", "()F", &[])?.f()?;
        let status = env.call_method(&state, "getStatus", "()I", &[])?.i()?;
        let percent = (capacity.clamp(0.0, 1.0) * 100.0) as u8;

        // android.os.BatteryManager.BATTERY_STATUS_*
        Ok(Some(match status {
            2 => PowerInfo::Charging(percent),
            3 | 4 => PowerInfo::Discharging(percent),
            5 => PowerInfo::Charged,
            _ => PowerInfo::Unknown,
        }))
    })
}

fn vibrator<'local>(
    env: &mut JNIEnv<'local>,
    device: &JObject,
    sdk_int: i32,
) -> jni::errors::Result<JObject<'local>> {
    if sdk_int >= 31 {
        let manager = env
            .call_method(
                device,
                "getVibratorManager",
                "()Landroid/os/VibratorManager;",
                &[],
            )?
            .l()?;
        if !manager.is_null() {
            return env
                .call_method(
                    &manager,
                    "getDefaultVibrator",
                    "()Landroid/os/Vibrator;",
                    &[],
                )?
                .l();
        }
    }

    env.call_method(device, "getVibrator", "()Landroid/os/Vibrator;", &[])?
        .l()
}

fn has_vibrator(env: &mut JNIEnv, device: &JObject, sdk_int: i32) -> jni::errors::Result<bool> {
    let vibrator = vibrator(env, device, sdk_int)?;
    if vibrator.is_null() {
        return Ok(false);
    }
    env.call_method(&vibrator, "hasVibrator", "()Z", &[])?.z()
}

/// Resolves and caches a device's vibrator, so a sustained rumble does not re-resolve it on every
/// force feedback tick.
fn cached_vibrator(
    env: &mut JNIEnv,
    worker: &mut Worker,
    device_id: i32,
) -> jni::errors::Result<Option<GlobalRef>> {
    if let Some(vibrator) = worker.vibrators.get(&device_id) {
        return Ok(Some(vibrator.clone()));
    }

    let sdk_int = worker.sdk_int;
    let vibrator = env.with_local_frame::<_, Option<GlobalRef>, jni::errors::Error>(16, |env| {
        let device = env
            .call_static_method(
                "android/view/InputDevice",
                "getDevice",
                "(I)Landroid/view/InputDevice;",
                &[JValue::Int(device_id)],
            )?
            .l()?;
        if device.is_null() {
            return Ok(None);
        }

        let vibrator = vibrator(env, &device, sdk_int)?;
        if vibrator.is_null() {
            return Ok(None);
        }

        env.new_global_ref(&vibrator).map(Some)
    })?;

    if let Some(vibrator) = &vibrator {
        worker.vibrators.insert(device_id, vibrator.clone());
    }

    Ok(vibrator)
}

fn run_command(
    env: &mut JNIEnv,
    worker: &mut Worker,
    command: &Command,
) -> jni::errors::Result<()> {
    let Command::Rumble {
        device_id,
        amplitude,
        duration_ms,
    } = command;

    let Some(vibrator) = cached_vibrator(env, worker, *device_id)? else {
        return Ok(());
    };
    let sdk_int = worker.sdk_int;

    env.with_local_frame(16, |env| {
        let vibrator = vibrator.as_obj();

        if *amplitude == 0 || *duration_ms <= 0 {
            return env.call_method(vibrator, "cancel", "()V", &[]).map(|_| ());
        }

        if sdk_int >= 26 {
            let effect = env
                .call_static_method(
                    "android/os/VibrationEffect",
                    "createOneShot",
                    "(JI)Landroid/os/VibrationEffect;",
                    &[JValue::Long(*duration_ms), JValue::Int(*amplitude)],
                )?
                .l()?;
            env.call_method(
                vibrator,
                "vibrate",
                "(Landroid/os/VibrationEffect;)V",
                &[JValue::Object(&effect)],
            )?;
        } else {
            env.call_method(vibrator, "vibrate", "(J)V", &[JValue::Long(*duration_ms)])?;
        }

        Ok(())
    })
}

// -- Window.Callback hook -------------------------------------------------------------------

/// Loads the hook once, retrying a bounded number of times.
///
/// Lifecycle observation starts before the first Activity resume; a cap keeps permanent setup
/// failures from warning on every poll. Java retries window installation once an Activity exists.
fn maybe_install_hook(env: &mut JNIEnv, worker: &mut Worker) {
    if worker.hook.is_some()
        || worker.hook_attempts >= MAX_HOOK_ATTEMPTS
        || !AUTO_HOOK.load(Ordering::Relaxed)
    {
        return;
    }

    let Some(context) = android_context().filter(|context| !context.context().is_null()) else {
        // Enumeration only needs the VM, so gamepads still show up here while input silently
        // never arrives. Say something rather than leave that looking like working support.
        if !worker.warned_no_context {
            worker.warned_no_context = true;
            warn!(
                "gilrs: no Android Context available, so gamepads will be detected but report no \
                 input. Applications that do not use android-activity should call \
                 gilrs::android::init_with_vm and forward input events themselves."
            );
        }
        return;
    };

    worker.hook_attempts += 1;
    worker.context_ptr = context.context() as usize;
    let context = unsafe { JObject::from_raw(context.context() as jobject) };
    match env.new_local_ref(&context) {
        Ok(context) => install_hook(env, worker, &context),
        Err(error) => {
            warn!("gilrs: failed to retain the Android input hook context: {error}");
            clear_exception(env);
        }
    }
}

fn install_hook(env: &mut JNIEnv, worker: &mut Worker, context: &JObject) {
    debug!(
        "gilrs: installing the Android input hook (attempt {}, sdk {})",
        worker.hook_attempts, worker.sdk_int
    );

    let is_context = env
        .is_instance_of(context, "android/content/Context")
        .unwrap_or(false);

    if !is_context {
        clear_exception(env);
        warn!(
            "gilrs: the Android context is not a Context, gamepad input events will only be seen \
             if the application forwards them via gilrs::android"
        );
        return;
    }

    // InMemoryDexClassLoader is API 26. Checked rather than caught, so that older devices do not
    // take a NoClassDefFoundError on every attempt.
    if worker.sdk_int < 26 {
        warn!(
            "gilrs: the Android input hook needs API 26 or newer (this device reports {}); \
             gamepad input events must be forwarded via gilrs::android",
            worker.sdk_int
        );
        return;
    }

    match load_hook(env, context) {
        Ok(hook) => {
            info!("gilrs: Android input hook initialized with Activity lifecycle tracking.");
            worker.hook = Some(hook);
            ensure_hook(env, worker);
        }
        Err(e) => {
            warn!(
                "gilrs: could not install the Android input hook ({e}); gamepads will still be \
                 detected, but input events must be forwarded via gilrs::android"
            );
            clear_exception(env);
        }
    }
}

fn load_hook(env: &mut JNIEnv, context: &JObject) -> jni::errors::Result<GlobalRef> {
    env.with_local_frame::<_, GlobalRef, jni::errors::Error>(32, |env| {
        load_hook_inner(env, context)
    })
}

fn load_hook_inner(env: &mut JNIEnv, context: &JObject) -> jni::errors::Result<GlobalRef> {
    let dex = env.byte_array_from_slice(DEX)?;
    let buffer = env
        .call_static_method(
            "java/nio/ByteBuffer",
            "wrap",
            "([B)Ljava/nio/ByteBuffer;",
            &[JValue::Object(&dex)],
        )?
        .l()?;

    let parent = env
        .call_method(context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;

    let loader = env.new_object(
        "dalvik/system/InMemoryDexClassLoader",
        "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
        &[JValue::Object(&buffer), JValue::Object(&parent)],
    )?;

    let name = env.new_string("org.gilrs.GilrsHook")?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = JClass::from(class);

    env.register_native_methods(
        &class,
        &[
            NativeMethod {
                name: "nativeKeyEvent".into(),
                sig: "(Landroid/view/KeyEvent;)V".into(),
                fn_ptr: native_key_event as *mut c_void,
            },
            NativeMethod {
                name: "nativeMotionEvent".into(),
                sig: "(Landroid/view/MotionEvent;)V".into(),
                fn_ptr: native_motion_event as *mut c_void,
            },
        ],
    )?;

    let hook = env.new_global_ref(&class)?;
    initialize_hook(env, &hook, context)?;
    Ok(hook)
}

fn initialize_hook(
    env: &mut JNIEnv,
    hook: &GlobalRef,
    context: &JObject,
) -> jni::errors::Result<()> {
    let class = <&JClass>::from(hook.as_obj());
    env.call_static_method(
        class,
        "initialize",
        "(Landroid/content/Context;)V",
        &[JValue::Object(context)],
    )?;
    Ok(())
}

/// Reinstalls the hook if something else replaced the window callback in the meantime; appcompat
/// and friends do exactly that. `ensureInstalled` is a no-op when we are already in place.
fn ensure_hook(env: &mut JNIEnv, worker: &Worker) {
    let Some(hook) = &worker.hook else {
        return;
    };

    let class = <&JClass>::from(hook.as_obj());
    let result = env.call_static_method(class, "ensureInstalled", "()V", &[]);
    if let Err(e) = result {
        warn!("gilrs: GilrsHook.ensureInstalled failed: {e}");
        clear_exception(env);
    }
}

// -- native callbacks (UI thread) -----------------------------------------------------------

unsafe extern "C" fn native_key_event(env: *mut jni::sys::JNIEnv, _class: jclass, event: jobject) {
    let Ok(mut env) = (unsafe { JNIEnv::from_raw(env) }) else {
        return;
    };
    let event = unsafe { JObject::from_raw(event) };
    super::api::handle_key_event(&mut env, &event);
}

unsafe extern "C" fn native_motion_event(
    env: *mut jni::sys::JNIEnv,
    _class: jclass,
    event: jobject,
) {
    let Ok(mut env) = (unsafe { JNIEnv::from_raw(env) }) else {
        return;
    };
    let event = unsafe { JObject::from_raw(event) };
    super::api::handle_motion_event(&mut env, &event);
}
