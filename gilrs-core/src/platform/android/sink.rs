// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Global ingestion point shared by every source of Android input.
//!
//! Android has no way to poll gamepad state, so events have to be pushed in from wherever they are
//! observed: the `Window.Callback` hook (UI thread), the public forwarding API (any thread), and
//! the JNI worker thread that enumerates devices. Routing all of them through one sink keeps
//! `Gilrs` itself free of JNI and lets a new source be added without touching the backend.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use super::gamepad::EvCode;
use crate::{AxisInfo, PowerInfo};

/// How an Android axis maps onto a gilrs axis, and how to rescale its values.
#[derive(Debug, Clone)]
pub(super) struct AxisSpec {
    /// `MotionEvent.AXIS_*`
    pub android: i32,
    pub code: EvCode,
    pub info: AxisInfo,
    /// Lower bound as reported by `InputDevice.MotionRange`.
    pub min: f32,
    /// `1.0 / (max - min)`, precomputed.
    pub inv_range: f32,
    /// Whether the axis is bipolar (a stick) rather than unipolar (a trigger).
    pub signed: bool,
}

impl AxisSpec {
    pub fn scale(&self, value: f32) -> i32 {
        let t = ((value - self.min) * self.inv_range).clamp(0.0, 1.0) as f64;
        let t = if self.signed { t * 2.0 - 1.0 } else { t };
        (t * i32::MAX as f64) as i32
    }
}

#[derive(Debug)]
pub(super) struct DeviceInfo {
    pub device_id: i32,
    pub name: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    /// Android keycode -> gilrs code. Built once, with every quirk already resolved, so that event
    /// translation is a plain lookup.
    pub key_map: Vec<(i32, EvCode)>,
    /// Deduplicated values of `key_map`; the order defines `Gamepad::buttons()`.
    pub buttons: Vec<EvCode>,
    pub axes: Vec<AxisSpec>,
    /// Codes of `axes`, in the same order; defines `Gamepad::axes()`.
    pub axis_codes: Vec<EvCode>,
    pub has_vibrator: bool,
}

#[derive(Debug)]
pub(super) enum RawEvent {
    DeviceAdded(Arc<DeviceInfo>),
    DeviceRemoved(i32),
    Battery { device_id: i32, info: PowerInfo },
    Key { device_id: i32, keycode: i32, down: bool },
    Motion { device_id: i32, values: Vec<(i32, f32)> },
}

/// Work for the JNI thread, which owns the only permanently attached `JNIEnv`.
#[derive(Debug)]
pub(super) enum Command {
    Rumble {
        device_id: i32,
        /// 0 cancels, otherwise 1..=255.
        amplitude: i32,
        duration_ms: i64,
    },
}

#[derive(Default)]
struct Devices {
    /// Axis ids to query on a `MotionEvent`, per device. Kept separate from `snapshot` so the UI
    /// thread can read it without cloning a whole `DeviceInfo`.
    axes: HashMap<i32, Arc<Vec<i32>>>,
    snapshot: HashMap<i32, Arc<DeviceInfo>>,
}

pub(super) struct Sink {
    events: Mutex<VecDeque<RawEvent>>,
    ready: Condvar,
    commands: Mutex<Vec<Command>>,
    devices: Mutex<Devices>,
    poll: Mutex<bool>,
    poll_cv: Condvar,
    first_pass: Mutex<bool>,
    first_pass_cv: Condvar,
}

static SINK: OnceLock<Sink> = OnceLock::new();

pub(super) fn sink() -> &'static Sink {
    SINK.get_or_init(|| Sink {
        events: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        commands: Mutex::new(Vec::new()),
        devices: Mutex::new(Devices::default()),
        poll: Mutex::new(false),
        poll_cv: Condvar::new(),
        first_pass: Mutex::new(false),
        first_pass_cv: Condvar::new(),
    })
}

impl Sink {
    pub fn push(&self, event: RawEvent) {
        if let Ok(mut q) = self.events.lock() {
            // A backlog only builds up if the app stopped calling next_event; drop the oldest
            // rather than grow without bound.
            if q.len() >= 4096 {
                q.pop_front();
            }
            q.push_back(event);
        }
        self.ready.notify_all();
    }

    pub fn drain(&self, out: &mut Vec<RawEvent>) {
        if let Ok(mut q) = self.events.lock() {
            out.extend(q.drain(..));
        }
    }

    pub fn clear(&self) {
        if let Ok(mut q) = self.events.lock() {
            q.clear();
        }
    }

    pub fn wait(&self, timeout: Option<Duration>) {
        let Ok(q) = self.events.lock() else { return };
        if !q.is_empty() {
            return;
        }
        match timeout {
            Some(t) => drop(self.ready.wait_timeout(q, t)),
            None => drop(self.ready.wait(q)),
        }
    }

    // -- device registry -------------------------------------------------------------------

    pub fn add_device(&self, info: Arc<DeviceInfo>) {
        if let Ok(mut d) = self.devices.lock() {
            let axes = info.axes.iter().map(|a| a.android).collect();
            d.axes.insert(info.device_id, Arc::new(axes));
            d.snapshot.insert(info.device_id, info.clone());
        }
        self.push(RawEvent::DeviceAdded(info));
    }

    pub fn remove_device(&self, device_id: i32) {
        if let Ok(mut d) = self.devices.lock() {
            d.axes.remove(&device_id);
            d.snapshot.remove(&device_id);
        }
        self.push(RawEvent::DeviceRemoved(device_id));
    }

    /// Axis ids to sample for a device, or `None` if it is not a known gamepad.
    pub fn device_axes(&self, device_id: i32) -> Option<Arc<Vec<i32>>> {
        self.devices.lock().ok()?.axes.get(&device_id).cloned()
    }

    pub fn is_known_device(&self, device_id: i32) -> bool {
        self.devices
            .lock()
            .map(|d| d.axes.contains_key(&device_id))
            .unwrap_or(false)
    }

    /// Currently connected devices, used to seed a freshly created `Gilrs`.
    pub fn devices(&self) -> Vec<Arc<DeviceInfo>> {
        match self.devices.lock() {
            Ok(d) => d.snapshot.values().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- JNI thread wakeup -----------------------------------------------------------------

    pub fn queue_command(&self, cmd: Command) {
        if let Ok(mut c) = self.commands.lock() {
            c.push(cmd);
        }
        self.request_poll();
    }

    pub fn take_commands(&self) -> Vec<Command> {
        match self.commands.lock() {
            Ok(mut c) => std::mem::take(&mut *c),
            Err(_) => Vec::new(),
        }
    }

    /// Wakes the JNI thread immediately instead of letting it sleep out its poll interval.
    pub fn request_poll(&self) {
        if let Ok(mut p) = self.poll.lock() {
            *p = true;
        }
        self.poll_cv.notify_all();
    }

    pub fn wait_poll(&self, timeout: Duration) {
        let Ok(mut p) = self.poll.lock() else { return };
        if !*p {
            let Ok((g, _)) = self.poll_cv.wait_timeout(p, timeout) else {
                return;
            };
            p = g;
        }
        *p = false;
    }

    pub fn signal_first_pass(&self) {
        if let Ok(mut f) = self.first_pass.lock() {
            *f = true;
        }
        self.first_pass_cv.notify_all();
    }

    /// Lets `Gilrs::new` report already-connected gamepads without waiting a full poll interval.
    pub fn wait_first_pass(&self, timeout: Duration) {
        let Ok(f) = self.first_pass.lock() else { return };
        if !*f {
            drop(self.first_pass_cv.wait_timeout(f, timeout));
        }
    }
}
