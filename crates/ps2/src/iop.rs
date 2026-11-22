//! The IOP's side of the machine: its memory map (2 MiB of RAM, the BIOS,
//! a 1 KiB scratchpad), its interrupt controller, six timers and DMA
//! controller, and the SIF. Its CPU is the PlayStation's, from `texel-ps1`,
//! with the IOP's processor ID.
//!
//! Peripherals the BIOS only configures are kept as registers; the ones it
//! waits on answer as idle.

use std::collections::BTreeMap;

use texel_ps1::cpu::CpuBus;

use crate::cdvd::Cdvd;
use crate::sif::{FIFO_WORDS, Sif};
use crate::sio2::Sio2;
use crate::spu2::Spu2;

/// The IOP's clock, in Hz.
const IOP_CLOCK: u64 = 36_864_000;

pub const IOP_RAM_SIZE: usize = 2 * 1024 * 1024;
/// The IOP runs at an eighth of the EE's clock: 36.864 MHz.
pub const IOP_CLOCK_DIVIDER: u64 = 8;
/// COP0 PRId of the IOP: the BIOS tells the two CPUs apart by it.
pub const IOP_PRID: u32 = 0x1F;

/// IOP interrupt sources, by bit of I_STAT.
pub mod irq {
    pub const VBLANK_START: u32 = 0;
    pub const CDVD: u32 = 2;
    pub const DMA: u32 = 3;
    pub const SPU2: u32 = 9;
    pub const TIMER: [u32; 6] = [4, 5, 6, 14, 15, 16];
    pub const VBLANK_END: u32 = 11;
    pub const SIO2: u32 = 17;
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Intc {
    pub stat: u32,
    pub mask: u32,
    /// I_CTRL: the global enable; reading it clears it.
    pub ctrl: u32,
    /// Interrupts raised, by source, for diagnostics.
    pub counts: [u64; 32],
}

impl Intc {
    pub fn raise(&mut self, bit: u32) {
        self.stat |= 1 << bit;
        self.counts[bit as usize] += 1;
    }

    fn line(&self) -> bool {
        self.ctrl & 1 != 0 && self.stat & self.mask != 0
    }
}

/// One of the six timers: 16-bit (0-2) or 32-bit (3-5) counters on the IOP
/// clock, or on horizontal blanks.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Timer {
    pub count: u64,
    pub mode: u32,
    pub target: u64,
    remainder: u64,
    /// The interrupt fired, and does not repeat.
    fired: bool,
}

const MODE_ZERO_RETURN: u32 = 1 << 3;
const MODE_IRQ_TARGET: u32 = 1 << 4;
const MODE_IRQ_OVERFLOW: u32 = 1 << 5;
const MODE_REPEAT: u32 = 1 << 6;
const MODE_IRQ_READY: u32 = 1 << 10;
const MODE_TARGET_REACHED: u32 = 1 << 11;
const MODE_OVERFLOW_REACHED: u32 = 1 << 12;

impl Timer {
    fn width(index: usize) -> u64 {
        if index < 3 { 1 << 16 } else { 1 << 32 }
    }

    /// IOP cycles per count; `None` for horizontal blanks.
    fn divider(&self, index: usize) -> Option<u64> {
        let hblank = self.mode & 0x100 != 0;
        match index {
            1 | 3 if hblank => None,
            2 if self.mode & 0x200 != 0 => Some(8),
            4 | 5 => Some(match (self.mode >> 13) & 3 {
                0 => 1,
                1 => 8,
                2 => 16,
                _ => 256,
            }),
            _ => Some(1),
        }
    }

    /// Counts `n`; returns whether to interrupt.
    fn count_ticks(&mut self, index: usize, n: u64) -> bool {
        if n == 0 {
            return false;
        }
        let width = Self::width(index);
        let before = self.count;
        let mut after = before + n;
        let mut irq = false;
        if self.target > before && self.target <= after {
            self.mode |= MODE_TARGET_REACHED;
            irq |= self.mode & MODE_IRQ_TARGET != 0;
            if self.mode & MODE_ZERO_RETURN != 0 && self.target > 0 {
                after = (after - self.target) % self.target;
            }
        }
        if after >= width {
            self.mode |= MODE_OVERFLOW_REACHED;
            irq |= self.mode & MODE_IRQ_OVERFLOW != 0;
            after %= width;
        }
        self.count = after;
        if irq && (!self.fired || self.mode & MODE_REPEAT != 0) {
            self.fired = true;
            self.mode &= !MODE_IRQ_READY;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Timers {
    pub t: [Timer; 6],
}

impl Timers {
    fn advance(&mut self, cycles: u64, hblanks: u64, intc: &mut Intc) {
        for (i, t) in self.t.iter_mut().enumerate() {
            let ticks = match t.divider(i) {
                Some(d) => {
                    t.remainder += cycles;
                    let n = t.remainder / d;
                    t.remainder %= d;
                    n
                }
                None => hblanks,
            };
            if t.count_ticks(i, ticks) {
                intc.raise(irq::TIMER[i]);
            }
        }
    }

    /// The timer at an address, and the register (0: count, 4: mode, 8: target).
    fn select(pa: u32) -> Option<(usize, u32)> {
        match pa {
            0x1F80_1100..0x1F80_1130 => Some((((pa - 0x1F80_1100) >> 4) as usize, pa & 0xC)),
            0x1F80_1480..0x1F80_14B0 => Some((3 + ((pa - 0x1F80_1480) >> 4) as usize, pa & 0xC)),
            _ => None,
        }
    }

    fn read(&mut self, index: usize, reg: u32) -> u32 {
        let t = &mut self.t[index];
        match reg {
            0 => t.count as u32,
            4 => {
                let v = t.mode;
                // Reading clears the reached flags.
                t.mode &= !(MODE_TARGET_REACHED | MODE_OVERFLOW_REACHED);
                v
            }
            8 => t.target as u32,
            _ => 0,
        }
    }

    fn write(&mut self, index: usize, reg: u32, v: u32) {
        let t = &mut self.t[index];
        match reg {
            0 => t.count = u64::from(v) % Timer::width(index),
            4 => {
                t.mode = (v & 0x63FF) | MODE_IRQ_READY;
                t.count = 0;
                t.remainder = 0;
                t.fired = false;
            }
            8 => {
                t.target = u64::from(v) % Timer::width(index);
                if t.mode & MODE_REPEAT == 0 {
                    t.fired = false;
                }
            }
            _ => {}
        }
    }
}

/// The DMA controller's registers. Channels 9 (SIF0) and 10 (SIF1) move
/// data through the SIF (see `Bus::pump_sif`); the others complete at once.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Dma {
    /// MADR, BCR, CHCR, TADR of channels 0-13.
    pub ch: [[u32; 4]; 14],
    pub dpcr: u32,
    pub dicr: u32,
    pub dpcr2: u32,
    pub dicr2: u32,
    pub dmacen: u32,
    pub dmacinten: u32,
    /// SIF0 and SIF1: words left in the current block, and whether it is
    /// the last one.
    pub sif0_words: u32,
    pub sif0_end: bool,
    pub sif1_words: u32,
    pub sif1_end: bool,
    /// Transfers under way: channel, IOP cycles left.
    pub pending: Vec<(usize, u64)>,
    /// CDVD (channel 3): bytes left to move.
    pub cdvd_bytes: u64,
    /// SIO2's replies (channel 12) wait for its transfer to run.
    pub sio2_out_waiting: bool,
    /// Auto-DMA under way to the SPU2's cores (channels 4 and 7).
    pub adma: [bool; 2],
    /// The last CDVD transfers, for diagnostics: (MADR, bytes, BCR at the
    /// start).
    #[serde(skip)]
    pub cdvd_log: std::collections::VecDeque<(u32, usize, u32)>,
}

/// IOP cycles per word of SPU2 DMA (PCSX2 has 24 per halfword).
const SPU2_DMA_CYCLES_PER_WORD: u64 = 48;

/// CHCR: the channel is busy.
pub const CHCR_BUSY: u32 = 1 << 24;

impl Dma {
    fn select(pa: u32) -> Option<(usize, usize)> {
        match pa {
            0x1F80_1080..0x1F80_10F0 => Some((((pa - 0x1F80_1080) >> 4) as usize, ((pa >> 2) & 3) as usize)),
            0x1F80_1500..0x1F80_1570 => Some((7 + ((pa - 0x1F80_1500) >> 4) as usize, ((pa >> 2) & 3) as usize)),
            _ => None,
        }
    }

    /// A channel finished: its flag, and the DMA interrupt if enabled.
    pub fn complete(&mut self, ch: usize, intc: &mut Intc) {
        self.ch[ch][2] &= !CHCR_BUSY;
        let (dicr, bit) = if ch < 7 {
            (&mut self.dicr, ch)
        } else {
            (&mut self.dicr2, ch - 7)
        };
        if *dicr & (1 << (16 + bit)) != 0 {
            *dicr |= 1 << (24 + bit);
            // The master enable is in DICR, for both registers.
            if self.dicr & (1 << 23) != 0 {
                intc.raise(irq::DMA);
            }
        }
    }

    fn write_dicr(old: u32, v: u32) -> u32 {
        // Flags (24-30) are cleared by writing 1; bit 31 is read-only.
        let flags = old & 0x7F00_0000 & !(v & 0x7F00_0000);
        (v & 0x00FF_FFFF) | flags
    }

    fn read_dicr(&self, second: bool) -> u32 {
        let dicr = if second { self.dicr2 } else { self.dicr };
        let pending = (self.dicr | self.dicr2) & 0x7F00_0000 != 0;
        let master = self.dicr & (1 << 15) != 0 || (self.dicr & (1 << 23) != 0 && pending);
        dicr | if master { 1 << 31 } else { 0 }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IopBus {
    pub ram: Box<[u8]>,
    #[serde(skip)]
    bios: Box<[u8]>,
    scratchpad: Box<[u8]>,
    pub intc: Intc,
    pub timers: Timers,
    pub dma: Dma,
    pub sif: Sif,
    pub cdvd: Cdvd,
    pub sio2: Sio2,
    pub spu2: Spu2,
    /// IOP cycles into the current second, for the CDVD's clock.
    second_cycles: u64,
    cache_ctrl: u32,
    /// Other registers: kept as written.
    regs: BTreeMap<u32, u32>,
    /// The IOP's PC when this instruction started, for diagnostics.
    pub pc: u32,
    /// What the IOP writes to its debug port (its kernel's printf).
    #[serde(skip)]
    pub out: Vec<u8>,
    /// Accesses to addresses not emulated: address → count.
    #[serde(skip)]
    pub unknown: BTreeMap<u32, u64>,
}

impl IopBus {
    /// See `Bus::transplant`.
    pub(crate) fn transplant(&mut self, old: &mut IopBus) {
        self.bios = std::mem::take(&mut old.bios);
        self.cdvd.disc = old.cdvd.disc.take();
        self.sio2.cards = std::mem::take(&mut old.sio2.cards);
    }

    pub fn new(bios: &[u8]) -> Self {
        let mut rom = vec![0u8; crate::bios::BIOS_SIZE].into_boxed_slice();
        let n = bios.len().min(rom.len());
        rom[..n].copy_from_slice(&bios[..n]);
        Self {
            ram: vec![0; IOP_RAM_SIZE].into_boxed_slice(),
            bios: rom,
            scratchpad: vec![0; 1024].into_boxed_slice(),
            intc: Intc::default(),
            timers: Timers::default(),
            dma: Dma::default(),
            sif: Sif::default(),
            cdvd: Cdvd::default(),
            sio2: Sio2::default(),
            spu2: Spu2::default(),
            second_cycles: 0,
            cache_ctrl: 0,
            regs: BTreeMap::new(),
            pc: 0,
            out: Vec::new(),
            unknown: BTreeMap::new(),
        }
    }

    /// Advances the timers by `cycles` IOP cycles and `hblanks` lines.
    pub fn advance(&mut self, cycles: u64, hblanks: u64) {
        self.timers.advance(cycles, hblanks, &mut self.intc);
        self.pump_sio2();
        if std::mem::take(&mut self.sio2.irq) {
            self.intc.raise(irq::SIO2);
        }
        if self.spu2.tick(cycles) {
            self.intc.raise(irq::SPU2);
        }
        for core in 0..2 {
            if self.dma.adma[core] && self.spu2.adma_wants(core) {
                self.dma.adma[core] = false;
                self.spu2.dma_done(core);
                self.dma.complete(if core == 0 { 4 } else { 7 }, &mut self.intc);
            }
        }
        let mut k = 0;
        while k < self.dma.pending.len() {
            let (ch, left) = self.dma.pending[k];
            if left > cycles {
                self.dma.pending[k].1 = left - cycles;
                k += 1;
                continue;
            }
            self.dma.pending.swap_remove(k);
            match ch {
                4 => self.spu2.dma_done(0),
                7 => self.spu2.dma_done(1),
                _ => {}
            }
            self.dma.complete(ch, &mut self.intc);
        }
        if self.cdvd.advance(cycles) {
            self.intc.raise(irq::CDVD);
        }
        self.pump_cdvd();
        self.second_cycles += cycles;
        if self.second_cycles >= IOP_CLOCK {
            self.second_cycles -= IOP_CLOCK;
            self.cdvd.rtc_seconds += 1;
        }
    }

    /// The registers kept as written, for diagnostics.
    pub fn registers(&self) -> &BTreeMap<u32, u32> {
        &self.regs
    }

    fn note_unknown(&mut self, pa: u32) {
        *self.unknown.entry(pa).or_insert(0) += 1;
    }

    /// Memory holding `pa`, and the offset in it.
    fn memory(&mut self, pa: u32) -> Option<(&mut [u8], usize)> {
        match pa {
            // 2 MiB, mirrored over the first 8.
            0x0000_0000..0x0080_0000 => Some((&mut self.ram, (pa as usize) & (IOP_RAM_SIZE - 1))),
            0x1F80_0000..0x1F80_0400 => Some((&mut self.scratchpad, (pa & 0x3FF) as usize)),
            0x1FC0_0000..0x2000_0000 => Some((&mut self.bios, (pa - 0x1FC0_0000) as usize)),
            _ => None,
        }
    }

    fn read<const N: usize>(&mut self, addr: u32) -> [u8; N] {
        let pa = physical(addr) & !(N as u32 - 1);
        // The CDVD's registers are bytes, the SPU2's halfwords.
        if (0x1F40_2000..0x1F40_2040).contains(&pa) {
            return std::array::from_fn(|i| self.cdvd.read(pa - 0x1F40_2000 + i as u32));
        }
        if (0x1F90_0000..0x1F90_0800).contains(&pa) {
            let off = pa - 0x1F90_0000;
            let lo = self.spu2.read16(off & !1);
            let hi = if N == 4 { self.spu2.read16((off & !1) + 2) } else { 0 };
            let v = (u32::from(hi) << 16 | u32::from(lo)) >> ((off & 1) * 8);
            let bytes = v.to_le_bytes();
            return std::array::from_fn(|i| bytes[i]);
        }
        if let Some((mem, at)) = self.memory(pa) {
            let mut out = [0u8; N];
            if let Some(src) = mem.get(at..at + N) {
                out.copy_from_slice(src);
            }
            return out;
        }
        let shift = (pa & 3) * 8;
        let v = self.io_read(pa & !3) >> shift;
        let bytes = v.to_le_bytes();
        let mut out = [0u8; N];
        out.copy_from_slice(&bytes[..N]);
        out
    }

    fn write<const N: usize>(&mut self, addr: u32, data: [u8; N]) {
        let pa = physical(addr) & !(N as u32 - 1);
        if (0x1FC0_0000..0x2000_0000).contains(&pa) {
            return;
        }
        if (0x1F90_0000..0x1F90_0800).contains(&pa) {
            let off = pa - 0x1F90_0000;
            let mut bytes = [0u8; 4];
            bytes[..N].copy_from_slice(&data);
            let v = u32::from_le_bytes(bytes);
            match N {
                1 => {}
                2 => self.spu2.write16(off, v as u16),
                _ => {
                    self.spu2.write16(off, v as u16);
                    self.spu2.write16(off + 2, (v >> 16) as u16);
                }
            }
            return;
        }
        if (0x1F40_2000..0x1F40_2040).contains(&pa) {
            if pa == 0x1F40_2016 {
                *self.cdvd.s_callers.entry(self.pc).or_insert(0) += 1;
            }
            for (i, b) in data.into_iter().enumerate() {
                if self.cdvd.write(pa - 0x1F40_2000 + i as u32, b) {
                    self.intc.raise(irq::CDVD);
                }
            }
            return;
        }
        if let Some((mem, at)) = self.memory(pa) {
            if let Some(dst) = mem.get_mut(at..at + N) {
                dst.copy_from_slice(&data);
            }
            return;
        }
        let mut bytes = [0u8; 4];
        bytes[..N].copy_from_slice(&data);
        let v = u32::from_le_bytes(bytes);
        // The debug port takes bytes.
        if pa == 0x1F80_380C {
            self.out.push(v as u8);
            return;
        }
        self.io_write(pa & !3, v << ((pa & 3) * 8), N);
    }

    fn io_read(&mut self, pa: u32) -> u32 {
        if let Some((t, reg)) = Timers::select(pa) {
            return self.timers.read(t, reg);
        }
        if let Some((ch, reg)) = Dma::select(pa) {
            return self.dma.ch[ch][reg];
        }
        match pa {
            0x1D00_0000..0x1D00_0070 => self.sif.iop_read(pa - 0x1D00_0000),
            0x1F80_8200..0x1F80_8300 => self.sio2.read(pa - 0x1F80_8200),
            0x1F80_1070 => self.intc.stat,
            0x1F80_1074 => self.intc.mask,
            0x1F80_1078 => std::mem::take(&mut self.intc.ctrl),
            0x1F80_10F0 => self.dma.dpcr,
            0x1F80_10F4 => self.dma.read_dicr(false),
            0x1F80_1570 => self.dma.dpcr2,
            0x1F80_1574 => self.dma.read_dicr(true),
            0x1F80_1578 => self.dma.dmacen,
            0x1F80_157C => self.dma.dmacinten,
            0xFFFE_0130 => self.cache_ctrl,
            // ROM1 (the DVD player's ROM, on later models) and ROM2: not
            // dumped, so absent.
            0x1E00_0000..0x1E40_0000 => 0,
            _ => match self.regs.get(&pa) {
                Some(&v) => v,
                None => {
                    self.note_unknown(pa);
                    0
                }
            },
        }
    }

    fn io_write(&mut self, pa: u32, v: u32, width: usize) {
        if let Some((t, reg)) = Timers::select(pa) {
            return self.timers.write(t, reg, v);
        }
        if let Some((ch, reg)) = Dma::select(pa) {
            self.dma.ch[ch][reg] = v;
            if reg == 2 && v & CHCR_BUSY != 0 && (ch == 9 || ch == 10) {
                self.sif.log_tag(
                    if ch == 9 { "IOP start ch9" } else { "IOP start ch10" },
                    v,
                    self.dma.ch[ch][3],
                );
            }
            if reg == 2 && v & CHCR_BUSY != 0 && ch != 9 && ch != 10 {
                // No data moves yet. Channels 4 and 7 (the SPU2's two
                // cores) take time, as the sound libraries expect to see
                // them under way; the others are done at once.
                self.note_unknown(pa);
                if ch == 12 && !self.sio2.has_replies() {
                    // SIO2's replies: once its transfer has run.
                    self.dma.sio2_out_waiting = true;
                } else if ch == 11 || ch == 12 {
                    self.sio2_dma(ch);
                } else if ch == 3 {
                    // CDVD: blocks as the drive reads them.
                    let bcr = self.dma.ch[3][1];
                    self.dma.cdvd_bytes = 4 * u64::from((bcr & 0xFFFF).max(1)) * u64::from((bcr >> 16).max(1));
                    self.pump_cdvd();
                } else if ch == 4 || ch == 7 {
                    self.spu2_dma(ch);
                } else {
                    self.dma.complete(ch, &mut self.intc);
                }
            }
            return;
        }
        match pa {
            0x1D00_0000..0x1D00_0070 => self.sif.iop_write(pa - 0x1D00_0000, v),
            0x1F80_8200..0x1F80_8300 => {
                self.sio2.write(pa - 0x1F80_8200, v);
                self.pump_sio2();
            }
            // Written 0 to acknowledge.
            0x1F80_1070 => self.intc.stat &= v,
            0x1F80_1074 => self.intc.mask = v,
            0x1F80_1078 => self.intc.ctrl = v & 1,
            0x1F80_10F0 => self.dma.dpcr = v,
            0x1F80_10F4 => self.dma.dicr = Dma::write_dicr(self.dma.dicr, v),
            0x1F80_1570 => self.dma.dpcr2 = v,
            0x1F80_1574 => self.dma.dicr2 = Dma::write_dicr(self.dma.dicr2, v),
            0x1F80_1578 => self.dma.dmacen = v,
            0x1F80_157C => self.dma.dmacinten = v,
            0xFFFE_0130 => self.cache_ctrl = v,
            _ => {
                if !self.regs.contains_key(&pa) && width == 4 {
                    self.note_unknown(pa);
                }
                self.regs.insert(pa, v);
            }
        }
    }

    /// SIO2's DMA: channel 11 brings the bytes to send, channel 12 takes
    /// the replies (once the transfer has run); a block per command.
    fn sio2_dma(&mut self, ch: usize) {
        let bcr = self.dma.ch[ch][1];
        let size = 4 * (bcr & 0xFFFF).max(1);
        let blocks = (bcr >> 16).max(1);
        let madr = self.dma.ch[ch][0];
        for b in 0..blocks {
            let start = madr.wrapping_add(b * size);
            let at = |k: u32| (start.wrapping_add(k) as usize) & (IOP_RAM_SIZE - 1);
            if ch == 11 {
                let block: Vec<u8> = (0..size).map(|k| self.ram[at(k)]).collect();
                self.sio2.push_block(&block);
            } else {
                let block = self.sio2.pop_block(size as usize);
                for (k, v) in block.into_iter().enumerate() {
                    self.ram[at(k as u32)] = v;
                }
            }
        }
        self.dma.ch[ch][0] = madr.wrapping_add(blocks * size);
        self.dma.complete(ch, &mut self.intc);
    }

    /// The SPU2's DMA (channel 4: core 0, 7: core 1): into sound memory (it
    /// takes time), out of it, or to the core's sound data input
    /// (auto-DMA: done once the core is ready for more).
    fn spu2_dma(&mut self, ch: usize) {
        let core = usize::from(ch == 7);
        self.spu2.dma_started(core);
        let bcr = self.dma.ch[ch][1];
        let words = u64::from((bcr >> 16).max(1)) * u64::from((bcr & 0xFFFF).max(1));
        let madr = self.dma.ch[ch][0] as usize;
        let len = 4 * words as usize;
        let at = |k: usize| (madr + k) & (IOP_RAM_SIZE - 1);
        if self.spu2.dma_reads(core) {
            let data = self.spu2.dma_read(core, len);
            for (k, b) in data.into_iter().enumerate() {
                self.ram[at(k)] = b;
            }
        } else {
            let data: Vec<u8> = (0..len).map(|k| self.ram[at(k)]).collect();
            if self.spu2.auto_dma(core) {
                self.spu2.adma_write(core, &data);
                self.dma.adma[core] = true;
                return;
            }
            self.spu2.dma_write(core, &data);
        }
        self.dma.pending.push((ch, (words * SPU2_DMA_CYCLES_PER_WORD).max(64)));
    }

    /// Channel 12, waiting for SIO2's transfer: done once it has run.
    fn pump_sio2(&mut self) {
        if self.dma.sio2_out_waiting && self.sio2.has_replies() {
            self.dma.sio2_out_waiting = false;
            self.sio2_dma(12);
        }
    }

    /// Moves the blocks the CDVD has read into memory, through channel 3
    /// when it is under way; completes it once it has moved all it was
    /// set for.
    fn pump_cdvd(&mut self) {
        while self.dma.ch[3][2] & CHCR_BUSY != 0 && self.dma.cdvd_bytes > 0 {
            let Some(block) = self.cdvd.blocks.pop_front() else {
                return;
            };
            let n = (block.len() as u64).min(self.dma.cdvd_bytes) as usize;
            let madr = self.dma.ch[3][0];
            for (k, &b) in block[..n].iter().enumerate() {
                self.ram[(madr as usize + k) & (IOP_RAM_SIZE - 1)] = b;
            }
            self.dma.ch[3][0] = madr.wrapping_add(n as u32);
            self.dma.cdvd_bytes -= n as u64;
            if self.dma.cdvd_log.len() == 64 {
                self.dma.cdvd_log.pop_front();
            }
            self.dma.cdvd_log.push_back((madr, block.len(), self.dma.ch[3][1]));
            if self.dma.cdvd_bytes == 0 {
                self.dma.complete(3, &mut self.intc);
            }
        }
    }

    // --- SIF transfers, IOP side ----------------------------------------------

    fn ram_word(&self, addr: u32) -> u32 {
        let at = (addr as usize) & (IOP_RAM_SIZE - 1) & !3;
        u32::from_le_bytes(self.ram[at..at + 4].try_into().unwrap_or_default())
    }

    fn set_ram_word(&mut self, addr: u32, v: u32) {
        let at = (addr as usize) & (IOP_RAM_SIZE - 1) & !3;
        self.ram[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// SIF0 (channel 9): IOP memory into the FIFO, by chain of tags in IOP
    /// memory. Each tag is 16 bytes: the IOP tag (address and flags, word
    /// count), then the EE's DMA tag, which goes into the FIFO first.
    /// Returns whether anything moved.
    pub fn pump_sif0(&mut self) -> bool {
        let mut moved = false;
        while self.dma.ch[9][2] & CHCR_BUSY != 0 && self.sif.sif0.len() < FIFO_WORDS {
            if self.dma.sif0_words == 0 {
                if self.dma.sif0_end {
                    self.dma.sif0_end = false;
                    self.dma.complete(9, &mut self.intc);
                    return true;
                }
                if self.sif.sif0.len() + 4 > FIFO_WORDS {
                    break;
                }
                let tadr = self.dma.ch[9][3];
                let tag = self.ram_word(tadr);
                let words = self.ram_word(tadr + 4);
                self.sif.log_tag("IOP sif0", tag, words);
                for k in 0..4 {
                    let w = self.ram_word(tadr + 8 + 4 * k);
                    self.sif.sif0.push_back(w);
                }
                if words <= 16 {
                    let p: Vec<u32> = (0..words).map(|k| self.ram_word((tag & 0xFF_FFFF) + 4 * k)).collect();
                    self.sif.log_packet("IOP->EE", p);
                }
                self.dma.ch[9][3] = tadr.wrapping_add(16);
                self.dma.ch[9][0] = tag & 0x00FF_FFFF;
                self.dma.sif0_words = (words + 3) & !3;
                self.dma.sif0_end = tag & 0xC000_0000 != 0;
                moved = true;
                continue;
            }
            let madr = self.dma.ch[9][0];
            let w = self.ram_word(madr);
            self.sif.sif0.push_back(w);
            self.sif.sif0_words += 1;
            self.dma.ch[9][0] = madr.wrapping_add(4);
            self.dma.sif0_words -= 1;
            moved = true;
        }
        moved
    }

    /// SIF1 (channel 10): the FIFO into IOP memory. Each block starts with
    /// a 4-word IOP tag: address and flags, word count.
    pub fn pump_sif1(&mut self) -> bool {
        let mut moved = false;
        while self.dma.ch[10][2] & CHCR_BUSY != 0 {
            if self.dma.sif1_words == 0 {
                if self.dma.sif1_end {
                    self.dma.sif1_end = false;
                    self.dma.complete(10, &mut self.intc);
                    return true;
                }
                if self.sif.sif1.len() < 4 {
                    break;
                }
                let tag = self.sif.sif1.pop_front().unwrap_or(0);
                let words = self.sif.sif1.pop_front().unwrap_or(0);
                self.sif.sif1.pop_front();
                self.sif.sif1.pop_front();
                self.sif.log_tag("IOP sif1", tag, words);
                self.dma.ch[10][0] = tag & 0x00FF_FFFF;
                self.dma.sif1_words = (words + 3) & !3;
                self.dma.sif1_end = tag & 0xC000_0000 != 0;
                moved = true;
                continue;
            }
            let Some(w) = self.sif.sif1.pop_front() else { break };
            let madr = self.dma.ch[10][0];
            self.set_ram_word(madr, w);
            self.sif.sif1_words += 1;
            self.dma.ch[10][0] = madr.wrapping_add(4);
            self.dma.sif1_words -= 1;
            if self.dma.sif1_words == 0 {
                let start = madr.wrapping_sub(60);
                let p: Vec<u32> = (0..16).map(|k| self.ram_word(start + 4 * k)).collect();
                self.sif.log_packet("EE->IOP", p);
            }
            moved = true;
        }
        moved
    }
}

fn physical(addr: u32) -> u32 {
    if addr == 0xFFFE_0130 { addr } else { addr & 0x1FFF_FFFF }
}

impl CpuBus for IopBus {
    fn fetch(&mut self, addr: u32) -> u32 {
        u32::from_le_bytes(self.read(addr))
    }
    fn load8(&mut self, addr: u32) -> u8 {
        self.read::<1>(addr)[0]
    }
    fn load16(&mut self, addr: u32) -> u16 {
        u16::from_le_bytes(self.read(addr))
    }
    fn load32(&mut self, addr: u32) -> u32 {
        u32::from_le_bytes(self.read(addr))
    }
    fn store8(&mut self, addr: u32, v: u8) {
        self.write(addr, [v]);
    }
    fn store16(&mut self, addr: u32, v: u16) {
        self.write(addr, v.to_le_bytes());
    }
    fn store32(&mut self, addr: u32, v: u32) {
        self.write(addr, v.to_le_bytes());
    }
    fn icache_enabled(&self) -> bool {
        self.cache_ctrl & 0x800 != 0
    }
    fn irq_active(&self) -> bool {
        self.intc.line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_interrupt_once_or_repeatedly() {
        let mut timers = Timers::default();
        let mut intc = Intc::default();
        // Timer 5: target 100, interrupt on target, zero return, repeat.
        timers.write(5, 8, 100);
        timers.write(5, 4, MODE_IRQ_TARGET | MODE_ZERO_RETURN | MODE_REPEAT);
        timers.advance(100, 0, &mut intc);
        assert_eq!(intc.stat, 1 << 16);
        assert_eq!(timers.read(5, 0), 0);
        intc.stat = 0;
        timers.advance(100, 0, &mut intc);
        assert_eq!(intc.stat, 1 << 16);
        // Timer 0, one-shot: only once.
        timers.write(0, 8, 10);
        timers.write(0, 4, MODE_IRQ_TARGET | MODE_ZERO_RETURN);
        intc.stat = 0;
        timers.advance(10, 0, &mut intc);
        timers.advance(10, 0, &mut intc);
        assert_eq!(intc.stat & (1 << 4), 1 << 4);
        assert_ne!(timers.read(0, 4) & MODE_TARGET_REACHED, 0);
        assert_eq!(timers.read(0, 4) & MODE_TARGET_REACHED, 0);
    }

    #[test]
    fn interrupt_control_clears_on_read() {
        let mut bus = IopBus::new(&[]);
        bus.store32(0xBF80_1074, 1);
        bus.store32(0xBF80_1078, 1);
        bus.intc.raise(0);
        assert!(bus.irq_active());
        assert_eq!(bus.load32(0xBF80_1078), 1);
        assert!(!bus.irq_active());
    }

    #[test]
    fn sif0_chain_and_sif1_blocks() {
        let mut bus = IopBus::new(&[]);
        // SIF0: one tag at 0x1000, ending, with 2 words at 0x2000.
        bus.store32(0x1000, 0x8000_2000);
        bus.store32(0x1004, 2);
        bus.store32(0x1008, 0xAAAA_0001);
        bus.store32(0x2000, 0x11);
        bus.store32(0x2004, 0x22);
        bus.dma.dicr = (1 << 23) | (1 << 16);
        bus.dma.dicr2 = 1 << (16 + 2);
        bus.store32(0xBF80_152C, 0x1000);
        bus.store32(0xBF80_1528, CHCR_BUSY);
        assert!(bus.pump_sif0());
        let words: Vec<u32> = bus.sif.sif0.drain(..).collect();
        assert_eq!(words, [0xAAAA_0001, 0, 0, 0, 0x11, 0x22, 0, 0]);
        bus.pump_sif0();
        assert_eq!(bus.dma.ch[9][2] & CHCR_BUSY, 0);
        assert_eq!(bus.intc.stat, 1 << irq::DMA);
        // SIF1: a block of 4 words to 0x3000.
        bus.sif.sif1.extend([0x8000_3000, 4, 0, 0, 1, 2, 3, 4]);
        bus.store32(0xBF80_1538, CHCR_BUSY);
        bus.pump_sif1();
        assert_eq!(bus.load32(0x300C), 4);
        assert_eq!(bus.dma.ch[10][2] & CHCR_BUSY, 0);
    }
}
