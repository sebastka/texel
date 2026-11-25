//! The three root counters.

use crate::gpu::{Gpu, GpuTick};
use crate::irq::{Interrupt, Irq};

#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct Timer {
    counter: u32,
    mode: u32,
    target: u32,
    /// Sub-tick remainder for divided clocks.
    frac: u32,
    /// Whether a non-repeating timer already fired.
    fired: bool,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Timers {
    t: [Timer; 3],
}

impl Timers {
    pub fn read(&mut self, off: u32) -> u32 {
        let i = (off >> 4) as usize;
        let Some(t) = self.t.get_mut(i) else { return 0 };
        match off & 0xf {
            0 => t.counter,
            4 => {
                let v = t.mode;
                // "Reached" flags reset on read.
                t.mode &= !((1 << 11) | (1 << 12));
                v
            }
            8 => t.target,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, v: u32) {
        let i = (off >> 4) as usize;
        let Some(t) = self.t.get_mut(i) else { return };
        match off & 0xf {
            0 => t.counter = v & 0xffff,
            4 => {
                t.mode = (v & 0x3ff) | (1 << 10);
                t.counter = 0;
                t.fired = false;
            }
            8 => t.target = v & 0xffff,
            _ => {}
        }
    }

    pub fn tick(&mut self, cycles: u32, gpu: &GpuTick, irq: &mut Irq) {
        for i in 0..3 {
            let t = &mut self.t[i];
            let source = (t.mode >> 8) & 3;
            let ticks = match (i, source) {
                (0, 1 | 3) => {
                    // Dot clock: GPU clock (CPU * 11/7) / divider.
                    t.frac += cycles * 11;
                    let div = 7 * gpu.dot_divider;
                    let n = t.frac / div;
                    t.frac %= div;
                    n
                }
                (1, 1 | 3) => gpu.hblanks,
                (2, 2 | 3) => {
                    t.frac += cycles;
                    let n = t.frac / 8;
                    t.frac %= 8;
                    n
                }
                _ => cycles,
            };
            // Timer 2 in sync modes 0 and 3 is stopped.
            if i == 2 && t.mode & 1 != 0 && matches!((t.mode >> 1) & 3, 0 | 3) {
                continue;
            }
            // Timer 1, sync mode 1: reset at each vblank.
            if i == 1 && t.mode & 1 != 0 && (t.mode >> 1) & 3 == 1 && gpu.vblank_start {
                t.counter = 0;
            }
            if ticks > 0 && Self::advance(t, ticks) {
                irq.raise([Interrupt::Timer0, Interrupt::Timer1, Interrupt::Timer2][i]);
            }
        }
    }

    /// The CPU cycles until a timer may raise its interrupt (reaching its
    /// target or overflowing), if one can.
    pub fn next_event(&self, gpu: &Gpu) -> Option<u32> {
        let mut next: Option<u32> = None;
        for (i, t) in self.t.iter().enumerate() {
            // No interrupt, or a one-shot one already raised.
            if t.mode & 0x30 == 0 || (t.mode & (1 << 6) == 0 && t.fired) {
                continue;
            }
            if i == 2 && t.mode & 1 != 0 && matches!((t.mode >> 1) & 3, 0 | 3) {
                continue;
            }
            let ticks = if t.target == 0 && t.counter == 0 {
                1
            } else if t.counter < t.target {
                t.target - t.counter
            } else {
                0x1_0000 - t.counter.min(0xffff)
            };
            let cycles = match (i, (t.mode >> 8) & 3) {
                (0, 1 | 3) => {
                    let need = u64::from(ticks) * 7 * u64::from(gpu.dot_divider());
                    need.saturating_sub(u64::from(t.frac))
                        .div_ceil(11)
                        .min(u64::from(u32::MAX)) as u32
                }
                (1, 1 | 3) => gpu.cycles_to_lines(ticks),
                (2, 2 | 3) => (ticks * 8).saturating_sub(t.frac),
                _ => ticks,
            };
            next = Some(next.map_or(cycles, |n| n.min(cycles)));
        }
        next
    }

    /// Returns whether an interrupt fires.
    fn advance(t: &mut Timer, ticks: u32) -> bool {
        let before = t.counter;
        let mut after = before + ticks;
        let mut irq = false;
        let target = t.target;
        if before < target && after >= target || (target == 0 && before == 0) {
            t.mode |= 1 << 11;
            irq |= t.mode & (1 << 4) != 0;
            if t.mode & (1 << 3) != 0 {
                after = if target == 0 {
                    0
                } else {
                    (after - target) % target.max(1)
                };
            }
        }
        if after > 0xffff {
            t.mode |= 1 << 12;
            irq |= t.mode & (1 << 5) != 0;
            after &= 0xffff;
        }
        t.counter = after;
        if !irq {
            return false;
        }
        // One-shot timers fire once until the mode is written again.
        if t.mode & (1 << 6) == 0 {
            if t.fired {
                return false;
            }
            t.fired = true;
        }
        if t.mode & (1 << 7) != 0 {
            // Toggle mode: IRQ when bit 10 goes low.
            t.mode ^= 1 << 10;
            t.mode & (1 << 10) == 0
        } else {
            // Pulse mode: bit 10 back high right away.
            true
        }
    }
}
