//! The DualShock 2: the PlayStation's DualShock (`texel_ps1::pad`), whose
//! buttons also report how hard they are pressed. A game asks for those
//! bytes in configuration mode (command 0x4F, with a mask of the poll's
//! bytes it wants: the buttons, the sticks, then twelve pressures), and
//! the controller then answers polls with them (its mode 0x79: nine
//! half-words). Some games refuse to start on a controller that will not.
//!
//! The rest is the DualShock's: this wraps it, answering the commands the
//! DualShock 2 adds (0x40, 0x41, 0x4F) and the polls in pressure mode
//! itself, but letting the DualShock see every byte (so its rumble, modes
//! and configuration go on as before). It is serialised as the DualShock
//! alone (save states of format 2 hold that); its mask is saved apart (see
//! `Ps2::save_core`).
//!
//! Pressures come from the buttons: 255 pressed, 0 not (keyboards and most
//! gamepads' buttons have nothing in between).

use std::ops::{Deref, DerefMut};

use texel_ps1::button;
use texel_ps1::pad::Pad;

/// The poll's bytes in analog mode: buttons and sticks.
pub const ANALOG_MASK: u32 = 0x3F;
/// Every byte the DualShock 2 can send: those, and twelve pressures.
const FULL_MASK: u32 = 0x3_FFFF;

/// The pressures' buttons, in the poll's order.
const PRESSURE_BUTTONS: [u16; 12] = [
    button::RIGHT,
    button::LEFT,
    button::UP,
    button::DOWN,
    button::TRIANGLE,
    button::CIRCLE,
    button::CROSS,
    button::SQUARE,
    button::L1,
    button::R1,
    button::L2,
    button::R2,
];

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DualShock2 {
    pad: Pad,
    /// The poll's bytes the game asked for (bit k: byte k), in analog mode.
    #[serde(skip, default = "analog_mask")]
    pub mask: u32,
    /// The mode the mask was set in: leaving it (the Analog button, or
    /// command 0x44 to digital) goes back to the DualShock's bytes.
    #[serde(skip)]
    analog: bool,
    #[serde(skip)]
    step: usize,
    #[serde(skip)]
    command: u8,
    #[serde(skip)]
    params: [u8; 3],
    /// This command is the DualShock 2's own, or a poll in pressure mode;
    /// and which (as the controller was at its start: a poll may enter
    /// configuration mode).
    #[serde(skip)]
    own: bool,
    #[serde(skip)]
    own_config: bool,
}

fn analog_mask() -> u32 {
    ANALOG_MASK
}

impl Deref for DualShock2 {
    type Target = Pad;
    fn deref(&self) -> &Pad {
        &self.pad
    }
}

impl DerefMut for DualShock2 {
    fn deref_mut(&mut self) -> &mut Pad {
        &mut self.pad
    }
}

impl DualShock2 {
    pub fn new(connected: bool) -> Self {
        Self {
            pad: Pad::new(connected),
            mask: ANALOG_MASK,
            analog: false,
            step: 0,
            command: 0,
            params: [0; 3],
            own: false,
            own_config: false,
        }
    }

    /// The mask a state saved (see `Ps2::save_core`), in the mode the
    /// controller is in.
    pub fn restore_mask(&mut self, mask: u32) {
        self.mask = mask & FULL_MASK;
        self.analog = self.pad.analog;
    }

    pub fn reset_transfer(&mut self) {
        self.pad.reset_transfer();
        self.step = 0;
    }

    /// The Analog button: the DualShock's, which ends pressure mode.
    pub fn press_analog_button(&mut self) {
        let was = self.pad.analog;
        self.pad.press_analog_button();
        if self.pad.analog != was {
            self.mask = ANALOG_MASK;
        }
    }

    /// The pressures are on (a mask beyond the buttons and sticks).
    fn pressure_mode(&self) -> bool {
        self.pad.analog && self.mask != ANALOG_MASK
    }

    /// The bytes a pressure-mode poll sends (those of the mask, in order).
    fn poll_bytes(&self) -> Vec<u8> {
        let b = !self.pad.buttons;
        let [lx, ly, rx, ry] = self.pad.axes;
        let mut all = vec![b as u8, (b >> 8) as u8, rx, ry, lx, ly];
        all.extend(
            PRESSURE_BUTTONS
                .iter()
                .map(|&k| if self.pad.buttons & k != 0 { 0xFF } else { 0 }),
        );
        all.into_iter()
            .enumerate()
            .filter(|(k, _)| self.mask & (1 << k) != 0)
            .map(|(_, v)| v)
            .collect()
    }

    /// Data byte `i` of the reply to one of the DualShock 2's commands.
    fn config_data(&self, i: usize) -> u8 {
        let table = match self.command {
            0x40 => [0x00, 0x00, 0x02, 0x00, 0x00, 0x5A],
            // The bytes polls can send, in analog mode: all of them,
            // pressures included, whatever the mask (games tell a
            // DualShock 2 from a DualShock by this: MTV Pimp My Ride asks
            // before choosing any, and refuses a controller without).
            0x41 if self.pad.analog => [
                FULL_MASK as u8,
                (FULL_MASK >> 8) as u8,
                (FULL_MASK >> 16) as u8,
                0x00,
                0x00,
                0x5A,
            ],
            0x41 => [0; 6],
            _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x5A],
        };
        table[i.min(5)]
    }

    /// Exchanges one byte; returns the reply and whether the controller
    /// acknowledges it (as `Pad::transfer`).
    pub fn transfer(&mut self, v: u8) -> (u8, bool) {
        // Mode changes since the mask was set end pressure mode.
        if self.pad.analog != self.analog {
            self.analog = self.pad.analog;
            self.mask = ANALOG_MASK;
        }
        let step = self.step;
        self.step += 1;
        if step == 1 {
            self.command = v;
            self.params = [0; 3];
            self.own_config = self.pad.in_config();
            // Setting the mode (0x44) ends pressure mode only by leaving
            // analog mode (seen at the next exchange, above): setting
            // analog mode again keeps it (MTV Pimp My Ride sets it,
            // unlocked, after asking for the pressures, and wants them).
            self.own = if self.own_config {
                matches!(v, 0x40 | 0x41 | 0x4F)
            } else {
                matches!(v, 0x42 | 0x43) && self.pressure_mode()
            };
        }
        // The DualShock sees every byte (rumble, configuration).
        let theirs = self.pad.transfer(v);
        if !self.own || step == 0 {
            return theirs;
        }
        let config = self.own_config;
        let len = if config { 6 } else { self.poll_bytes().len() };
        match step {
            1 if config => (0xF3, true),
            1 => (0x70 | (len / 2) as u8, true),
            2 => (0x5A, true),
            _ => {
                let i = step - 3;
                if let Some(p) = self.params.get_mut(i) {
                    *p = v;
                }
                let reply = if config {
                    self.config_data(i)
                } else {
                    self.poll_bytes().get(i).copied().unwrap_or(0xFF)
                };
                let last = i + 1 >= len;
                if last && self.command == 0x4F && self.pad.analog {
                    let [a, b, c] = self.params;
                    self.mask = (u32::from(a) | u32::from(b) << 8 | u32::from(c) << 16) & FULL_MASK;
                }
                (reply, !last)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(pad: &mut DualShock2, bytes: &[u8]) -> Vec<u8> {
        pad.reset_transfer();
        bytes.iter().map(|&b| pad.transfer(b).0).collect()
    }

    fn analog_and_locked(pad: &mut DualShock2) {
        command(pad, &[0x01, 0x43, 0, 1, 0]);
        command(pad, &[0x01, 0x44, 0, 1, 3, 0, 0, 0, 0]);
    }

    #[test]
    fn pressures_once_asked_for() {
        let mut pad = DualShock2::new(true);
        analog_and_locked(&mut pad);
        // What polls can send: the buttons, the sticks and the pressures.
        assert_eq!(
            command(&mut pad, &[0x01, 0x41, 0, 0, 0, 0, 0, 0, 0]),
            [0xFF, 0xF3, 0x5A, 0xFF, 0xFF, 0x03, 0x00, 0x00, 0x5A]
        );
        // Everything, then out of configuration.
        command(&mut pad, &[0x01, 0x4F, 0, 0xFF, 0xFF, 0x03, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        pad.buttons = button::CROSS | button::R2;
        let mut poll = vec![0x01, 0x42, 0];
        poll.resize(3 + 18, 0);
        let reply = command(&mut pad, &poll);
        assert_eq!(reply[1], 0x79);
        assert_eq!(reply[2], 0x5A);
        assert_eq!(&reply[3..5], &[0xFF, 0xBD]);
        assert_eq!(&reply[5..9], &[0x80; 4]);
        // Right, left, up, down, triangle, circle, cross, square, L1, R1, L2, R2.
        assert_eq!(&reply[9..21], &[0, 0, 0, 0, 0, 0, 0xFF, 0, 0, 0, 0, 0xFF]);
    }

    #[test]
    fn a_dualshock_until_asked() {
        let mut pad = DualShock2::new(true);
        pad.buttons = button::CROSS;
        assert_eq!(
            command(&mut pad, &[0x01, 0x42, 0, 0, 0]),
            [0xFF, 0x41, 0x5A, 0xFF, 0xBF]
        );
        analog_and_locked(&mut pad);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        let reply = command(&mut pad, &[0x01, 0x42, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&reply[..3], &[0xFF, 0x73, 0x5A]);
    }

    #[test]
    fn rumble_in_pressure_mode() {
        let mut pad = DualShock2::new(true);
        analog_and_locked(&mut pad);
        command(&mut pad, &[0x01, 0x4D, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFF]);
        command(&mut pad, &[0x01, 0x4F, 0, 0xFF, 0xFF, 0x03, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        let mut poll = vec![0x01, 0x42, 0, 1, 0x80];
        poll.resize(3 + 18, 0);
        command(&mut pad, &poll);
        assert_eq!(pad.rumble, (0xFF, 0x80));
    }

    #[test]
    fn setting_analog_mode_again_keeps_pressures() {
        let mut pad = DualShock2::new(true);
        analog_and_locked(&mut pad);
        command(&mut pad, &[0x01, 0x4F, 0, 0xFF, 0xFF, 0x03, 0, 0, 0]);
        // Analog again, unlocked, in the same configuration session.
        command(&mut pad, &[0x01, 0x44, 0, 1, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        let mut poll = vec![0x01, 0x42, 0];
        poll.resize(3 + 18, 0);
        assert_eq!(command(&mut pad, &poll)[1], 0x79);
        // To digital: the pressures end.
        command(
            &mut pad,
            &[0x01, 0x43, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        command(&mut pad, &[0x01, 0x44, 0, 0, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(command(&mut pad, &[0x01, 0x42, 0, 0, 0])[1], 0x41);
        command(&mut pad, &[0x01, 0x43, 0, 1, 0]);
        command(&mut pad, &[0x01, 0x44, 0, 1, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(command(&mut pad, &[0x01, 0x42, 0, 0, 0, 0, 0, 0, 0])[1], 0x73);
    }

    #[test]
    fn leaving_analog_mode_ends_pressures() {
        let mut pad = DualShock2::new(true);
        command(&mut pad, &[0x01, 0x43, 0, 1, 0]);
        command(&mut pad, &[0x01, 0x44, 0, 1, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x4F, 0, 0xFF, 0xFF, 0x03, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        pad.press_analog_button();
        pad.press_analog_button();
        let reply = command(&mut pad, &[0x01, 0x42, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(reply[1], 0x73);
    }
}
