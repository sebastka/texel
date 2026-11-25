//! The IOP's USB host controller: an OHCI one (the Open Host Controller
//! Interface's registers, at 0x1F801600), with its root hub's two ports.
//!
//! Games load Sony's USB driver for keyboards, cameras, headsets and the
//! like, and its start waits for the controller: once it resets it (bit
//! HCR of HcCommandStatus), until the bit clears. Without a controller the
//! bit stayed set, the module never finished loading, and the game waited
//! for it forever (ten games of a library of 46).
//!
//! Nothing is plugged in: the ports report no device, so the driver never
//! queues a transfer, and the controller walks no list. It counts frames
//! (one a millisecond, written to the HCCA's frame number) and raises its
//! interrupts (start of frame, frame number overflow) when the driver
//! enables them.

use crate::iop::Intc;

/// IOP interrupt source (bit of I_STAT).
pub const IRQ: u32 = 22;

/// IOP cycles a USB frame (a millisecond) lasts.
const FRAME_CYCLES: u64 = 36_864;

// Registers (offsets).
const REVISION: u32 = 0x00;
const CONTROL: u32 = 0x04;
const COMMAND_STATUS: u32 = 0x08;
const INTERRUPT_STATUS: u32 = 0x0C;
const INTERRUPT_ENABLE: u32 = 0x10;
const INTERRUPT_DISABLE: u32 = 0x14;
const HCCA: u32 = 0x18;
const FM_INTERVAL: u32 = 0x34;
const FM_REMAINING: u32 = 0x38;
const FM_NUMBER: u32 = 0x3C;
const LS_THRESHOLD: u32 = 0x44;
const RH_DESCRIPTOR_A: u32 = 0x48;
const RH_STATUS: u32 = 0x50;
const RH_PORT_STATUS: u32 = 0x54;
const PORTS: u32 = 2;

/// HcControl's functional state, and the operational one.
const HCFS: u32 = 3 << 6;
const OPERATIONAL: u32 = 2 << 6;
const SUSPEND: u32 = 3 << 6;
/// Interrupts: start of frame, frame number overflow, master enable.
const SF: u32 = 1 << 2;
const FNO: u32 = 1 << 5;
const MIE: u32 = 1 << 31;
/// A port's power status (powered, nothing connected).
const PPS: u32 = 1 << 8;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Ohci {
    /// Registers by offset / 4 (those kept as written, and the state).
    regs: [u32; 0x20],
    /// IOP cycles into the current frame.
    cycles: u64,
    /// The interrupt line, as last raised (raised again on a new cause).
    raised: bool,
    /// Diagnostics: accesses (write, offset, value), the first 256.
    #[serde(skip)]
    pub log: Vec<(bool, u32, u32)>,
}

impl Default for Ohci {
    fn default() -> Self {
        let mut o = Self {
            regs: [0; 0x20],
            cycles: 0,
            raised: false,
            log: Vec::new(),
        };
        o.reset();
        o
    }
}

impl Ohci {
    /// The registers after a reset (hardware or HCR): suspended, the
    /// frame interval and thresholds at their defaults.
    fn reset(&mut self) {
        let hcca = self.regs[(HCCA / 4) as usize];
        self.regs = [0; 0x20];
        self.regs[(HCCA / 4) as usize] = hcca;
        self.regs[(CONTROL / 4) as usize] = SUSPEND;
        self.regs[(FM_INTERVAL / 4) as usize] = 0x2EDF;
        self.regs[(LS_THRESHOLD / 4) as usize] = 0x628;
        // Two ports, not power switched (always powered), and the time to
        // power good.
        self.regs[(RH_DESCRIPTOR_A / 4) as usize] = PORTS | 1 << 9 | 1 << 24;
        self.cycles = 0;
        self.raised = false;
    }

    fn reg(&self, offset: u32) -> u32 {
        self.regs.get((offset / 4) as usize).copied().unwrap_or(0)
    }

    pub fn read(&mut self, offset: u32) -> u32 {
        let v = match offset {
            REVISION => 0x10,
            INTERRUPT_DISABLE => self.reg(INTERRUPT_ENABLE),
            FM_REMAINING => {
                let left = FRAME_CYCLES.saturating_sub(self.cycles);
                (left * u64::from(self.reg(FM_INTERVAL) & 0x3FFF) / FRAME_CYCLES) as u32
            }
            o if (RH_PORT_STATUS..RH_PORT_STATUS + 4 * PORTS).contains(&o) => PPS | self.reg(o),
            o => self.reg(o),
        };
        self.note(false, offset, v);
        v
    }

    pub fn write(&mut self, offset: u32, v: u32, intc: &mut Intc) {
        self.note(true, offset, v);
        let i = (offset / 4) as usize;
        match offset {
            REVISION | FM_REMAINING | FM_NUMBER => {}
            COMMAND_STATUS => {
                // HCR: a reset, done at once (the bit reads 0 again).
                if v & 1 != 0 {
                    self.reset();
                }
                self.regs[i] |= v & 0x0003_000E;
            }
            // Written 1 to clear.
            INTERRUPT_STATUS => self.regs[i] &= !v,
            INTERRUPT_ENABLE => self.regs[i] |= v,
            INTERRUPT_DISABLE => self.regs[(INTERRUPT_ENABLE / 4) as usize] &= !v,
            HCCA => self.regs[i] = v & !0xFF,
            RH_STATUS => {
                // Its change bits are written 1 to clear; nothing else to
                // keep (no power switching, no over-current).
                self.regs[i] &= !(v & 0x0002_0000);
            }
            o if (RH_PORT_STATUS..RH_PORT_STATUS + 4 * PORTS).contains(&o) => {
                // Change bits (16 to 20) written 1 to clear; resetting or
                // enabling a port with nothing connected does nothing.
                self.regs[i] &= !(v & 0x001F_0000);
            }
            _ => {
                if let Some(r) = self.regs.get_mut(i) {
                    *r = v;
                }
            }
        }
        self.update_irq(intc);
    }

    /// Advances `cycles` IOP cycles: frames go by while operational, each
    /// written to the HCCA (`ram`, the IOP's) and raising SF.
    pub fn advance(&mut self, cycles: u64, ram: &mut [u8], intc: &mut Intc) {
        if self.reg(CONTROL) & HCFS != OPERATIONAL {
            return;
        }
        self.cycles += cycles;
        while self.cycles >= FRAME_CYCLES {
            self.cycles -= FRAME_CYCLES;
            let n = (self.reg(FM_NUMBER) + 1) & 0xFFFF;
            self.regs[(FM_NUMBER / 4) as usize] = n;
            // The HCCA's frame number (and its pad, zero), at 0x80.
            let at = (self.reg(HCCA) as usize & (ram.len() - 1)) + 0x80;
            if let Some(w) = ram.get_mut(at..at + 4) {
                w.copy_from_slice(&n.to_le_bytes());
            }
            let mut status = SF;
            if n & 0x7FFF == 0 {
                status |= FNO;
            }
            self.regs[(INTERRUPT_STATUS / 4) as usize] |= status;
        }
        self.update_irq(intc);
    }

    /// Raises the interrupt when an enabled cause is pending.
    fn update_irq(&mut self, intc: &mut Intc) {
        let enable = self.reg(INTERRUPT_ENABLE);
        let pending = enable & MIE != 0 && self.reg(INTERRUPT_STATUS) & enable & !MIE != 0;
        if pending && !self.raised {
            intc.raise(IRQ);
        }
        self.raised = pending;
    }

    fn note(&mut self, write: bool, offset: u32, v: u32) {
        if self.log.len() < 256 {
            self.log.push((write, offset, v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_ends_at_once_and_frames_count() {
        let mut intc = Intc::default();
        let mut ram = vec![0u8; 0x20_0000];
        let mut o = Ohci::default();
        o.write(COMMAND_STATUS, 1, &mut intc);
        assert_eq!(o.read(COMMAND_STATUS) & 1, 0);
        assert_eq!(o.read(CONTROL) & HCFS, SUSPEND);
        o.write(HCCA, 0x1_0000, &mut intc);
        o.write(INTERRUPT_ENABLE, MIE | SF, &mut intc);
        // Suspended: no frames.
        o.advance(10 * FRAME_CYCLES, &mut ram, &mut intc);
        assert_eq!(o.read(FM_NUMBER), 0);
        o.write(CONTROL, OPERATIONAL, &mut intc);
        o.advance(3 * FRAME_CYCLES + 5, &mut ram, &mut intc);
        assert_eq!(o.read(FM_NUMBER), 3);
        assert_eq!(&ram[0x1_0080..0x1_0084], &[3, 0, 0, 0]);
        assert_ne!(intc.stat & (1 << IRQ), 0);
        // Acknowledged: the line falls, and rises with the next frame.
        intc.stat = 0;
        o.write(INTERRUPT_STATUS, SF, &mut intc);
        o.advance(FRAME_CYCLES, &mut ram, &mut intc);
        assert_ne!(intc.stat & (1 << IRQ), 0);
    }

    #[test]
    fn two_empty_powered_ports() {
        let mut o = Ohci::default();
        assert_eq!(o.read(RH_DESCRIPTOR_A) & 0xFF, 2);
        assert_eq!(o.read(RH_PORT_STATUS), PPS);
        assert_eq!(o.read(RH_PORT_STATUS + 4), PPS);
    }
}
