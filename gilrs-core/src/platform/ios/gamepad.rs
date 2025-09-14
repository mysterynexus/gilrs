// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
#![allow(unused_variables)]

use super::FfDevice;
use crate::{AxisInfo, Event, EventType, PlatformError, PowerInfo};
use uuid::Uuid;

use std::collections::VecDeque;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::Duration;

// Ensure linking of GameController.framework; we'll start discovery via ObjC message send.
use block2::StackBlock as ConcreteBlock;
use dispatch::Queue;
use objc2::ffi::sel_registerName;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send, sel};
use objc2_foundation::NSString;
#[allow(unused_imports)]
use objc2_game_controller as _gc;
use std::ffi::CStr;
use std::sync::{Arc, Mutex};

#[inline]
fn is_main_thread() -> bool {
    unsafe {
        if let Some(cls) = AnyClass::get(c"NSThread") {
            let is_main: bool = msg_send![cls, isMainThread];
            is_main
        } else {
            false
        }
    }
}

// Snapshot of controller state captured on the main thread
#[derive(Debug)]
struct Snapshot {
    addr: usize,
    name: Option<String>,
    buttons: [bool; BUTTONS.len()],
    axes: [i32; AXES.len()],
}

#[derive(Debug)]
pub struct Gilrs {
    events: VecDeque<Event>,
    gamepads: Vec<Gamepad>,
    snapshot_buf: Arc<Mutex<Option<Vec<Snapshot>>>>,
    // buffered (controller_addr, event_type) emitted from main-thread handlers
    event_buf: Arc<Mutex<Vec<(usize, EventType)>>>,
}

impl Gilrs {
    // Run on main with no return value.
    fn run_on_main<F: FnOnce() + Send + 'static>(&self, f: F) {
        if is_main_thread() {
            f();
        } else {
            Queue::main().exec_async(f);
        }
    }

    // Post work on main without blocking. Use when main queue might not be running yet.
    fn post_on_main<F: FnOnce() + Send + 'static>(&self, f: F) {
        Queue::main().exec_async(f);
    }

    // Run on main and return a value (looser bounds for callers inside &mut self methods).
    fn run_on_main_capture<R: Send + 'static, F: FnOnce() -> R + Send>(&self, f: F) -> R {
        if is_main_thread() {
            f()
        } else {
            use std::sync::{Arc, Mutex};
            let slot: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));
            let slot2 = slot.clone();
            Queue::main().exec_sync(move || {
                let r = f();
                *slot2.lock().unwrap() = Some(r);
            });
            Arc::try_unwrap(slot)
                .ok()
                .unwrap()
                .into_inner()
                .unwrap()
                .take()
                .unwrap()
        }
    }

    unsafe fn install_handlers(
        ctrl: *mut AnyObject,
        event_buf: &Arc<Mutex<Vec<(usize, EventType)>>>,
    ) {
        // Extended profile handler
        let ext: *mut AnyObject = msg_send![ctrl, extendedGamepad];
        if !ext.is_null() {
            // valueChangedHandler: (GCExtendedGamepad*, GCControllerElement*) -> void
            let eb = event_buf.clone();
            let block = ConcreteBlock::new(move |gp: *mut AnyObject, elem: *mut AnyObject| {
                unsafe {
                    // Compute controller address from owning GCController of this profile
                    let ctrl: *mut AnyObject = msg_send![gp, controller];
                    let ctrl_addr = ctrl as usize;
                    let mut push = |et: EventType| {
                        if let Ok(mut b) = eb.lock() {
                            b.push((ctrl_addr, et));
                        }
                    };
                    // Buttons
                    let bmap: [(*const i8, crate::EvCode); 10] = [
                        (sel!(buttonA).name().as_ptr(), crate::EvCode(BUTTONS[0])),
                        (sel!(buttonB).name().as_ptr(), crate::EvCode(BUTTONS[1])),
                        (sel!(buttonX).name().as_ptr(), crate::EvCode(BUTTONS[2])),
                        (sel!(buttonY).name().as_ptr(), crate::EvCode(BUTTONS[3])),
                        (
                            sel!(leftShoulder).name().as_ptr(),
                            crate::EvCode(BUTTONS[4]),
                        ),
                        (
                            sel!(rightShoulder).name().as_ptr(),
                            crate::EvCode(BUTTONS[5]),
                        ),
                        (std::ptr::null(), crate::EvCode(BUTTONS[6])),
                        (std::ptr::null(), crate::EvCode(BUTTONS[7])),
                        (std::ptr::null(), crate::EvCode(BUTTONS[8])),
                        (std::ptr::null(), crate::EvCode(BUTTONS[9])),
                    ];
                    for (sel_ptr, code) in bmap.iter() {
                        if sel_ptr.is_null() {
                            continue;
                        }
                        let btn: *mut AnyObject =
                            msg_send![gp, performSelector: sel_registerName(*sel_ptr)];
                        if btn == elem {
                            let pressed: bool = msg_send![btn, isPressed];
                            push(if pressed {
                                EventType::ButtonPressed(*code)
                            } else {
                                EventType::ButtonReleased(*code)
                            });
                            return;
                        }
                    }
                    let read_axis = |a: *mut AnyObject| -> i32 {
                        if a.is_null() {
                            0
                        } else {
                            let v: f32 = msg_send![a, value];
                            (v as f64 * i32::MAX as f64) as i32
                        }
                    };
                    let ls: *mut AnyObject = msg_send![gp, leftThumbstick];
                    let rs: *mut AnyObject = msg_send![gp, rightThumbstick];
                    let dpad: *mut AnyObject = msg_send![gp, dpad];
                    let candidates: [(*mut AnyObject, crate::EvCode); 8] = [
                        (msg_send![ls, xAxis], crate::EvCode(AXES[0])),
                        (msg_send![ls, yAxis], crate::EvCode(AXES[1])),
                        (msg_send![rs, xAxis], crate::EvCode(AXES[2])),
                        (msg_send![rs, yAxis], crate::EvCode(AXES[3])),
                        (msg_send![dpad, xAxis], crate::EvCode(AXES[4])),
                        (msg_send![dpad, yAxis], crate::EvCode(AXES[5])),
                        (msg_send![gp, leftTrigger], crate::EvCode(AXES[6])),
                        (msg_send![gp, rightTrigger], crate::EvCode(AXES[7])),
                    ];
                    for (axis, code) in candidates.iter() {
                        if *axis == elem {
                            let val = read_axis(*axis);
                            push(EventType::AxisValueChanged(val, *code));
                            return;
                        }
                    }
                }
            })
            .copy();
            let _: () = msg_send![ext, setValueChangedHandler: &*block];
        }

        // Micro profile handler
        let micro: *mut AnyObject = msg_send![ctrl, microGamepad];
        if !micro.is_null() {
            let eb = event_buf.clone();
            let _: () = msg_send![micro, setReportsAbsoluteDpadValues: true];
            let block =
                ConcreteBlock::new(move |mgp: *mut AnyObject, elem: *mut AnyObject| unsafe {
                    let ctrl: *mut AnyObject = msg_send![mgp, controller];
                    let ctrl_addr = ctrl as usize;
                    let mut push = |et: EventType| {
                        if let Ok(mut b) = eb.lock() {
                            b.push((ctrl_addr, et));
                        }
                    };
                    let b_a: *mut AnyObject = msg_send![mgp, buttonA];
                    let b_x: *mut AnyObject = msg_send![mgp, buttonX];
                    let dpad: *mut AnyObject = msg_send![mgp, dpad];
                    let d_up: *mut AnyObject = msg_send![dpad, up];
                    let d_down: *mut AnyObject = msg_send![dpad, down];
                    let d_left: *mut AnyObject = msg_send![dpad, left];
                    let d_right: *mut AnyObject = msg_send![dpad, right];

                    let check_btn = |btn: *mut AnyObject, code: crate::EvCode| {
                        if btn == elem {
                            let pressed: bool = unsafe { msg_send![btn, isPressed] };
                            push(if pressed {
                                EventType::ButtonPressed(code)
                            } else {
                                EventType::ButtonReleased(code)
                            });
                            return true;
                        }
                        false
                    };
                    if check_btn(b_a, crate::EvCode(BUTTONS[0]))
                        || check_btn(b_x, crate::EvCode(BUTTONS[2]))
                        || check_btn(d_up, crate::EvCode(BUTTONS[6]))
                        || check_btn(d_down, crate::EvCode(BUTTONS[7]))
                        || check_btn(d_left, crate::EvCode(BUTTONS[8]))
                        || check_btn(d_right, crate::EvCode(BUTTONS[9]))
                    {
                        return;
                    }

                    let read_axis = |a: *mut AnyObject| -> i32 {
                        if a.is_null() {
                            0
                        } else {
                            let v: f32 = msg_send![a, value];
                            (v as f64 * i32::MAX as f64) as i32
                        }
                    };
                    let dpx: *mut AnyObject = msg_send![dpad, xAxis];
                    let dpy: *mut AnyObject = msg_send![dpad, yAxis];
                    if dpx == elem {
                        push(EventType::AxisValueChanged(
                            read_axis(dpx),
                            crate::EvCode(AXES[4]),
                        ));
                        return;
                    }
                    if dpy == elem {
                        push(EventType::AxisValueChanged(
                            read_axis(dpy),
                            crate::EvCode(AXES[5]),
                        ));
                        return;
                    }
                })
                .copy();
            let _: () = msg_send![micro, setValueChangedHandler: &*block];
        }
    }

    pub(crate) fn new() -> Result<Self, PlatformError> {
        let mut ctx = Gilrs {
            events: VecDeque::new(),
            gamepads: Vec::new(),
            snapshot_buf: Arc::new(Mutex::new(None)),
            event_buf: Arc::new(Mutex::new(Vec::new())),
        };

        // Start discovery and register valueChanged handlers on main thread.
        let event_buf = ctx.event_buf.clone();
        ctx.post_on_main(move || unsafe {
            autoreleasepool(|_| {
                if let Some(klass) = AnyClass::get(c"GCController") {
                    let nil_block: Option<&AnyObject> = None;
                    let _: () = msg_send![klass,
                        startWirelessControllerDiscoveryWithCompletionHandler: nil_block
                    ];

                    // Register for existing controllers
                    let controllers: *mut AnyObject = msg_send![klass, controllers];
                    if !controllers.is_null() {
                        let controllers_ref: &AnyObject = &*controllers;
                        let count: usize = msg_send![controllers_ref, count];
                        for i in 0..count {
                            let ctrl: *mut AnyObject = msg_send![controllers_ref, objectAtIndex: i];
                            if ctrl.is_null() {
                                continue;
                            }
                            Gilrs::install_handlers(ctrl, &event_buf);
                        }
                    }
                }
            });
        });

        Ok(ctx)
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        // Drain events from main-thread handlers into local queue first
        if let Ok(mut eb) = self.event_buf.lock() {
            for (addr, et) in eb.drain(..) {
                if let Some(id) = self.gamepads.iter().position(|g| g.controller_addr == addr) {
                    self.events.push_back(Event::new(id, et));
                }
            }
        }

        if let Some(ev) = self.events.pop_front() {
            return Some(ev);
        }

        // Schedule a snapshot on the main thread without blocking.
        let buf = self.snapshot_buf.clone();
        self.post_on_main(move || unsafe {
            autoreleasepool(|_| {
                let mut out: Vec<Snapshot> = Vec::new();
                if let Some(klass) = AnyClass::get(c"GCController") {
                    let controllers: *mut AnyObject = msg_send![klass, controllers];
                    if !controllers.is_null() {
                        let controllers_ref: &AnyObject = &*controllers;
                        let count: usize = msg_send![controllers_ref, count];
                        for i in 0..count {
                            let ctrl: *mut AnyObject = msg_send![controllers_ref, objectAtIndex: i];
                            if ctrl.is_null() {
                                continue;
                            }

                            let mut buttons = [false; BUTTONS.len()];
                            let mut axes = [0i32; AXES.len()];

                            let ext: *mut AnyObject = msg_send![ctrl, extendedGamepad];
                            if !ext.is_null() {
                                let b_a: *mut AnyObject = msg_send![ext, buttonA];
                                let b_b: *mut AnyObject = msg_send![ext, buttonB];
                                let b_x: *mut AnyObject = msg_send![ext, buttonX];
                                let b_y: *mut AnyObject = msg_send![ext, buttonY];
                                let b_l1: *mut AnyObject = msg_send![ext, leftShoulder];
                                let b_r1: *mut AnyObject = msg_send![ext, rightShoulder];
                                let dpad: *mut AnyObject = msg_send![ext, dpad];
                                let d_up: *mut AnyObject = msg_send![dpad, up];
                                let d_down: *mut AnyObject = msg_send![dpad, down];
                                let d_left: *mut AnyObject = msg_send![dpad, left];
                                let d_right: *mut AnyObject = msg_send![dpad, right];

                                let read_btn = |btn: *mut AnyObject| -> bool {
                                    if btn.is_null() {
                                        false
                                    } else {
                                        let p: bool = msg_send![btn, isPressed];
                                        p
                                    }
                                };
                                buttons[0] = read_btn(b_a);
                                buttons[1] = read_btn(b_b);
                                buttons[2] = read_btn(b_x);
                                buttons[3] = read_btn(b_y);
                                buttons[4] = read_btn(b_l1);
                                buttons[5] = read_btn(b_r1);
                                buttons[6] = read_btn(d_up);
                                buttons[7] = read_btn(d_down);
                                buttons[8] = read_btn(d_left);
                                buttons[9] = read_btn(d_right);

                                let ls: *mut AnyObject = msg_send![ext, leftThumbstick];
                                let rs: *mut AnyObject = msg_send![ext, rightThumbstick];
                                let lsx: *mut AnyObject = msg_send![ls, xAxis];
                                let lsy: *mut AnyObject = msg_send![ls, yAxis];
                                let rsx: *mut AnyObject = msg_send![rs, xAxis];
                                let rsy: *mut AnyObject = msg_send![rs, yAxis];
                                let dpx: *mut AnyObject = msg_send![dpad, xAxis];
                                let dpy: *mut AnyObject = msg_send![dpad, yAxis];
                                let lt: *mut AnyObject = msg_send![ext, leftTrigger];
                                let rt: *mut AnyObject = msg_send![ext, rightTrigger];

                                let read_axis = |a: *mut AnyObject| -> i32 {
                                    if a.is_null() {
                                        0
                                    } else {
                                        let v: f32 = msg_send![a, value];
                                        (v as f64 * i32::MAX as f64) as i32
                                    }
                                };
                                axes[0] = read_axis(lsx);
                                axes[1] = read_axis(lsy);
                                axes[2] = read_axis(rsx);
                                axes[3] = read_axis(rsy);
                                axes[4] = read_axis(dpx);
                                axes[5] = read_axis(dpy);
                                axes[6] = read_axis(lt);
                                axes[7] = read_axis(rt);
                            } else {
                                let micro: *mut AnyObject = msg_send![ctrl, microGamepad];
                                if !micro.is_null() {
                                    let b_a: *mut AnyObject = msg_send![micro, buttonA];
                                    let b_x: *mut AnyObject = msg_send![micro, buttonX];
                                    let dpad: *mut AnyObject = msg_send![micro, dpad];
                                    let d_up: *mut AnyObject = msg_send![dpad, up];
                                    let d_down: *mut AnyObject = msg_send![dpad, down];
                                    let d_left: *mut AnyObject = msg_send![dpad, left];
                                    let d_right: *mut AnyObject = msg_send![dpad, right];

                                    let read_btn = |btn: *mut AnyObject| -> bool {
                                        if btn.is_null() {
                                            false
                                        } else {
                                            let p: bool = msg_send![btn, isPressed];
                                            p
                                        }
                                    };
                                    buttons[0] = read_btn(b_a);
                                    buttons[2] = read_btn(b_x);
                                    buttons[6] = read_btn(d_up);
                                    buttons[7] = read_btn(d_down);
                                    buttons[8] = read_btn(d_left);
                                    buttons[9] = read_btn(d_right);

                                    let read_axis = |a: *mut AnyObject| -> i32 {
                                        if a.is_null() {
                                            0
                                        } else {
                                            let v: f32 = msg_send![a, value];
                                            (v as f64 * i32::MAX as f64) as i32
                                        }
                                    };
                                    let dpx: *mut AnyObject = msg_send![dpad, xAxis];
                                    let dpy: *mut AnyObject = msg_send![dpad, yAxis];
                                    axes[4] = read_axis(dpx);
                                    axes[5] = read_axis(dpy);
                                }
                            }

                            let name_ns: *mut NSString = msg_send![ctrl, vendorName];
                            let name = if name_ns.is_null() {
                                None
                            } else {
                                Some(Gilrs::nsstring_to_string(name_ns))
                            };

                            out.push(Snapshot {
                                addr: ctrl as usize,
                                name,
                                buttons,
                                axes,
                            });
                        }
                    }
                }
                if let Ok(mut guard) = buf.lock() {
                    *guard = Some(out);
                }
            })
        });

        // Take the latest snapshot if available, and note whether it is fresh.
        let (snapshots, had_snapshot): (Vec<Snapshot>, bool) = match self.snapshot_buf.lock() {
            Ok(mut g) => match g.take() {
                Some(v) => (v, true),
                None => (Vec::new(), false),
            },
            Err(_) => (Vec::new(), false),
        };

        // Presence detection and event generation (only when we have a fresh snapshot)
        if had_snapshot {
            let present_addrs: Vec<usize> = snapshots.iter().map(|s| s.addr).collect();

            for (id, g) in self.gamepads.iter_mut().enumerate() {
                let present = present_addrs.iter().any(|&a| a == g.controller_addr);
                if g.connected && !present {
                    g.connected = false;
                    self.events
                        .push_back(Event::new(id, EventType::Disconnected));
                } else if !g.connected && present {
                    g.connected = true;
                    self.events.push_back(Event::new(id, EventType::Connected));
                }
            }
        }

        // Update or create gamepads and emit diffs
        for snap in snapshots {
            let idx = if let Some(i) = self
                .gamepads
                .iter()
                .position(|g| g.controller_addr == snap.addr)
            {
                i
            } else {
                let id = self.gamepads.len();
                self.gamepads.push(Gamepad {
                    name: snap.name.unwrap_or_default(),
                    uuid: Uuid::nil(),
                    connected: true,
                    controller_addr: snap.addr,
                    buttons: [false; BUTTONS.len()],
                    axes: [0; AXES.len()],
                });
                self.events.push_back(Event::new(id, EventType::Connected));
                id
            };

            // Buttons
            for i in 0..BUTTONS.len() {
                let new = snap.buttons[i];
                if self.gamepads[idx].buttons[i] != new {
                    self.gamepads[idx].buttons[i] = new;
                    let code = BUTTONS[i];
                    self.events.push_back(if new {
                        Event::new(idx, EventType::ButtonPressed(crate::EvCode(code)))
                    } else {
                        Event::new(idx, EventType::ButtonReleased(crate::EvCode(code)))
                    });
                }
            }

            // Axes
            for i in 0..AXES.len() {
                let new = snap.axes[i];
                if self.gamepads[idx].axes[i] != new {
                    self.gamepads[idx].axes[i] = new;
                    let code = AXES[i];
                    self.events.push_back(Event::new(
                        idx,
                        EventType::AxisValueChanged(new, crate::EvCode(code)),
                    ));
                }
            }
        }

        self.events.pop_front()
    }

    pub(crate) fn next_event_blocking(&mut self, _timeout: Option<Duration>) -> Option<Event> {
        // iOS GameController is callback-driven; non-blocking semantics for now.
        self.next_event()
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        self.gamepads.get(id)
    }

    /// Returns index greater than index of last connected gamepad.
    pub fn last_gamepad_hint(&self) -> usize {
        self.gamepads.len()
    }

    // Helper: convert NSString* to Rust String. Safe to call with null.
    fn nsstring_to_string(ns: *mut NSString) -> String {
        if ns.is_null() {
            return String::new();
        }
        unsafe {
            let utf8: *const i8 = msg_send![ns, UTF8String];
            if utf8.is_null() {
                String::new()
            } else {
                CStr::from_ptr(utf8).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for Gilrs {
    fn drop(&mut self) {
        self.run_on_main(|| unsafe {
            autoreleasepool(|_| {
                if let Some(klass) = AnyClass::get(c"GCController") {
                    let _: () = msg_send![klass, stopWirelessControllerDiscovery];
                }
            });
        });
    }
}

#[derive(Debug, Default)]
pub struct Gamepad {
    name: String,
    uuid: Uuid,
    connected: bool,
    // Store only the controller address to keep this struct Send across threads.
    controller_addr: usize,
    buttons: [bool; BUTTONS.len()],
    axes: [i32; AXES.len()],
}

impl Gamepad {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn vendor_id(&self) -> Option<u16> {
        None
    }

    pub fn product_id(&self) -> Option<u16> {
        None
    }

    pub fn power_info(&self) -> PowerInfo {
        PowerInfo::Unknown
    }

    pub fn is_ff_supported(&self) -> bool {
        false
    }

    /// Creates Ffdevice corresponding to this gamepad.
    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice)
    }

    pub fn buttons(&self) -> &[EvCode] {
        &BUTTONS
    }

    pub fn axes(&self) -> &[EvCode] {
        &AXES
    }

    pub(crate) fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        if AXES.iter().any(|&c| c == nec) {
            Some(&AXIS_INFO)
        } else {
            None
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }
}

#[cfg(feature = "serde-serialize")]
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode(u16);

impl EvCode {
    pub fn into_u32(self) -> u32 {
        self.0 as u32
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        self.0.fmt(f)
    }
}

pub mod native_ev_codes {
    use super::EvCode;

    pub const AXIS_LSTICKX: EvCode = EvCode(0);
    pub const AXIS_LSTICKY: EvCode = EvCode(1);
    pub const AXIS_LEFTZ: EvCode = EvCode(2);
    pub const AXIS_RSTICKX: EvCode = EvCode(3);
    pub const AXIS_RSTICKY: EvCode = EvCode(4);
    pub const AXIS_RIGHTZ: EvCode = EvCode(5);
    pub const AXIS_DPADX: EvCode = EvCode(6);
    pub const AXIS_DPADY: EvCode = EvCode(7);
    pub const AXIS_RT: EvCode = EvCode(8);
    pub const AXIS_LT: EvCode = EvCode(9);
    pub const AXIS_RT2: EvCode = EvCode(10);
    pub const AXIS_LT2: EvCode = EvCode(11);

    pub const BTN_SOUTH: EvCode = EvCode(12);
    pub const BTN_EAST: EvCode = EvCode(13);
    pub const BTN_C: EvCode = EvCode(14);
    pub const BTN_NORTH: EvCode = EvCode(15);
    pub const BTN_WEST: EvCode = EvCode(16);
    pub const BTN_Z: EvCode = EvCode(17);
    pub const BTN_LT: EvCode = EvCode(18);
    pub const BTN_RT: EvCode = EvCode(19);
    pub const BTN_LT2: EvCode = EvCode(20);
    pub const BTN_RT2: EvCode = EvCode(21);
    pub const BTN_SELECT: EvCode = EvCode(22);
    pub const BTN_START: EvCode = EvCode(23);
    pub const BTN_MODE: EvCode = EvCode(24);
    pub const BTN_LTHUMB: EvCode = EvCode(25);
    pub const BTN_RTHUMB: EvCode = EvCode(26);

    pub const BTN_DPAD_UP: EvCode = EvCode(27);
    pub const BTN_DPAD_DOWN: EvCode = EvCode(28);
    pub const BTN_DPAD_LEFT: EvCode = EvCode(29);
    pub const BTN_DPAD_RIGHT: EvCode = EvCode(30);
}

// Fixed mapping for iOS extended profile
const BUTTONS: [EvCode; 10] = [
    native_ev_codes::BTN_SOUTH,
    native_ev_codes::BTN_EAST,
    native_ev_codes::BTN_WEST,
    native_ev_codes::BTN_NORTH,
    native_ev_codes::BTN_LT,
    native_ev_codes::BTN_RT,
    native_ev_codes::BTN_DPAD_UP,
    native_ev_codes::BTN_DPAD_DOWN,
    native_ev_codes::BTN_DPAD_LEFT,
    native_ev_codes::BTN_DPAD_RIGHT,
];

const AXES: [EvCode; 8] = [
    native_ev_codes::AXIS_LSTICKX,
    native_ev_codes::AXIS_LSTICKY,
    native_ev_codes::AXIS_RSTICKX,
    native_ev_codes::AXIS_RSTICKY,
    native_ev_codes::AXIS_DPADX,
    native_ev_codes::AXIS_DPADY,
    native_ev_codes::AXIS_LT,
    native_ev_codes::AXIS_RT,
];

static AXIS_INFO: AxisInfo = AxisInfo {
    min: i32::MIN,
    max: i32::MAX,
    deadzone: None,
};
