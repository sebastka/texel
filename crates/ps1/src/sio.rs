//! Serial port 0: digital controllers and memory cards.

use crate::irq::{Interrupt, Irq};
use crate::pad::Pad;

pub const MEMCARD_SIZE: usize = 128 * 1024;

/// Controller buttons, as bits of the mask given to [`Sio::set_buttons`]
/// (1 = pressed).
pub mod button {
    pub const SELECT: u16 = 1 << 0;
    pub const L3: u16 = 1 << 1;
    pub const R3: u16 = 1 << 2;
    pub const START: u16 = 1 << 3;
    pub const UP: u16 = 1 << 4;
    pub const RIGHT: u16 = 1 << 5;
    pub const DOWN: u16 = 1 << 6;
    pub const LEFT: u16 = 1 << 7;
    pub const L2: u16 = 1 << 8;
    pub const R2: u16 = 1 << 9;
    pub const L1: u16 = 1 << 10;
    pub const R1: u16 = 1 << 11;
    pub const TRIANGLE: u16 = 1 << 12;
    pub const CIRCLE: u16 = 1 << 13;
    pub const CROSS: u16 = 1 << 14;
    pub const SQUARE: u16 = 1 << 15;
}

/// A 128 KiB memory card, in the raw (.mcd/.mcr) format.
pub struct MemoryCard {
    pub data: Box<[u8]>,
    flag: u8,
    /// Set when the game wrote to the card, until [`MemoryCard::take_dirty`].
    dirty: bool,
    command: u8,
    step: u32,
    sector: u16,
    checksum: u8,
    last: u8,
    buffer: [u8; 128],
}

impl MemoryCard {
    /// A freshly formatted card.
    pub fn formatted() -> Self {
        let mut data = vec![0u8; MEMCARD_SIZE].into_boxed_slice();
        let mut frame = |n: usize, f: &dyn Fn(&mut [u8])| {
            let s = &mut data[n * 128..(n + 1) * 128];
            f(s);
            s[127] = s[..127].iter().fold(0, |a, b| a ^ b);
        };
        frame(0, &|s| s[..2].copy_from_slice(b"MC"));
        for n in 1..16 {
            frame(n, &|s| {
                s[0] = 0xa0;
                s[8..10].copy_from_slice(&[0xff, 0xff]);
            });
        }
        for n in 16..36 {
            frame(n, &|s| {
                s[..4].copy_from_slice(&[0xff; 4]);
                s[8..10].copy_from_slice(&[0xff, 0xff]);
            });
        }
        frame(63, &|s| s[..2].copy_from_slice(b"MC"));
        Self::from_data(data)
    }

    pub fn from_data(data: Box<[u8]>) -> Self {
        Self {
            data,
            flag: 0x08,
            dirty: false,
            command: 0,
            step: 0,
            sector: 0,
            checksum: 0,
            last: 0,
            buffer: [0; 128],
        }
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn reset(&mut self) {
        self.command = 0;
        self.step = 0;
    }

    /// Exchanges one byte; returns the response and whether the card acks.
    fn transfer(&mut self, v: u8) -> (u8, bool) {
        let step = self.step;
        self.step += 1;
        if step == 0 {
            return (0xff, true);
        }
        if step == 1 {
            self.command = v;
            return match v {
                b'R' | b'W' | b'S' => (self.flag, true),
                _ => (0xff, false),
            };
        }
        match (self.command, step) {
            (_, 2) => (0x5a, true),
            (_, 3) => (0x5d, true),
            (b'S', 4) => (0x5c, true),
            (b'S', 5) => (0x5d, true),
            (b'S', 6) => (0x04, true),
            (b'S', 7) => (0x00, true),
            (b'S', 8) => (0x00, true),
            (b'S', 9) => (0x80, false),
            (_, 4) => {
                self.sector = u16::from(v) << 8;
                self.checksum = v;
                (0x00, true)
            }
            (_, 5) => {
                self.sector |= u16::from(v);
                self.checksum ^= v;
                self.last = v;
                (self.sector.to_be_bytes()[0], true)
            }
            (b'R', _) => self.read_step(step),
            (b'W', _) => self.write_step(step, v),
            _ => (0xff, false),
        }
    }

    fn read_step(&mut self, step: u32) -> (u8, bool) {
        let valid = self.sector < 0x400;
        match step {
            6 => (0x5c, true),
            7 => (0x5d, true),
            8 => (if valid { self.sector.to_be_bytes()[0] } else { 0xff }, valid),
            9 => (self.sector.to_be_bytes()[1], true),
            10..=137 => {
                let b = self.data[usize::from(self.sector) * 128 + (step as usize - 10)];
                self.checksum ^= b;
                (b, true)
            }
            138 => (self.checksum, true),
            _ => (b'G', false),
        }
    }

    fn write_step(&mut self, step: u32, v: u8) -> (u8, bool) {
        match step {
            6..=133 => {
                self.buffer[step as usize - 6] = v;
                self.checksum ^= v;
                let prev = self.last;
                self.last = v;
                (prev, true)
            }
            134 => {
                let ok = v == self.checksum;
                self.checksum = u8::from(ok);
                (self.last, true)
            }
            135 => (0x5c, true),
            136 => (0x5d, true),
            _ => {
                if self.sector >= 0x400 {
                    return (0xff, false);
                }
                if self.checksum != 1 {
                    return (b'N', false);
                }
                let o = usize::from(self.sector) * 128;
                self.data[o..o + 128].copy_from_slice(&self.buffer);
                self.flag &= !0x08;
                self.dirty = true;
                (b'G', false)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Device {
    None,
    Pad,
    Card,
    /// Nothing answered: ignore bytes until deselected.
    Absent,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Sio {
    ctrl: u16,
    mode: u16,
    baud: u16,
    rx: Option<u8>,
    irq: bool,
    /// Byte being transferred: cycles left, response, ack.
    transfer: Option<(u32, u8, bool)>,
    /// Cycles until the /ACK interrupt.
    ack: Option<u32>,
    device: Device,
    pub pads: [Pad; 2],
    /// Not in save states: memory cards are saved separately.
    #[serde(skip)]
    pub cards: [Option<MemoryCard>; 2],
}

impl Default for Sio {
    fn default() -> Self {
        Self::new()
    }
}

impl Sio {
    pub fn new() -> Self {
        Self {
            ctrl: 0,
            mode: 0,
            baud: 0,
            rx: None,
            irq: false,
            transfer: None,
            ack: None,
            device: Device::None,
            // Both controllers plugged in: games look for the second one
            // when they offer two players.
            pads: [Pad::new(true), Pad::new(true)],
            cards: [Some(MemoryCard::formatted()), None],
        }
    }

    fn port(&self) -> usize {
        usize::from(self.ctrl & 0x2000 != 0)
    }

    fn deselect(&mut self) {
        self.device = Device::None;
        for pad in &mut self.pads {
            pad.reset_transfer();
        }
        for card in self.cards.iter_mut().flatten() {
            card.reset();
        }
    }

    pub fn read(&mut self, off: u32, size: u32) -> u32 {
        match off {
            0x0 => {
                let v = self.rx.take().unwrap_or(0xff);
                if size == 1 {
                    u32::from(v)
                } else {
                    u32::from(v) * 0x0101_0101
                }
            }
            0x4 => {
                let mut s = 1u32 | (1 << 2);
                s |= u32::from(self.rx.is_some()) << 1;
                if self.transfer.is_some() {
                    s &= !(1 << 2);
                }
                s |= u32::from(self.irq) << 9;
                s
            }
            0x8 => u32::from(self.mode),
            0xa => u32::from(self.ctrl),
            0xe => u32::from(self.baud),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, v: u32, _size: u32) {
        match off {
            0x0 => self.send(v as u8),
            0x8 => self.mode = v as u16,
            0xa => {
                let v = v as u16;
                if v & 0x40 != 0 {
                    self.ctrl = 0;
                    self.irq = false;
                    self.rx = None;
                    self.transfer = None;
                    self.ack = None;
                    self.deselect();
                    return;
                }
                if v & 0x10 != 0 {
                    self.irq = false;
                }
                self.ctrl = v & !0x50;
                if v & 2 == 0 {
                    self.deselect();
                }
            }
            0xe => self.baud = v as u16,
            _ => {}
        }
    }

    fn send(&mut self, v: u8) {
        let selected = self.ctrl & 2 != 0;
        let (resp, ack) = if selected { self.exchange(v) } else { (0xff, false) };
        let cycles = (u32::from(self.baud).max(1) * 8).max(400);
        self.transfer = Some((cycles, resp, ack));
    }

    fn exchange(&mut self, v: u8) -> (u8, bool) {
        let port = self.port();
        if self.device == Device::None {
            self.device = match v {
                0x01 if self.pads[port].connected => Device::Pad,
                0x81 if self.cards[port].is_some() => Device::Card,
                _ => Device::Absent,
            };
        }
        match self.device {
            Device::Pad => self.pads[port].transfer(v),
            Device::Card => match self.cards[port].as_mut() {
                Some(card) => card.transfer(v),
                None => (0xff, false),
            },
            _ => (0xff, false),
        }
    }

    /// The CPU cycles until a transfer or its acknowledge ends, if one is
    /// under way.
    pub fn next_event(&self) -> Option<u32> {
        match (self.transfer, self.ack) {
            (Some((left, ..)), _) => Some(left.max(1)),
            (None, Some(left)) => Some(left.max(1)),
            (None, None) => None,
        }
    }

    pub fn tick(&mut self, cycles: u32, irq: &mut Irq) {
        if let Some((left, resp, ack)) = self.transfer {
            if left > cycles {
                self.transfer = Some((left - cycles, resp, ack));
            } else {
                self.transfer = None;
                self.rx = Some(resp);
                if ack {
                    self.ack = Some(100);
                } else {
                    self.device = Device::Absent;
                }
            }
        }
        if let Some(left) = self.ack {
            if left > cycles {
                self.ack = Some(left - cycles);
            } else {
                self.ack = None;
                if self.ctrl & 0x1000 != 0 {
                    self.irq = true;
                    irq.raise(Interrupt::Controller);
                }
            }
        }
    }
}
