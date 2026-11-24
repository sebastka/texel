//! The EE's interrupt controller (INTC) and the DMA controller's interrupt
//! status: the two interrupt lines of the EE, INT0 and INT1.

/// INTC sources, by bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    Gs = 0,
    Sbus = 1,
    VblankStart = 2,
    VblankEnd = 3,
    Vif0 = 4,
    Vif1 = 5,
    Vu0 = 6,
    Vu1 = 7,
    Ipu = 8,
    Timer0 = 9,
    Timer1 = 10,
    Timer2 = 11,
    Timer3 = 12,
    Sfifo = 13,
    Vu0Watchdog = 14,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Intc {
    /// INTC_STAT: raised sources; written 1 to clear.
    pub stat: u32,
    /// INTC_MASK: enabled sources; written 1 to toggle.
    pub mask: u32,
    /// The vertical blanks the bus's clock raised in its last advance: INT0
    /// waits for them until the EE's next read of INTC_STAT, a few cycles at
    /// most (`Ps2::run`). Code polling INTC_STAT sees them, as on the
    /// console, where a load of INTC_STAT under way when the source rises
    /// returns it, and the interrupt is taken right after (games wait for
    /// VBON so, interrupts on: the kernel's handler would clear it first).
    /// Never outlives a slice: not saved.
    #[serde(skip)]
    pub fresh: u32,
}

impl Intc {
    pub fn raise(&mut self, i: Interrupt) {
        self.stat |= 1 << i as u32;
    }

    pub fn write_stat(&mut self, value: u32) {
        self.stat &= !value;
    }

    pub fn write_mask(&mut self, value: u32) {
        self.mask ^= value & 0x7FFF;
    }

    /// INT0, wired to the EE's IP2: the sources raised and enabled, but
    /// those just raised by the clock (`fresh`).
    pub fn line(&self) -> bool {
        self.stat & !self.fresh & self.mask != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_toggles_and_stat_clears() {
        let mut i = Intc::default();
        i.raise(Interrupt::VblankStart);
        assert!(!i.line());
        i.write_mask(1 << 2);
        assert!(i.line());
        i.write_mask(1 << 2);
        assert!(!i.line());
        i.write_mask(1 << 2);
        i.write_stat(1 << 2);
        assert!(!i.line());
    }

    #[test]
    fn fresh_sources_wait() {
        let mut i = Intc::default();
        i.write_mask(1 << 2);
        i.raise(Interrupt::VblankStart);
        i.fresh = 1 << 2;
        assert!(!i.line(), "a vertical blank waits a few cycles");
        i.fresh = 0;
        assert!(i.line());
    }
}
