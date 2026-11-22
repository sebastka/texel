//! The EE's four timers: 16-bit counters on the bus clock (or a fraction
//! of it, or horizontal blanks), with compare and overflow interrupts.

use crate::intc::{Intc, Interrupt};

const IRQ: [Interrupt; 4] = [
    Interrupt::Timer0,
    Interrupt::Timer1,
    Interrupt::Timer2,
    Interrupt::Timer3,
];

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Timer {
    pub count: u32,
    pub mode: u32,
    pub compare: u32,
    pub hold: u32,
    /// Bus cycles not yet counted, for divided clocks.
    remainder: u64,
}

impl Timer {
    fn enabled(&self) -> bool {
        self.mode & 0x80 != 0
    }

    /// Bus cycles per count; `None` when counting horizontal blanks.
    fn divider(&self) -> Option<u64> {
        match self.mode & 3 {
            0 => Some(1),
            1 => Some(16),
            2 => Some(256),
            _ => None,
        }
    }

    /// Counts `n` ticks; returns whether to interrupt.
    fn count_ticks(&mut self, n: u64) -> bool {
        let mut irq = false;
        let mut left = n;
        while left > 0 {
            // Up to the next event: the compare value, or the overflow.
            let to_compare = if self.compare > self.count {
                u64::from(self.compare - self.count)
            } else {
                u64::MAX
            };
            let to_overflow = u64::from(0x1_0000 - self.count);
            let step = left.min(to_compare).min(to_overflow);
            self.count += step as u32;
            left -= step;
            if self.count == self.compare {
                if self.mode & 0x100 != 0 && self.mode & 0x400 == 0 {
                    self.mode |= 0x400;
                    irq = true;
                }
                // ZRET: back to zero on reaching the compare value.
                if self.mode & 0x40 != 0 {
                    self.count = 0;
                }
            }
            if self.count >= 0x1_0000 {
                self.count = 0;
                if self.mode & 0x200 != 0 && self.mode & 0x800 == 0 {
                    self.mode |= 0x800;
                    irq = true;
                }
            }
            if step == 0 {
                break;
            }
        }
        irq
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Timers {
    pub t: [Timer; 4],
}

impl Timers {
    /// Advances the timers by `bus_cycles`, and `hblanks` horizontal blanks.
    pub fn advance(&mut self, bus_cycles: u64, hblanks: u64, intc: &mut Intc) {
        for (i, t) in self.t.iter_mut().enumerate() {
            if !t.enabled() {
                continue;
            }
            let ticks = match t.divider() {
                Some(d) => {
                    t.remainder += bus_cycles;
                    let n = t.remainder / d;
                    t.remainder %= d;
                    n
                }
                None => hblanks,
            };
            if ticks > 0 && t.count_ticks(ticks) {
                intc.raise(IRQ[i]);
            }
        }
    }

    /// `offset`: within the timers' range, 0x10000000-0x10001FFF.
    pub fn read(&self, offset: u32) -> u32 {
        let t = &self.t[(offset >> 11) as usize & 3];
        match offset & 0x7F0 {
            0x00 => t.count,
            0x10 => t.mode,
            0x20 => t.compare,
            0x30 => t.hold,
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u32, value: u32) {
        let t = &mut self.t[(offset >> 11) as usize & 3];
        match offset & 0x7F0 {
            0x00 => t.count = value & 0xFFFF,
            0x10 => {
                // EQUF and OVFF are cleared by writing 1.
                let flags = t.mode & 0xC00 & !value;
                t.mode = (value & 0x3FF) | flags;
                t.remainder = 0;
            }
            0x20 => t.compare = value & 0xFFFF,
            0x30 => t.hold = value & 0xFFFF,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_interrupt_and_zero_return() {
        let mut timers = Timers::default();
        let mut intc = Intc::default();
        timers.write(0x20, 100);
        // Enabled, bus clock, ZRET, compare interrupt.
        timers.write(0x10, 0x80 | 0x40 | 0x100);
        timers.advance(99, 0, &mut intc);
        assert_eq!(intc.stat, 0);
        timers.advance(1, 0, &mut intc);
        assert_eq!(intc.stat, 1 << 9);
        assert_eq!(timers.read(0x00), 0);
        assert_ne!(timers.read(0x10) & 0x400, 0);
        // Written 1: cleared.
        timers.write(0x10, 0x80 | 0x40 | 0x100 | 0x400);
        assert_eq!(timers.read(0x10) & 0x400, 0);
    }

    #[test]
    fn divided_clock_and_overflow() {
        let mut timers = Timers::default();
        let mut intc = Intc::default();
        // Timer 3, 1/256 of the bus clock, overflow interrupt.
        timers.write(0x1800, 0xFFFF);
        timers.write(0x1810, 0x80 | 2 | 0x200);
        timers.advance(255, 0, &mut intc);
        assert_eq!(timers.read(0x1800), 0xFFFF);
        timers.advance(1, 0, &mut intc);
        assert_eq!(timers.read(0x1800), 0);
        assert_eq!(intc.stat, 1 << 12);
    }
}
