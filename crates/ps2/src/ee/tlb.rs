//! Address translation: kseg0 and kseg1 are fixed windows on physical
//! memory; everything else goes through the 48-entry TLB.
//!
//! The TLB is searched once, when it changes, not on every access: its
//! entries are expanded into a table with one word per 4 KiB page of the
//! address space (physical page and flags), so that translating is one
//! table read. The table is rebuilt when an entry is written, or when the
//! address space ID changes while some entry depends on it.

use super::cop0::{self, Cop0, TlbEntry};
use crate::bus::SCRATCHPAD;

/// Table entries: the physical page, and these flags. 0: no entry.
const MAPPED: u32 = 1;
const VALID: u32 = 2;
const DIRTY: u32 = 4;

/// EntryLo: the scratchpad (EntryLo0 only), dirty (writable), valid, global.
const LO_SCRATCHPAD: u32 = 1 << 31;
const LO_DIRTY: u32 = 1 << 2;
const LO_VALID: u32 = 1 << 1;
const LO_GLOBAL: u32 = 1;

/// Why a translation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// No entry: the TLB refill exception.
    Refill,
    /// The entry is not valid.
    Invalid,
    /// A write to a page that is not dirty (writable).
    Modified,
}

#[derive(Clone)]
pub struct PageTable {
    pages: Box<[u32]>,
    /// The pages set, to clear them on rebuilding.
    filled: Vec<u32>,
    /// The ASID the table was built for.
    asid: u32,
    /// Some entry is not global: the table depends on the ASID.
    asid_dependent: bool,
    stale: bool,
    /// Counts the changes of mapping (each time the table goes stale).
    generation: u32,
}

impl std::fmt::Debug for PageTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTable")
            .field("pages", &self.filled.len())
            .finish_non_exhaustive()
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self {
            pages: vec![0; 1 << 20].into_boxed_slice(),
            filled: Vec::new(),
            asid: 0,
            asid_dependent: false,
            stale: true,
            generation: 0,
        }
    }
}

impl PageTable {
    /// Where the table and its `stale` flag are (for compiled code, which
    /// reads them directly).
    pub fn addresses(&self) -> (usize, usize) {
        (self.pages.as_ptr() as usize, &self.stale as *const bool as usize)
    }

    pub fn invalidate(&mut self) {
        self.stale = true;
        self.generation = self.generation.wrapping_add(1);
    }

    /// EntryHi changed: rebuild if the ASID matters.
    pub fn asid_changed(&mut self, asid: u32) {
        if self.asid_dependent && asid != self.asid {
            self.invalidate();
        }
    }

    fn rebuild(&mut self, regs: &[u32; 32], tlb: &[TlbEntry; 48]) {
        for &p in &self.filled {
            self.pages[p as usize] = 0;
        }
        self.filled.clear();
        self.asid = regs[cop0::ENTRY_HI] & 0xFF;
        self.asid_dependent = false;
        // The first matching entry wins: later ones only fill free pages.
        for e in tlb {
            let global = e.entry_lo0 & e.entry_lo1 & LO_GLOBAL != 0;
            if !global {
                self.asid_dependent = true;
                if e.entry_hi & 0xFF != self.asid {
                    continue;
                }
            }
            // Each entry maps an even and an odd page of 4 KiB to 16 MiB.
            let scratchpad = e.entry_lo0 & LO_SCRATCHPAD != 0;
            // A scratchpad entry maps all 16 KiB of it, whatever its page
            // size (the BIOS sets 4 KiB, and keeps its stack at the top).
            let page = if scratchpad {
                8192
            } else {
                u64::from(((e.page_mask >> 13) & 0xFFF) + 1) * 4096
            };
            let base = u64::from(e.entry_hi) & !(2 * page - 1);
            for (half, lo) in [e.entry_lo0, e.entry_lo1].into_iter().enumerate() {
                let flags =
                    MAPPED | if lo & LO_VALID != 0 { VALID } else { 0 } | if lo & LO_DIRTY != 0 { DIRTY } else { 0 };
                let start = base + half as u64 * page;
                for k in (0..page).step_by(4096) {
                    let va = start + k;
                    if va >= 1 << 32 {
                        break;
                    }
                    let index = (va >> 12) as u32;
                    if self.pages[index as usize] != 0 {
                        continue;
                    }
                    let pa = if scratchpad {
                        SCRATCHPAD + ((half as u64 * page + k) & 0x3FFF) as u32
                    } else {
                        (((lo >> 6) & 0xF_FFFF) << 12).wrapping_add(k as u32)
                    };
                    self.pages[index as usize] = (pa & !0xFFF) | flags;
                    self.filled.push(index);
                }
            }
        }
        self.stale = false;
    }
}

impl Cop0 {
    /// Changes whenever a virtual address may translate differently (the
    /// TLB, the ASID, Status.ERL).
    #[inline]
    pub fn mapping(&self) -> u32 {
        (self.pages.generation << 1) | u32::from(self.regs[cop0::STATUS] & cop0::STATUS_ERL != 0)
    }

    /// A virtual address's physical address.
    #[inline]
    pub fn translate(&mut self, va: u32, write: bool) -> Result<u32, Fault> {
        // kseg0 and kseg1: unmapped.
        if va & 0xC000_0000 == 0x8000_0000 {
            return Ok(va & 0x1FFF_FFFF);
        }
        // With ERL set (at reset), kuseg is unmapped too.
        if va < 0x8000_0000 && self.regs[cop0::STATUS] & cop0::STATUS_ERL != 0 {
            return Ok(va & 0x1FFF_FFFF);
        }
        if self.pages.stale {
            self.pages.rebuild(&self.regs, &self.tlb);
        }
        let e = self.pages.pages[(va >> 12) as usize];
        if e == 0 {
            Err(Fault::Refill)
        } else if e & VALID == 0 {
            Err(Fault::Invalid)
        } else if write && e & DIRTY == 0 {
            Err(Fault::Modified)
        } else {
            Ok((e & !0xFFF) | (va & 0xFFF))
        }
    }

    /// Records a translation fault: BadVAddr, Context and EntryHi.
    pub fn record_fault(&mut self, va: u32) {
        self.regs[cop0::BAD_VADDR] = va;
        let context = self.regs[cop0::CONTEXT];
        self.regs[cop0::CONTEXT] = (context & 0xFF80_0000) | ((va >> 9) & 0x007F_FFF0);
        let hi = self.regs[cop0::ENTRY_HI];
        self.regs[cop0::ENTRY_HI] = (va & 0xFFFF_E000) | (hi & 0xFF);
        self.pages.asid_changed(hi & 0xFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cop0_with(entries: &[TlbEntry]) -> Cop0 {
        let mut c = Cop0::new();
        c.regs[cop0::STATUS] = 0;
        c.tlb[..entries.len()].copy_from_slice(entries);
        // Other entries: invalid, at a high address nothing uses here.
        for e in &mut c.tlb[entries.len()..] {
            e.entry_hi = 0xE000_0000;
        }
        c.pages.invalidate();
        c
    }

    #[test]
    fn pages_faults_and_scratchpad() {
        let mut c = cop0_with(&[
            // 0x00000000: two 16 MiB pages, onto physical 0 and 16 MiB,
            // global; the second one read-only.
            TlbEntry {
                page_mask: 0x01FF_E000,
                entry_hi: 0,
                entry_lo0: LO_DIRTY | LO_VALID | LO_GLOBAL,
                entry_lo1: (0x1000 << 6) | LO_VALID | LO_GLOBAL,
            },
            // 0x70000000: the scratchpad, 16 KiB, though with 4 KiB pages,
            // as the BIOS sets it.
            TlbEntry {
                page_mask: 0,
                entry_hi: 0x7000_0000,
                entry_lo0: LO_SCRATCHPAD | LO_DIRTY | LO_VALID | LO_GLOBAL,
                entry_lo1: LO_SCRATCHPAD | LO_DIRTY | LO_VALID | LO_GLOBAL,
            },
        ]);
        assert_eq!(c.translate(0x0012_3456, true), Ok(0x0012_3456));
        assert_eq!(c.translate(0x0123_4567, false), Ok(0x0123_4567));
        assert_eq!(c.translate(0x0123_4567, true), Err(Fault::Modified));
        assert_eq!(c.translate(0x7000_1234, true), Ok(SCRATCHPAD + 0x1234));
        assert_eq!(c.translate(0x7000_3FF0, true), Ok(SCRATCHPAD + 0x3FF0));
        assert_eq!(c.translate(0x4000_0000, false), Err(Fault::Refill));
        assert_eq!(c.translate(0x8000_1000, false), Ok(0x1000));
        assert_eq!(c.translate(0xBFC0_0000, false), Ok(0x1FC0_0000));
        c.record_fault(0x4000_3000);
        assert_eq!(c.regs[cop0::BAD_VADDR], 0x4000_3000);
        assert_eq!(c.regs[cop0::ENTRY_HI] & !0xFF, 0x4000_2000);
    }

    #[test]
    fn asids_and_rewrites() {
        let mut c = cop0_with(&[TlbEntry {
            page_mask: 0,
            entry_hi: 0x0040_0000 | 5,
            entry_lo0: (0x100 << 6) | LO_VALID,
            entry_lo1: LO_VALID,
        }]);
        assert_eq!(c.translate(0x0040_0010, false), Err(Fault::Refill));
        c.write(cop0::ENTRY_HI as u32, 0, 5);
        assert_eq!(c.translate(0x0040_0010, false), Ok(0x0010_0010));
        // Rewritten through TLBWI: the old mapping goes.
        c.regs[cop0::INDEX] = 0;
        c.regs[cop0::ENTRY_HI] = 0x0080_0000 | 5;
        c.regs[cop0::ENTRY_LO0] = (0x200 << 6) | LO_VALID | LO_GLOBAL;
        c.regs[cop0::ENTRY_LO1] = LO_VALID | LO_GLOBAL;
        c.tlbwi();
        assert_eq!(c.translate(0x0040_0010, false), Err(Fault::Refill));
        assert_eq!(c.translate(0x0080_0010, false), Ok(0x0020_0010));
    }
}
