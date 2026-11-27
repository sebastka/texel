//! The SIF (subsystem interface): how the EE and the IOP talk. Mailbox and
//! flag registers that both see (at 0x1000F200 on the EE, 0x1D000000 on
//! the IOP), and two FIFOs fed and drained by DMA: SIF0 from the IOP to
//! the EE, SIF1 from the EE to the IOP.

use std::collections::VecDeque;

/// FIFO capacity, in words: the transfers stop when it is full and go on
/// when the other side has drained it.
pub const FIFO_WORDS: usize = 32;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Sif {
    /// Main (EE) to sub (IOP) command and flags, and the reverse.
    pub mscom: u32,
    pub smcom: u32,
    pub msflg: u32,
    pub smflg: u32,
    pub ctrl: u32,
    pub bd6: u32,
    /// How often the EE read SMFLG: polling it means waiting for the IOP.
    pub smflg_reads: u64,
    /// SIF0: IOP → EE.
    pub sif0: VecDeque<u32>,
    /// SIF1: EE → IOP.
    pub sif1: VecDeque<u32>,
    /// Words moved so far: SIF0 (IOP → EE) and SIF1, for diagnostics.
    pub sif0_words: u64,
    pub sif1_words: u64,
    /// The register writes, for diagnostics, the first 64: (offset, value),
    /// the EE's with 0x1000 added to the offset.
    #[serde(skip)]
    pub iop_writes: Vec<(u32, u32)>,
    /// Transfer tags, for diagnostics: (who, tag word 0, word 1).
    #[serde(skip)]
    pub tags: Vec<(&'static str, u32, u32)>,
    /// Command packets (blocks of up to 16 words), for diagnostics.
    #[serde(skip)]
    pub packets: Vec<(&'static str, Vec<u32>)>,
}

impl Sif {
    /// Keeps the last 40 packets.
    pub fn log_packet(&mut self, dir: &'static str, p: Vec<u32>) {
        if self.packets.len() == 40 {
            self.packets.remove(0);
        }
        self.packets.push((dir, p));
    }

    pub fn log_tag(&mut self, who: &'static str, w0: u32, w1: u32) {
        if self.tags.len() < 200 {
            self.tags.push((who, w0, w1));
        }
    }
}

impl Sif {
    /// `offset`: from 0x1000F200.
    pub fn ee_read(&mut self, offset: u32) -> u32 {
        match offset {
            0x00 => self.mscom,
            0x10 => self.smcom,
            0x20 => self.msflg,
            0x30 => {
                self.smflg_reads += 1;
                self.smflg
            }
            0x40 => self.ctrl | 0xF000_0102,
            0x60 => self.bd6,
            _ => 0,
        }
    }

    pub fn ee_write(&mut self, offset: u32, v: u32) {
        // In the same log as the IOP's, marked (0x1000 + offset).
        if self.iop_writes.len() < 64 {
            self.iop_writes.push((0x1000 | offset, v));
        }
        match offset {
            0x00 => self.mscom = v,
            // The EE sets its own flags, and clears the IOP's.
            0x20 => self.msflg |= v,
            0x30 => self.smflg &= !v,
            0x40 => self.ctrl = (self.ctrl & !0x100) | (v & 0x100),
            0x60 => self.bd6 = v,
            _ => {}
        }
    }

    /// `offset`: from 0x1D000000.
    pub fn iop_read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => self.mscom,
            0x10 => self.smcom,
            0x20 => self.msflg,
            0x30 => self.smflg,
            0x40 => self.ctrl | 0xF000_0002,
            0x60 => self.bd6,
            _ => 0,
        }
    }

    pub fn iop_write(&mut self, offset: u32, v: u32) {
        if self.iop_writes.len() < 64 {
            self.iop_writes.push((offset, v));
        }
        match offset {
            0x10 => self.smcom = v,
            // The IOP sets its own flags, and clears the EE's.
            0x20 => self.msflg &= !v,
            0x30 => self.smflg |= v,
            0x40 => {
                if v & 0xA0 != 0 {
                    self.ctrl = (self.ctrl & !0xF000) | 0x2000;
                }
                if self.ctrl & v != 0 {
                    self.ctrl &= !v;
                } else {
                    self.ctrl |= v;
                }
            }
            0x60 => self.bd6 = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_set_by_one_side_and_cleared_by_the_other() {
        let mut sif = Sif::default();
        sif.iop_write(0x30, 0x1_0000);
        assert_eq!(sif.ee_read(0x30), 0x1_0000);
        sif.ee_write(0x30, 0x1_0000);
        assert_eq!(sif.iop_read(0x30), 0);
        sif.ee_write(0x20, 0x2_0000);
        sif.ee_write(0x20, 0x1);
        assert_eq!(sif.iop_read(0x20), 0x2_0001);
        sif.iop_write(0x20, 0x2_0000);
        assert_eq!(sif.ee_read(0x20), 0x1);
    }
}
