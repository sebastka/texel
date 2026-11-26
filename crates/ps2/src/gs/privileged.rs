//! The Graphics Synthesizer's privileged registers, mapped at 0x12000000:
//! display setup (PMODE, SMODE, DISPFB, DISPLAY, BGCOLOR...) and CSR, its
//! status and control.

/// CSR: revision and ID in the upper half.
const CSR_ID: u64 = 0x551B_0000;
/// CSR: the FIFO is empty.
const CSR_FIFO_EMPTY: u64 = 1 << 14;
/// CSR events, set by the GS and cleared by writing 1.
pub const CSR_VSINT: u64 = 1 << 3;
const CSR_EVENTS: u64 = 0x1F;
pub const CSR_SIGNAL: u64 = 1 << 0;
pub const CSR_FINISH: u64 = 1 << 1;
const CSR_RESET: u64 = 1 << 9;
const CSR_FIELD: u64 = 1 << 13;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Privileged {
    /// The registers at 0x12000000-0x120000F0, by index (offset / 16).
    pub regs: [u64; 16],
    pub csr: u64,
    pub imr: u64,
    pub siglblid: u64,
    /// Register writes, for diagnostics: (offset, value).
    pub writes: Vec<(u32, u64)>,
}

impl Default for Privileged {
    fn default() -> Self {
        Self {
            regs: [0; 16],
            csr: 0,
            imr: 0x7F00,
            siglblid: 0,
            writes: Vec::new(),
        }
    }
}

/// Names of the registers at 0x12000000 + 16 * index.
pub const REG_NAMES: [&str; 16] = [
    "PMODE", "SMODE1", "SMODE2", "SRFSH", "SYNCH1", "SYNCH2", "SYNCV", "DISPFB1", "DISPLAY1", "DISPFB2", "DISPLAY2",
    "EXTBUF", "EXTDATA", "EXTWRITE", "BGCOLOR", "?",
];

impl Privileged {
    /// Whether the CRTC is set for PAL: SYNCV's parts (front porch, back
    /// porch, sync, display, in half-lines) make 625 half-lines a field
    /// for PAL, 525 for NTSC.
    pub fn pal(&self) -> bool {
        let v = self.regs[6];
        let part = |lo: u32, bits: u32| (v >> lo) & ((1 << bits) - 1);
        let half_lines = part(0, 10) + part(10, 10) + part(20, 12) + part(32, 10) + part(42, 11) + part(53, 11);
        (600..=650).contains(&half_lines)
    }

    /// Starts a vertical blank: the VSINT event, and the next field.
    pub fn vblank(&mut self) {
        self.csr |= CSR_VSINT;
        self.csr ^= CSR_FIELD;
    }

    fn csr_value(&self) -> u64 {
        CSR_ID | CSR_FIFO_EMPTY | (self.csr & (CSR_EVENTS | CSR_FIELD))
    }

    /// `offset`: from 0x12000000.
    pub fn read64(&self, offset: u32) -> u64 {
        match offset {
            0x0000..=0x00FF => self.regs[(offset >> 4) as usize & 15],
            0x1000 => self.csr_value(),
            0x1010 => self.imr,
            0x1080 => self.siglblid,
            _ => 0,
        }
    }

    pub fn write64(&mut self, offset: u32, value: u64) {
        if self.writes.len() < 4096 {
            self.writes.push((offset, value));
        }
        match offset {
            0x0000..=0x00FF => self.regs[(offset >> 4) as usize & 15] = value,
            0x1000 => {
                self.csr &= !(value & CSR_EVENTS);
                if value & CSR_RESET != 0 {
                    self.csr &= CSR_FIELD;
                }
            }
            0x1010 => self.imr = value,
            0x1080 => self.siglblid = value,
            _ => {}
        }
    }

    /// 32-bit accesses reach either half of a 64-bit register.
    pub fn read32(&self, offset: u32) -> u32 {
        (self.read64(offset & !7) >> ((offset & 4) * 8)) as u32
    }

    pub fn write32(&mut self, offset: u32, value: u32) {
        let base = offset & !7;
        let shift = (offset & 4) * 8;
        let old = match base {
            // CSR: writing one half must not act on the other's bits.
            0x1000 => 0,
            _ => self.read64(base),
        };
        let merged = (old & !(0xFFFF_FFFF << shift)) | (u64::from(value) << shift);
        self.write64(base, merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_events() {
        let mut gs = Privileged::default();
        assert_eq!(gs.read32(0x1000) >> 16, 0x551B);
        gs.vblank();
        assert_ne!(gs.read64(0x1000) & CSR_VSINT, 0);
        gs.write64(0x1000, CSR_VSINT);
        assert_eq!(gs.read64(0x1000) & CSR_VSINT, 0);
        gs.write32(0x0E4, 0x1234);
        assert_eq!(gs.read64(0x0E0), 0x1234_0000_0000);
    }
}
