//! A DualShock controller: digital mode (like the original pad), analog
//! mode with two sticks, the configuration protocol games use to switch
//! modes, and rumble.

/// Axis centre value.
pub const CENTER: u8 = 0x80;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Pad {
    pub connected: bool,
    /// Pressed buttons (1 = pressed), see `crate::button`.
    pub buttons: u16,
    /// Left X, left Y, right X, right Y; 0 is left/up, 0xff right/down.
    pub axes: [u8; 4],
    pub analog: bool,
    /// Set by games: the Analog button then does nothing.
    pub locked: bool,
    config: bool,
    step: usize,
    command: u8,
    params: [u8; 8],
    /// Which rumble byte of the poll command drives which motor (0x00:
    /// small, 0x01: large, 0xff: none).
    motor_map: [u8; 6],
    /// Small (on/off) and large (0-255) motor strengths.
    pub rumble: (u8, u8),
}

impl Pad {
    pub fn new(connected: bool) -> Self {
        Self {
            connected,
            buttons: 0,
            axes: [CENTER; 4],
            analog: false,
            locked: false,
            config: false,
            step: 0,
            command: 0,
            params: [0; 8],
            motor_map: [0xff; 6],
            rumble: (0, 0),
        }
    }

    /// The Analog button: toggles the mode, unless a game locked it.
    pub fn press_analog_button(&mut self) {
        if !self.locked {
            self.analog = !self.analog;
            if !self.analog {
                self.rumble = (0, 0);
            }
        }
    }

    pub fn reset_transfer(&mut self) {
        self.step = 0;
    }

    /// In configuration mode (command 0x43): the DualShock 2's commands
    /// are answered then (see the PS2's `dualshock2`).
    pub fn in_config(&self) -> bool {
        self.config
    }

    fn id(&self) -> u8 {
        if self.config {
            0xf3
        } else if self.analog {
            0x73
        } else {
            0x41
        }
    }

    fn poll_data(&self) -> [u8; 6] {
        let b = !self.buttons;
        let [lx, ly, rx, ry] = self.axes;
        // Right stick first.
        [b as u8, (b >> 8) as u8, rx, ry, lx, ly]
    }

    /// Length of the data part of the reply to the current command.
    fn data_len(&self) -> usize {
        if self.config || self.analog { 6 } else { 2 }
    }

    /// Data byte `i` of the reply to the current command.
    fn data(&self, i: usize) -> u8 {
        let p0 = self.params[0];
        let table: [u8; 6] = match (self.config, self.command) {
            (false, _) | (true, 0x42) => self.poll_data(),
            (true, 0x45) => [0x03, 0x02, u8::from(self.analog), 0x02, 0x01, 0x00],
            (true, 0x46) if p0 == 0 => [0x00, 0x00, 0x01, 0x02, 0x00, 0x0a],
            (true, 0x46) => [0x00, 0x00, 0x01, 0x01, 0x01, 0x14],
            (true, 0x47) => [0x00, 0x00, 0x02, 0x00, 0x01, 0x00],
            (true, 0x4c) if p0 == 0 => [0x00, 0x00, 0x00, 0x04, 0x00, 0x00],
            (true, 0x4c) => [0x00, 0x00, 0x00, 0x07, 0x00, 0x00],
            (true, 0x4d) => self.motor_map,
            _ => [0; 6],
        };
        table[i]
    }

    /// Exchanges one byte; returns the reply and whether the pad acks.
    pub fn transfer(&mut self, v: u8) -> (u8, bool) {
        let step = self.step;
        self.step += 1;
        match step {
            0 => (0xff, true),
            1 => {
                self.command = v;
                self.params = [0; 8];
                let valid = matches!(v, 0x42 | 0x43) || (self.config && matches!(v, 0x44..=0x4f));
                if valid { (self.id(), true) } else { (0xff, false) }
            }
            2 => (0x5a, true),
            _ => {
                let i = step - 3;
                if let Some(p) = self.params.get_mut(i) {
                    *p = v;
                }
                let len = self.data_len();
                let reply = self.data(i.min(5));
                let last = i + 1 >= len;
                if last {
                    self.finish();
                }
                (reply, !last)
            }
        }
    }

    /// Applies the effects of a completed command.
    fn finish(&mut self) {
        let p = self.params;
        match self.command {
            0x42 if self.analog => {
                let mut small = 0;
                let mut large = 0;
                for (i, &m) in self.motor_map.iter().enumerate() {
                    match m {
                        0x00 => small = if p[i] & 1 != 0 { 0xff } else { 0 },
                        0x01 => large = p[i],
                        _ => {}
                    }
                }
                self.rumble = (small, large);
            }
            0x43 => self.config = p[0] == 1,
            0x44 if self.config => {
                self.analog = p[0] == 1;
                self.locked = p[1] == 3;
            }
            0x4d if self.config => self.motor_map.copy_from_slice(&p[..6]),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(pad: &mut Pad, bytes: &[u8]) -> Vec<u8> {
        pad.reset_transfer();
        bytes.iter().map(|&b| pad.transfer(b).0).collect()
    }

    #[test]
    fn digital_poll_then_switch_to_analog() {
        let mut pad = Pad::new(true);
        pad.buttons = crate::button::CROSS;
        assert_eq!(
            command(&mut pad, &[0x01, 0x42, 0, 0, 0]),
            [0xff, 0x41, 0x5a, 0xff, 0xbf]
        );
        // Enter configuration, set analog and lock, exit.
        command(&mut pad, &[0x01, 0x43, 0, 1, 0]);
        assert_eq!(command(&mut pad, &[0x01, 0x44, 0, 1, 3, 0, 0, 0, 0])[1], 0xf3);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        assert!(pad.analog && pad.locked);
        pad.axes = [0x10, 0x20, 0x30, 0x40];
        let reply = command(&mut pad, &[0x01, 0x42, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(reply, [0xff, 0x73, 0x5a, 0xff, 0xbf, 0x30, 0x40, 0x10, 0x20]);
        pad.press_analog_button();
        assert!(pad.analog, "locked");
    }

    #[test]
    fn rumble() {
        let mut pad = Pad::new(true);
        command(&mut pad, &[0x01, 0x43, 0, 1, 0]);
        command(&mut pad, &[0x01, 0x44, 0, 1, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x4d, 0, 0, 1, 0xff, 0xff, 0xff, 0xff]);
        command(&mut pad, &[0x01, 0x43, 0, 0, 0, 0, 0, 0, 0]);
        command(&mut pad, &[0x01, 0x42, 0, 1, 0x80, 0, 0, 0, 0]);
        assert_eq!(pad.rumble, (0xff, 0x80));
    }
}
