// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::time::Duration;

use super::sink::{sink, Command};

#[derive(Debug)]
/// Represents gamepad. Reexported as FfDevice
pub struct Device {
    device_id: i32,
    last_amplitude: i32,
}

impl Device {
    pub(super) fn new(device_id: i32) -> Self {
        Device {
            device_id,
            last_amplitude: 0,
        }
    }

    /// Sets magnitude for strong and weak ff motors.
    ///
    /// Android's `Vibrator` exposes a single amplitude rather than separate motors, so the
    /// stronger of the two is used.
    pub fn set_ff_state(&mut self, strong: u16, weak: u16, min_duration: Duration) {
        let magnitude = strong.max(weak);
        let amplitude = if magnitude == 0 {
            0
        } else {
            // VibrationEffect amplitudes are 1..=255.
            ((magnitude >> 8) as i32).clamp(1, 255)
        };

        // The force feedback server ticks every device unconditionally, 20 times a second, whether
        // or not an effect is playing. Staying silent while idle keeps that from turning into a
        // steady stream of binder calls. A non-zero amplitude still has to be re-sent every tick,
        // because each VibrationEffect only lasts for the duration it was given.
        if amplitude == 0 && self.last_amplitude == 0 {
            return;
        }
        self.last_amplitude = amplitude;

        sink().queue_command(Command::Rumble {
            device_id: self.device_id,
            amplitude,
            duration_ms: min_duration.as_millis().min(i64::MAX as u128) as i64,
        });
    }
}
