//! COP0: the R5900's system control coprocessor. Status and Cause for
//! exceptions and interrupts, Count and Compare (a timer interrupt), and
//! the TLB (see tlb.rs for translation).

pub const INDEX: usize = 0;
pub const RANDOM: usize = 1;
pub const ENTRY_LO0: usize = 2;
pub const ENTRY_LO1: usize = 3;
pub const CONTEXT: usize = 4;
pub const PAGE_MASK: usize = 5;
pub const WIRED: usize = 6;
pub const BAD_VADDR: usize = 8;
pub const COUNT: usize = 9;
pub const ENTRY_HI: usize = 10;
pub const COMPARE: usize = 11;
pub const STATUS: usize = 12;
pub const CAUSE: usize = 13;
pub const EPC: usize = 14;
pub const PRID: usize = 15;
pub const CONFIG: usize = 16;
pub const ERROR_EPC: usize = 30;

pub const STATUS_IE: u32 = 1 << 0;
pub const STATUS_EXL: u32 = 1 << 1;
pub const STATUS_ERL: u32 = 1 << 2;
pub const STATUS_EIE: u32 = 1 << 16;
pub const STATUS_EDI: u32 = 1 << 17;
pub const STATUS_BEV: u32 = 1 << 22;
const KSU: u32 = 3 << 3;

/// The EE's implementation and revision.
const EE_PRID: u32 = 0x2E20;

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TlbEntry {
    pub page_mask: u32,
    pub entry_hi: u32,
    pub entry_lo0: u32,
    pub entry_lo1: u32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Cop0 {
    pub regs: [u32; 32],
    #[serde(with = "serde_big_array::BigArray")]
    pub tlb: [TlbEntry; 48],
    /// Performance counter registers (MFPC/MTPC) and debug registers
    /// (breakpoints), kept as written.
    perf: [u32; 3],
    debug: [u32; 8],
    /// The TLB, expanded for translation (see tlb.rs).
    #[serde(skip)]
    pub(super) pages: super::tlb::PageTable,
}

impl Cop0 {
    pub fn new() -> Self {
        let mut regs = [0; 32];
        regs[STATUS] = STATUS_BEV | STATUS_ERL;
        regs[PRID] = EE_PRID;
        regs[CONFIG] = 0x440;
        regs[RANDOM] = 47;
        Self {
            regs,
            tlb: [TlbEntry::default(); 48],
            perf: [0; 3],
            debug: [0; 8],
            pages: super::tlb::PageTable::default(),
        }
    }

    pub fn exl(&self) -> bool {
        self.regs[STATUS] & STATUS_EXL != 0
    }

    pub fn set_bd(&mut self, bd: bool) {
        self.regs[CAUSE] = (self.regs[CAUSE] & !(1 << 31)) | (u32::from(bd) << 31);
    }

    /// Sets an interrupt pending bit: IP2 (INTC), IP3 (DMAC), IP7 (Count).
    pub fn set_ip(&mut self, n: u32, on: bool) {
        let bit = 1 << (8 + n);
        if on {
            self.regs[CAUSE] |= bit;
        } else {
            self.regs[CAUSE] &= !bit;
        }
    }

    /// Interrupts are taken when IE and EIE are set, outside exceptions.
    pub fn interrupt_pending(&self) -> bool {
        let s = self.regs[STATUS];
        let enabled = s & STATUS_IE != 0 && s & STATUS_EIE != 0 && s & (STATUS_EXL | STATUS_ERL) == 0;
        // IM2, IM3 and IM7 are Status bits 10, 11 and 15, matching Cause.
        enabled && self.regs[CAUSE] & s & 0x8C00 != 0
    }

    /// Count runs at the CPU clock; reaching Compare raises IP7.
    pub fn tick(&mut self, cycles: u32) {
        let before = self.regs[COUNT];
        let after = before.wrapping_add(cycles);
        self.regs[COUNT] = after;
        let compare = self.regs[COMPARE];
        if compare.wrapping_sub(before).wrapping_sub(1) < cycles {
            self.set_ip(7, true);
        }
    }

    /// MFC0. `low`: the instruction's low bits, which select among the
    /// debug and performance registers (24 and 25).
    pub fn read(&self, reg: u32, low: u32) -> u32 {
        match reg as usize {
            24 => self.debug[(low & 7) as usize],
            25 => self.perf[((low >> 1) & 1) as usize + usize::from(low & 1 == 1)],
            r => self.regs[r],
        }
    }

    pub fn write(&mut self, reg: u32, low: u32, v: u32) {
        match reg as usize {
            // Read-only: Random, BadVAddr, PRId.
            RANDOM | BAD_VADDR | PRID => {}
            ENTRY_HI => {
                self.regs[ENTRY_HI] = v;
                self.pages.asid_changed(v & 0xFF);
            }
            COMPARE => {
                self.regs[COMPARE] = v;
                self.set_ip(7, false);
            }
            CAUSE => {
                // Only the software interrupt bits are writable (none on
                // the EE are used): keep the rest.
                self.regs[CAUSE] = (self.regs[CAUSE] & !0x300) | (v & 0x300);
            }
            24 => self.debug[(low & 7) as usize] = v,
            25 => self.perf[((low >> 1) & 1) as usize + usize::from(low & 1 == 1)] = v,
            r => self.regs[r] = v,
        }
    }

    pub fn eret(&mut self) -> u32 {
        let s = &mut self.regs[STATUS];
        if *s & STATUS_ERL != 0 {
            *s &= !STATUS_ERL;
            self.regs[ERROR_EPC]
        } else {
            *s &= !STATUS_EXL;
            self.regs[EPC]
        }
    }

    /// EI and DI work in kernel mode, during exceptions, or with EDI set.
    fn may_toggle_interrupts(&self) -> bool {
        let s = self.regs[STATUS];
        s & STATUS_EDI != 0 || s & (STATUS_EXL | STATUS_ERL) != 0 || s & KSU == 0
    }

    pub fn ei(&mut self) {
        if self.may_toggle_interrupts() {
            self.regs[STATUS] |= STATUS_EIE;
        }
    }

    pub fn di(&mut self) {
        if self.may_toggle_interrupts() {
            self.regs[STATUS] &= !STATUS_EIE;
        }
    }

    fn entry(&self) -> TlbEntry {
        TlbEntry {
            page_mask: self.regs[PAGE_MASK],
            entry_hi: self.regs[ENTRY_HI],
            entry_lo0: self.regs[ENTRY_LO0],
            entry_lo1: self.regs[ENTRY_LO1],
        }
    }

    pub fn tlbr(&mut self) {
        let e = self.tlb[(self.regs[INDEX] & 63) as usize % 48];
        self.regs[PAGE_MASK] = e.page_mask;
        self.regs[ENTRY_HI] = e.entry_hi;
        self.pages.asid_changed(e.entry_hi & 0xFF);
        self.regs[ENTRY_LO0] = e.entry_lo0;
        self.regs[ENTRY_LO1] = e.entry_lo1;
    }

    pub fn tlbwi(&mut self) {
        let i = (self.regs[INDEX] & 63) as usize % 48;
        self.tlb[i] = self.entry();
        self.pages.invalidate();
    }

    pub fn tlbwr(&mut self) {
        let wired = self.regs[WIRED] as usize % 48;
        let random = self.regs[RANDOM] as usize % 48;
        let i = random.max(wired);
        self.tlb[i] = self.entry();
        self.pages.invalidate();
        self.regs[RANDOM] = if random <= wired { 47 } else { random as u32 - 1 };
    }

    pub fn tlbp(&mut self) {
        let hi = self.regs[ENTRY_HI];
        let found = self.tlb.iter().position(|e| {
            let mask = !(e.page_mask | 0x1FFF);
            let global = e.entry_lo0 & e.entry_lo1 & 1 != 0;
            (e.entry_hi & mask) == (hi & mask) && (global || (e.entry_hi & 0xFF) == (hi & 0xFF))
        });
        self.regs[INDEX] = match found {
            Some(i) => i as u32,
            None => 0x8000_0000,
        };
    }
}

impl Default for Cop0 {
    fn default() -> Self {
        Self::new()
    }
}
