// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::collections::VecDeque;
use std::error;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::ff::Device as FfDevice;
use super::jni_glue;
use super::sink::{sink, DeviceInfo, RawEvent};
use crate::{AxisInfo, Event, EventType, PlatformError, PowerInfo};

#[derive(Debug)]
pub struct Error(pub(super) String);

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(&self.0)
    }
}

impl error::Error for Error {}

#[derive(Debug)]
pub struct Gilrs {
    gamepads: Vec<Gamepad>,
    events: VecDeque<Event>,
}

impl Gilrs {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        jni_glue::start().map_err(|e| PlatformError::Other(Box::new(Error(e))))?;

        // Give the worker thread a moment to complete its first enumeration, so that gamepads
        // which were already connected show up before the caller's first next_event().
        sink().wait_first_pass(Duration::from_millis(500));
        sink().clear();

        let mut gilrs = Gilrs {
            gamepads: Vec::new(),
            events: VecDeque::new(),
        };
        for info in sink().devices() {
            gilrs.connect(info);
        }

        Ok(gilrs)
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        if let Some(event) = self.events.pop_front() {
            return Some(event);
        }

        let mut raw = Vec::new();
        sink().drain(&mut raw);
        for event in raw {
            self.handle(event);
        }

        self.events.pop_front()
    }

    pub(crate) fn next_event_blocking(&mut self, timeout: Option<Duration>) -> Option<Event> {
        loop {
            if let Some(event) = self.next_event() {
                return Some(event);
            }

            sink().wait(timeout);

            if let Some(event) = self.next_event() {
                return Some(event);
            }
            if timeout.is_some() {
                return None;
            }
        }
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        self.gamepads.get(id)
    }

    /// Returns index greater than index of last connected gamepad.
    pub fn last_gamepad_hint(&self) -> usize {
        self.gamepads.len()
    }

    fn handle(&mut self, event: RawEvent) {
        match event {
            RawEvent::DeviceAdded(info) => self.connect(info),
            RawEvent::DeviceRemoved(device_id) => {
                if let Some(id) = self.index_of(device_id) {
                    self.gamepads[id].connected = false;
                    self.events
                        .push_back(Event::new(id, EventType::Disconnected));
                }
            }
            RawEvent::Battery { device_id, info } => {
                if let Some(id) = self.index_of(device_id) {
                    self.gamepads[id].power = info;
                }
            }
            RawEvent::Key {
                device_id,
                keycode,
                down,
            } => self.on_key(device_id, keycode, down),
            RawEvent::Motion { device_id, values } => self.on_motion(device_id, &values),
        }
    }

    fn index_of(&self, device_id: i32) -> Option<usize> {
        self.gamepads
            .iter()
            .position(|g| g.connected && g.info.device_id == device_id)
    }

    /// Reuses the slot of a matching disconnected gamepad so that replugging a controller keeps
    /// its `GamepadId`, as the other backends do.
    fn connect(&mut self, info: Arc<DeviceInfo>) {
        // Seeding from the sink's snapshot can race a DeviceAdded that is still queued; without
        // this the same controller would be added twice.
        if self.index_of(info.device_id).is_some() {
            return;
        }

        let existing = self.gamepads.iter().position(|g| {
            !g.connected
                && g.info.name == info.name
                && g.info.vendor_id == info.vendor_id
                && g.info.product_id == info.product_id
        });

        let id = match existing {
            Some(id) => {
                self.gamepads[id] = Gamepad::new(info);
                id
            }
            None => {
                self.gamepads.push(Gamepad::new(info));
                self.gamepads.len() - 1
            }
        };

        self.events.push_back(Event::new(id, EventType::Connected));
    }

    fn on_key(&mut self, device_id: i32, keycode: i32, down: bool) {
        let Some(id) = self.index_of(device_id) else {
            return;
        };

        let gamepad = &self.gamepads[id];
        let Some(&(_, code)) = gamepad.info.key_map.iter().find(|(k, _)| *k == keycode) else {
            debug!(
                "gilrs: gamepad {id} ({}) reported Android keycode {keycode}, which is not in its \
                 button map",
                gamepad.info.name
            );
            return;
        };
        let Some(index) = gamepad.info.buttons.iter().position(|&b| b == code) else {
            return;
        };
        if gamepad.buttons_state[index] == down {
            return;
        }

        self.gamepads[id].buttons_state[index] = down;
        self.events.push_back(Event::new(
            id,
            if down {
                EventType::ButtonPressed(crate::EvCode(code))
            } else {
                EventType::ButtonReleased(crate::EvCode(code))
            },
        ));
    }

    fn on_motion(&mut self, device_id: i32, values: &[(i32, f32)]) {
        let Some(id) = self.index_of(device_id) else {
            return;
        };

        for &(android, value) in values {
            let Some(index) = self.gamepads[id]
                .info
                .axes
                .iter()
                .position(|a| a.android == android)
            else {
                continue;
            };

            let scaled = self.gamepads[id].info.axes[index].scale(value);
            if self.gamepads[id].axes_state[index] == scaled {
                continue;
            }

            self.gamepads[id].axes_state[index] = scaled;
            let code = self.gamepads[id].info.axis_codes[index];
            self.events.push_back(Event::new(
                id,
                EventType::AxisValueChanged(scaled, crate::EvCode(code)),
            ));
        }
    }
}

#[derive(Debug)]
pub struct Gamepad {
    info: Arc<DeviceInfo>,
    connected: bool,
    power: PowerInfo,
    buttons_state: Vec<bool>,
    axes_state: Vec<i32>,
}

impl Gamepad {
    fn new(info: Arc<DeviceInfo>) -> Self {
        Gamepad {
            buttons_state: vec![false; info.buttons.len()],
            axes_state: vec![0; info.axes.len()],
            connected: true,
            power: PowerInfo::Unknown,
            info,
        }
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Android normalises gamepads onto standard keycodes and axes, so the backend reports
    /// semantic codes and relies on gilrs' default mapping. There is no SDL-compatible GUID to
    /// return, and returning one would risk matching a mapping written against SDL's positional
    /// button ordering.
    pub fn uuid(&self) -> Uuid {
        Uuid::nil()
    }

    pub fn vendor_id(&self) -> Option<u16> {
        self.info.vendor_id
    }

    pub fn product_id(&self) -> Option<u16> {
        self.info.product_id
    }

    pub fn power_info(&self) -> PowerInfo {
        self.power
    }

    pub fn is_ff_supported(&self) -> bool {
        self.info.has_vibrator
    }

    /// Creates Ffdevice corresponding to this gamepad.
    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice::new(self.info.device_id))
    }

    pub fn buttons(&self) -> &[EvCode] {
        &self.info.buttons
    }

    pub fn axes(&self) -> &[EvCode] {
        &self.info.axis_codes
    }

    pub(crate) fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        self.info
            .axes
            .iter()
            .find(|a| a.code == nec)
            .map(|a| &a.info)
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
