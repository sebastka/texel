//! The EE's memory map: RAM, BIOS ROM, scratchpad, VU memory, the IOP's RAM
//! as the EE sees it, and the hardware registers.
//!
//! Addresses here are physical: the CPU translates its virtual addresses
//! first (see `ee::tlb`). The scratchpad, which only the TLB maps, has a
//! physical address of its own here: [`SCRATCHPAD`].

use std::collections::BTreeMap;

use crate::gif::Gif;
use crate::gs::Gs;
use crate::intc::{Intc, Interrupt};
use crate::iop::{self, IOP_CLOCK_DIVIDER, IopBus};
use crate::sif::FIFO_WORDS;
use crate::timers::Timers;
use crate::vif::{Direct, Vif};
use crate::vu::Vu;
use crate::vu1q::Why;

/// The scratchpad channels' interleave sizes: skip (bits 0-7) and
/// transfer (bits 16-23), in quadwords.
const D_SQWC: u32 = 0x1000_E030;
/// The memory FIFO's ring buffer: its size, as a mask of quadword
/// addresses (RBSR), and where it is (RBOR).
const D_RBSR: u32 = 0x1000_E040;
const D_RBOR: u32 = 0x1000_E050;

pub const RAM_SIZE: usize = 32 * 1024 * 1024;
/// Writes to compiled code are watched by lines of this many bytes.
pub const CODE_LINE: usize = 64;
pub const BIOS_SIZE: usize = crate::bios::BIOS_SIZE;
const SCRATCHPAD_SIZE: usize = 16 * 1024;
/// Where the scratchpad is, for this bus: past the 512 MiB of physical
/// addresses, which nothing else uses.
pub const SCRATCHPAD: u32 = 0x2000_0000;

/// EE cycles per second (294.912 MHz); the bus runs at half that.
pub const EE_CLOCK: u64 = 294_912_000;
/// NTSC: a frame and a scanline, in EE cycles.
pub const FRAME_CYCLES: u64 = EE_CLOCK / 60;
const LINE_CYCLES: u64 = FRAME_CYCLES / 262;
/// The vertical blank starts with line 240.
const VBLANK_START: u64 = LINE_CYCLES * 240;
/// PAL: 50 fields a second, of 312.5 lines (15,625 a second); the
/// vertical blank starts after 288.
pub const PAL_FRAME_CYCLES: u64 = EE_CLOCK / 50;
const PAL_LINE_CYCLES: u64 = EE_CLOCK / 15_625;
const PAL_VBLANK_START: u64 = PAL_LINE_CYCLES * 288;

// Hardware registers.
pub const INTC_STAT: u32 = 0x1000_F000;
const INTC_MASK: u32 = 0x1000_F010;
const SIO_ISR: u32 = 0x1000_F130;
const SIO_TXFIFO: u32 = 0x1000_F180;
const MCH_RICM: u32 = 0x1000_F430;
const MCH_DRD: u32 = 0x1000_F440;
const MCH_STATUS: u32 = 0x1000_F410;
const D_ENABLER: u32 = 0x1000_F520;
const D_ENABLEW: u32 = 0x1000_F590;
const D_CTRL: u32 = 0x1000_E000;
const D_STAT: u32 = 0x1000_E010;
/// The GIF's mode (path 3's mask, intermittent mode) and status.
const GIF_MODE: u32 = 0x1000_3010;
const GIF_STAT: u32 = 0x1000_3020;
/// The GIF's DMA channel (2).
const GIF_BASE: u32 = 0x1000_A000;
const D_PCR: u32 = 0x1000_E020;
/// RDRAM chips: 2 × 16 MiB.
const RDRAM_DEVICES: u32 = 2;

/// The RDRAM controller, which the BIOS initialises at reset.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Mch {
    ricm: u32,
    drd: u32,
    sdevid: u32,
}

impl Mch {
    fn write_ricm(&mut self, value: u32) {
        // SA = INIT, SOP = 1, SIO repeater cleared: device numbering restarts.
        if (value >> 16) & 0xFFF == 0x21 && (value >> 6) & 0xF == 1 && (self.drd >> 7) & 1 == 0 {
            self.sdevid = 0;
        }
        // Never busy.
        self.ricm = value & !0x8000_0000;
    }

    fn read_drd(&mut self) -> u32 {
        if (self.ricm >> 6) & 0xF != 0 {
            return 0;
        }
        match (self.ricm >> 16) & 0xFFF {
            // INIT: each device answers once.
            0x21 if self.sdevid < RDRAM_DEVICES => {
                self.sdevid += 1;
                0x1F
            }
            // CNFGA, CNFGB: the devices' configuration.
            0x23 => 0x0D0D,
            0x24 => 0x0090,
            // DEVID.
            0x40 => self.ricm & 0x1F,
            _ => 0,
        }
    }
}

/// `Bus::ipu_sources`: the log, (quadwords taken, address, kind and
/// values) entries; the quadwords taken so far; the next address.
pub type IpuSources = (Vec<(u64, u32, u64)>, u64, u32);

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Bus {
    /// Written through `write` only, once code runs compiled from it (the
    /// writes to compiled code are watched there).
    pub ram: Box<[u8]>,
    /// RAM lines (64 bytes) holding compiled code, a bit each; and the ones
    /// written since.
    #[serde(skip)]
    code_lines: Box<[u64]>,
    #[serde(skip)]
    written_code: Vec<u32>,
    #[serde(skip)]
    pub bios: Box<[u8]>,
    pub scratchpad: Box<[u8]>,
    pub intc: Intc,
    pub timers: Timers,
    pub gs: Gs,
    pub gif: Gif,
    pub vif0: Vif,
    pub vif1: Vif,
    pub vu0: Vu,
    pub vu1: Vu,
    /// The IPU (MPEG decoder), and DMA channels 3 and 4 to and from it.
    pub ipu: crate::ipu::Ipu,
    /// VU1's work, queued until something needs what it owns (`vu1q`).
    #[serde(skip)]
    pub vu1q: crate::vu1q::Vu1Queue,
    /// CMSAR0: where VCALLMSR starts VU0.
    pub cmsar0: u32,
    /// The IOP's memory and devices, which the EE reaches too: its RAM
    /// at 0x1C000000, and the SIF.
    pub iop: IopBus,
    /// SIF transfers, EE side: the last block of the chain is under way;
    /// the DMA call stack of channel 6.
    sif0_end: bool,
    /// Source chains: the last block is under way; the call stacks.
    chain_end: [bool; 10],
    chain_stack: [Vec<u32>; 10],
    /// EE cycles not yet given to the IOP's timers.
    iop_remainder: u64,
    mch: Mch,
    d_stat: u32,
    d_enable: u32,
    /// Other hardware registers: kept as written.
    regs: HwRegs,
    /// What the kernel writes to its debug port.
    #[serde(skip)]
    pub sio_out: Vec<u8>,
    /// Accesses to unmapped addresses and unknown registers: address → count.
    #[serde(skip)]
    pub unknown: BTreeMap<u32, u64>,
    /// DMA transfers started (no transfer happens yet): channel address → count.
    #[serde(skip)]
    pub dma_starts: BTreeMap<(u32, u32), u64>,
    /// EE cycles into the current frame.
    frame_pos: u64,
    /// EE cycles not yet given to the timers (they run at half speed).
    bus_remainder: u64,
    line_pos: u64,
    pub frames: u64,
    /// The frame count, for logs.
    ee_cycles_hint: u32,
    /// The EE's writes to D_STAT, for diagnostics.
    #[serde(skip)]
    pub d_stat_writes: Vec<u32>,
    /// VIF1's MSKPATH3 was last set by a VIF1 DMA list now done (rather
    /// than written to its FIFO by the EE): see `path3_masked`.
    #[serde(skip)]
    path3_mask_by_dma: bool,
    ba6: u32,
    /// Diagnostics, when set: where channel 4 took the IPU's input from,
    /// as (quadwords taken before, address, 0) at each jump, (taken, TADR,
    /// tag) at each tag, (taken, MADR, CHCR | the next address before << 32
    /// | 1 << 63) at each start; and
    /// the quadwords taken so far, the next address.
    #[serde(skip)]
    pub ipu_sources: Option<IpuSources>,
    /// Diagnostics: lines of 64 bytes whose writes go into `ipu_sources`
    /// (taken, address, low 32 bits | 1 << 61).
    #[serde(skip)]
    pub ipu_watch: Vec<u32>,
}

impl Bus {
    /// After a save state was read into `self`: takes what states leave out
    /// (the ROMs, the disc, memory cards, how the GS renders) from `old`,
    /// the bus it replaces, and starts with no compiled code.
    pub(crate) fn transplant(&mut self, old: &mut Bus) {
        self.bios = std::mem::take(&mut old.bios);
        self.code_lines = vec![0; RAM_SIZE / CODE_LINE / 64].into_boxed_slice();
        self.iop.transplant(&mut old.iop);
        self.gs.rendering = old.gs.rendering;
        // VU1 (elsewhere, or here) goes on from the new state.
        let backend = old.vu1q.take_backend();
        self.vu1q.set_backend(backend, &self.vu1, &self.gif, &self.vif1);
    }

    pub fn new(bios: &[u8]) -> Self {
        let mut rom = vec![0u8; BIOS_SIZE].into_boxed_slice();
        let n = bios.len().min(BIOS_SIZE);
        rom[..n].copy_from_slice(&bios[..n]);
        Self {
            ram: vec![0; RAM_SIZE].into_boxed_slice(),
            code_lines: vec![0; RAM_SIZE / CODE_LINE / 64].into_boxed_slice(),
            written_code: Vec::new(),
            bios: rom,
            scratchpad: vec![0; SCRATCHPAD_SIZE].into_boxed_slice(),
            intc: Intc::default(),
            timers: Timers::default(),
            gs: Gs::default(),
            gif: Gif::default(),
            vif0: Vif::new(0),
            vif1: Vif::new(1),
            vu0: Vu::new(0),
            vu1: Vu::new(1),
            ipu: Default::default(),
            vu1q: Default::default(),
            cmsar0: 0,
            iop: IopBus::new(bios),
            sif0_end: false,
            chain_end: [false; 10],
            chain_stack: std::array::from_fn(|_| Vec::new()),
            iop_remainder: 0,
            mch: Mch::default(),
            d_stat: 0,
            d_enable: 0x1201,
            regs: HwRegs::default(),
            sio_out: Vec::new(),
            unknown: BTreeMap::new(),
            dma_starts: BTreeMap::new(),
            frame_pos: 0,
            bus_remainder: 0,
            line_pos: 0,
            frames: 0,
            ee_cycles_hint: 0,
            d_stat_writes: Vec::new(),
            path3_mask_by_dma: false,
            ba6: 0,
            ipu_sources: None,
            ipu_watch: Vec::new(),
        }
    }

    /// INT1: the DMA controller's interrupt (a channel's status bit, enabled
    /// by its mask bit).
    pub fn dmac_line(&self) -> bool {
        // The channels, and the stall, MFIFO empty and bus error causes.
        (self.d_stat & 0xE3FF) & ((self.d_stat >> 16) & 0xE3FF) != 0
    }

    /// COP0 condition (BC0F/BC0T): every channel D_PCR names is done.
    pub fn cpcond0(&self) -> bool {
        let pcr = self.regs.get(&D_PCR).copied().unwrap_or(0);
        (!pcr | self.d_stat) & 0x3FF == 0x3FF
    }

    /// Advances the timers and video timing by `cycles` EE cycles.
    pub fn advance(&mut self, cycles: u64) {
        if self.ipu.busy() || !self.ipu.output.is_empty() || self.reg(IPU_TO) & CHCR_STR != 0 {
            self.run_ipu();
        }
        // What the EE's slice raised (GS, VIFs) goes to INT0 at once; the
        // vertical blanks raised below wait a few cycles (`Intc::fresh`).
        self.sync_irqs();
        let raised = self.intc.stat;
        self.vu1q.now += cycles;
        self.bus_remainder += cycles;
        let bus_cycles = self.bus_remainder / 2;
        self.bus_remainder %= 2;
        let (frame_cycles, line_cycles, vblank_start) = self.timing();
        self.line_pos += cycles;
        let hblanks = self.line_pos / line_cycles;
        self.line_pos %= line_cycles;
        self.timers.advance(bus_cycles, hblanks, &mut self.intc);
        self.iop_remainder += cycles;
        let iop_cycles = self.iop_remainder / IOP_CLOCK_DIVIDER;
        self.iop_remainder %= IOP_CLOCK_DIVIDER;
        self.iop.advance(iop_cycles, hblanks);
        let before = self.frame_pos;
        self.frame_pos += cycles;
        if before < vblank_start && self.frame_pos >= vblank_start {
            self.intc.raise(Interrupt::VblankStart);
            self.iop.intc.raise(iop::irq::VBLANK_START);
            self.sync_vu1(Why::VBlank);
            self.gs.vblank();
        }
        if self.frame_pos >= frame_cycles {
            self.frame_pos -= frame_cycles;
            self.frames += 1;
            self.ee_cycles_hint = self.frames as u32;
            self.intc.raise(Interrupt::VblankEnd);
            self.iop.intc.raise(iop::irq::VBLANK_END);
        }
        // The vertical blanks only: the timers' interrupts come at once.
        let blanks = 1 << Interrupt::VblankStart as u32 | 1 << Interrupt::VblankEnd as u32;
        self.intc.fresh = self.intc.stat & !raised & blanks;
    }

    /// Whether the video timing is PAL's (as the GS's CRTC is set).
    pub fn pal(&self) -> bool {
        self.gs.privileged.pal()
    }

    /// A frame (a field), in EE cycles, at the video timing now.
    pub fn frame_cycles(&self) -> u64 {
        self.timing().0
    }

    /// A frame, a line and the start of the vertical blank, in EE cycles.
    fn timing(&self) -> (u64, u64, u64) {
        if self.pal() {
            (PAL_FRAME_CYCLES, PAL_LINE_CYCLES, PAL_VBLANK_START)
        } else {
            (FRAME_CYCLES, LINE_CYCLES, VBLANK_START)
        }
    }

    fn note_unknown(&mut self, addr: u32) {
        *self.unknown.entry(addr).or_insert(0) += 1;
    }

    // --- Memory --------------------------------------------------------------

    /// A memory region holding `pa` (physical), and the offset in it.
    fn memory(&mut self, pa: u32) -> Option<(&mut [u8], usize)> {
        let pa = pa as usize;
        match pa {
            0x2000_0000..0x2000_4000 => Some((&mut self.scratchpad, pa - 0x2000_0000)),
            0x0000_0000..0x0200_0000 => Some((&mut self.ram, pa)),
            // VU0's micro and data memory (4 KiB, mirrored), VU1's (16 KiB).
            0x1100_0000..0x1100_4000 => Some((&mut self.vu0.micro, (pa - 0x1100_0000) & 0xFFF)),
            0x1100_4000..0x1100_8000 => Some((&mut self.vu0.data, (pa - 0x1100_4000) & 0xFFF)),
            0x1100_8000..0x1100_C000 => Some((&mut self.vu1.micro, pa - 0x1100_8000)),
            0x1100_C000..0x1101_0000 => Some((&mut self.vu1.data, pa - 0x1100_C000)),
            0x1C00_0000..0x1C20_0000 => Some((&mut self.iop.ram, pa - 0x1C00_0000)),
            0x1FC0_0000..0x2000_0000 => Some((&mut self.bios, pa - 0x1FC0_0000)),
            _ => None,
        }
    }

    pub fn read<const N: usize>(&mut self, pa: u32) -> [u8; N] {
        let pa = pa & !(N as u32 - 1);
        if (0x1100_8000..0x1101_0000).contains(&pa) {
            self.sync_vu1(Why::VuMemory);
        }
        if let Some((mem, at)) = self.memory(pa) {
            let mut out = [0u8; N];
            if let Some(src) = mem.get(at..at + N) {
                out.copy_from_slice(src);
            }
            return out;
        }
        let v = self.io_read(pa, N);
        let bytes = v.to_le_bytes();
        let mut out = [0u8; N];
        out.copy_from_slice(&bytes[..N]);
        out
    }

    pub fn write<const N: usize>(&mut self, pa: u32, data: [u8; N]) {
        let pa = pa & !(N as u32 - 1);
        if self.ipu_watch.iter().any(|&w| pa.wrapping_sub(w) < 64)
            && let Some((log, taken, _)) = &mut self.ipu_sources
        {
            let mut low = [0u8; 4];
            let n = N.min(4);
            low[..n].copy_from_slice(&data[..n]);
            log.push((
                *taken,
                pa,
                u64::from(u32::from_le_bytes(low)) | (N as u64) << 32 | 1 << 61,
            ));
        }
        if (pa as usize) < RAM_SIZE {
            let line = pa / CODE_LINE as u32;
            let (word, bit) = ((line / 64) as usize, 1u64 << (line % 64));
            if self.code_lines[word] & bit != 0 {
                self.code_lines[word] &= !bit;
                self.written_code.push(line);
            }
        }
        // The BIOS is read-only.
        if (0x1FC0_0000..0x2000_0000).contains(&pa) {
            return;
        }
        if (0x1100_8000..0x1101_0000).contains(&pa) {
            self.sync_vu1(Why::VuMemory);
            // VU1 elsewhere: its copy too.
            self.vu1q.poke(pa - 0x1100_8000, &data);
        }
        // The VUs' micro memory: compiled programs check theirs again.
        if (0x1100_8000..0x1100_C000).contains(&pa) {
            self.vu1.micro_gen += 1;
        } else if (0x1100_0000..0x1100_4000).contains(&pa) {
            self.vu0.micro_gen += 1;
        }
        if let Some((mem, at)) = self.memory(pa) {
            if let Some(dst) = mem.get_mut(at..at + N) {
                dst.copy_from_slice(&data);
            }
            return;
        }
        let mut bytes = [0u8; 16];
        bytes[..N].copy_from_slice(&data);
        self.io_write(pa, N, u128::from_le_bytes(bytes));
    }

    /// Watches writes to RAM line `line` (`CODE_LINE` bytes): code was
    /// compiled from it.
    pub fn watch_code(&mut self, line: u32) {
        self.code_lines[(line / 64) as usize] |= 1 << (line % 64);
    }

    /// Where RAM, the bitmap of watched lines and the scratchpad are (for compiled code,
    /// which reads and writes them directly).
    pub fn addresses(&self) -> (usize, usize, usize) {
        (
            self.ram.as_ptr() as usize,
            self.code_lines.as_ptr() as usize,
            self.scratchpad.as_ptr() as usize,
        )
    }

    /// Whether watched lines were written.
    #[inline]
    pub fn code_written(&self) -> bool {
        !self.written_code.is_empty()
    }

    /// The watched lines written since the last call (no longer watched).
    pub fn take_written_code(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.written_code)
    }

    pub fn read8(&mut self, pa: u32) -> u8 {
        self.read::<1>(pa)[0]
    }
    pub fn read16(&mut self, pa: u32) -> u16 {
        u16::from_le_bytes(self.read(pa))
    }
    pub fn read32(&mut self, pa: u32) -> u32 {
        u32::from_le_bytes(self.read(pa))
    }
    pub fn read64(&mut self, pa: u32) -> u64 {
        u64::from_le_bytes(self.read(pa))
    }
    pub fn read128(&mut self, pa: u32) -> u128 {
        u128::from_le_bytes(self.read(pa))
    }
    pub fn write8(&mut self, pa: u32, v: u8) {
        self.write(pa, [v]);
    }
    pub fn write16(&mut self, pa: u32, v: u16) {
        self.write(pa, v.to_le_bytes());
    }
    pub fn write32(&mut self, pa: u32, v: u32) {
        self.write(pa, v.to_le_bytes());
    }
    pub fn write64(&mut self, pa: u32, v: u64) {
        self.write(pa, v.to_le_bytes());
    }
    pub fn write128(&mut self, pa: u32, v: u128) {
        self.write(pa, v.to_le_bytes());
    }

    // --- Hardware registers --------------------------------------------------

    fn io_read(&mut self, pa: u32, width: usize) -> u128 {
        match pa {
            0x1200_0000..0x1200_2000 => {
                self.sync_vu1(Why::Gs);
                let off = pa - 0x1200_0000;
                match width {
                    8 | 16 => u128::from(self.gs.read64(off & !7)),
                    _ => u128::from(self.gs.read32(off & !3) >> ((off & 3) * 8)),
                }
            }
            // The IPU's output FIFO.
            0x1000_7000..0x1000_7010 if width == 16 => {
                let q = self.ipu.read_fifo();
                self.run_ipu();
                q
            }
            0x1000_0000..0x1001_0000 => match width {
                16 => (0..4).fold(0u128, |acc, i| {
                    acc | (u128::from(self.hw_read32(pa + i * 4)) << (32 * i))
                }),
                8 => u128::from(self.hw_read32(pa)) | (u128::from(self.hw_read32(pa + 4)) << 32),
                _ => u128::from(self.hw_read32(pa & !3) >> ((pa & 3) * 8)),
            },
            // The CDVD's registers, which the EE reads too (EELOAD checks
            // the disc type there): bytes.
            0x1F40_2000..0x1F40_2040 => (0..width.min(4) as u32).fold(0u128, |acc, i| {
                acc | (u128::from(self.iop.cdvd.read(pa - 0x1F40_2000 + i)) << (8 * i))
            }),
            // A register of the development tools' hardware, which the
            // BIOS polls: on retail consoles, it cycles through 1, 2, 0.
            0x1A00_0006 => {
                self.ba6 = (self.ba6 + 1) % 3;
                u128::from(self.ba6)
            }
            _ => {
                self.note_unknown(pa);
                0
            }
        }
    }

    fn io_write(&mut self, pa: u32, width: usize, value: u128) {
        match pa {
            0x1200_0000..0x1200_2000 => {
                self.sync_vu1(Why::Gs);
                let off = pa - 0x1200_0000;
                match width {
                    8 | 16 => self.gs.write64(off & !7, value as u64),
                    _ => self.gs.write32(off & !3, (value as u32) << ((off & 3) * 8)),
                }
            }
            // The FIFOs of VIF0, VIF1 and the GIF (path 3).
            0x1000_4000..0x1000_7000 if width == 16 => {
                let words: [u32; 4] = std::array::from_fn(|k| (value >> (32 * k)) as u32);
                match pa & 0xF000 {
                    0x4000 => self.vif0.write(
                        &words,
                        &mut Direct {
                            vu: &mut self.vu0,
                            gif: &mut self.gif,
                            gs: &mut self.gs,
                        },
                    ),
                    0x5000 => {
                        self.vu1q.vif1(&mut self.vif1, &mut self.vu1, &words);
                        self.path3_mask_by_dma = false;
                        self.resume_path3();
                    }
                    _ => {
                        self.sync_vu1(Why::Gif);
                        self.gif.write(2, value, &mut self.gs);
                    }
                }
            }
            // The IPU's input FIFO.
            0x1000_7010..0x1000_7020 => {
                if width == 16 {
                    self.ipu.write_fifo(value);
                } else {
                    self.ipu.write_fifo_word((pa >> 2) & 3, value as u32);
                }
                self.run_ipu();
            }
            0x1000_0000..0x1001_0000 => match width {
                16 | 8 => {
                    for i in 0..(width / 4) as u32 {
                        self.hw_write32(pa + i * 4, (value >> (32 * i)) as u32);
                    }
                }
                _ if pa & !3 == SIO_TXFIFO => self.sio_out.push(value as u8),
                _ => self.hw_write32(pa & !3, (value as u32) << ((pa & 3) * 8)),
            },
            _ => self.note_unknown(pa),
        }
    }

    fn hw_read32(&mut self, pa: u32) -> u32 {
        match pa {
            0x1000_0000..0x1000_2000 => self.timers.read(pa - 0x1000_0000),
            INTC_STAT => self.intc.stat,
            INTC_MASK => self.intc.mask,
            D_STAT => self.d_stat,
            // Path 3 masked by GIF_MODE (M3R) or by VIF1's MSKPATH3 (M3P),
            // the intermittent mode (IMT). Transfers end at once here, but
            // one path 3 holds back while masked: queued (IP3, P3Q), the
            // FIFO full of it (FQC).
            GIF_STAT => {
                let mode = self.reg(GIF_MODE);
                let mut stat = (mode & 1) | u32::from(self.vif1.mask_path3) << 1 | (mode & 4);
                if self.path3_waiting() {
                    let qwc = self.reg(GIF_BASE + QWC) & 0xFFFF;
                    let fifo = if qwc == 0 { 16 } else { qwc.min(16) };
                    stat |= 1 << 5 | 1 << 6 | fifo << 24;
                }
                stat
            }
            SIO_ISR => 0,
            IPU_CMD => self.ipu.read_cmd().0,
            IPU_CMD_HI => u32::from(self.ipu.read_cmd().1) << 31,
            IPU_CTRL => self.ipu.read_ctrl(),
            IPU_BP => self.ipu.read_bp(),
            IPU_TOP | IPU_TOP_HI => {
                let mut ipu = std::mem::take(&mut self.ipu);
                let (top, short) = ipu.read_top(&mut || self.ipu_pull());
                self.ipu = ipu;
                if pa == IPU_TOP { top } else { u32::from(short) << 31 }
            }
            0x1000_F200..0x1000_F270 => self.iop.sif.ee_read(pa - 0x1000_F200),
            // MCH_RICM, and the register next to it: never busy.
            MCH_RICM | MCH_STATUS => 0,
            MCH_DRD => self.mch.read_drd(),
            D_ENABLER => self.d_enable,
            _ => match self.regs.get(&pa) {
                Some(&v) => v,
                None => {
                    if !is_known_register(pa) {
                        self.note_unknown(pa);
                    }
                    0
                }
            },
        }
    }

    fn hw_write32(&mut self, pa: u32, value: u32) {
        match pa {
            0x1000_0000..0x1000_2000 => self.timers.write(pa - 0x1000_0000, value),
            IPU_CMD => {
                self.ipu.write_cmd(value);
                self.run_ipu();
            }
            IPU_CTRL => {
                self.ipu.write_ctrl(value);
                self.run_ipu();
            }
            INTC_STAT => self.intc.write_stat(value),
            INTC_MASK => self.intc.write_mask(value),
            // Lifting GIF_MODE's mask (M3R) lets a waiting path 3 go.
            GIF_MODE => {
                self.regs.insert(GIF_MODE, value);
                self.resume_path3();
            }
            D_STAT => {
                if self.d_stat_writes.len() < 32 {
                    self.d_stat_writes.push(value);
                }
                let status = self.d_stat & 0xFFFF & !(value & 0xFFFF);
                let mask = (self.d_stat ^ value) & 0xFFFF_0000;
                self.d_stat = status | mask;
            }
            SIO_TXFIFO => self.sio_out.push(value as u8),
            0x1000_F200..0x1000_F270 => self.iop.sif.ee_write(pa - 0x1000_F200, value),
            MCH_RICM => self.mch.write_ricm(value),
            MCH_DRD => self.mch.drd = value,
            D_ENABLEW => self.d_enable = value,
            _ => {
                if !is_known_register(pa) {
                    self.note_unknown(pa);
                }
                let started = dma_channel(pa).filter(|_| value & CHCR_STR != 0 && self.reg(pa) & CHCR_STR == 0);
                self.regs.insert(pa, value);
                if let Some(ch @ (SIF0 | SIF1)) = started {
                    self.iop.sif.log_tag(
                        if ch == SIF0 { "EE start ch5" } else { "EE start ch6" },
                        value,
                        self.ee_cycles_hint,
                    );
                }
                match started {
                    Some(SIF0) => self.sif0_end = false,
                    Some(ch) => {
                        // A chain started with data left (QWC) goes on from
                        // the tag CHCR keeps: after an end tag (REFE or END;
                        // a destination chain's END), or one asking for an
                        // interrupt with TIE set, it stops once that data is
                        // through. GTA III's movies restart the IPU's input
                        // so, in the block their last REFE gave.
                        let id = value >> 28 & 7;
                        let end_tag = id == 7 || (id == 0 && ch != 8);
                        let irq = value >> 31 != 0 && value & CHCR_TIE != 0;
                        let chain = value >> 2 & 3 == 1;
                        let qwc = self.reg(pa + QWC) & 0xFFFF;
                        self.chain_end[ch as usize] = chain && qwc > 0 && (end_tag || irq);
                        self.chain_stack[ch as usize].clear();
                        *self.dma_starts.entry((pa, value & 0x4C)).or_insert(0) += 1;
                        match ch {
                            // Moved as the SIF has room (pump_sif).
                            SIF1 => {}
                            0..=2 | 8 | 9 => self.run_dma(ch),
                            // Moved as the IPU takes and gives (run_ipu).
                            3 | 4 => {
                                let madr = self.reg(IPU_TO + MADR);
                                if ch == 4
                                    && let Some((log, taken, next)) = &mut self.ipu_sources
                                {
                                    log.push((*taken, madr, u64::from(value) | u64::from(*next) << 32 | 1 << 63));
                                    let (ifc, fp) = self.ipu.counts();
                                    let bp = self.ipu.read_bp();
                                    log.push((*taken, bp, u64::from(ifc) << 8 | u64::from(fp) | 1 << 62));
                                }
                                self.run_ipu()
                            }
                            // Not emulated yet: done at once.
                            _ => self.dma_complete(ch),
                        }
                    }
                    None => {}
                }
            }
        }
    }
}

// --- EE DMA ---------------------------------------------------------------------

/// CHCR: start (and busy), the tag interrupt enable, the tag transfer
/// enable.
const CHCR_STR: u32 = 1 << 8;
const CHCR_TIE: u32 = 1 << 7;
const CHCR_TTE: u32 = 1 << 6;
/// Channels: their CHCR, and their number (D_STAT bit).
const CHANNELS: [(u32, u32); 10] = [
    (0x1000_8000, 0),
    (0x1000_9000, 1),
    (0x1000_A000, 2),
    (0x1000_B000, 3),
    (0x1000_B400, 4),
    (0x1000_C000, 5),
    (0x1000_C400, 6),
    (0x1000_C800, 7),
    (0x1000_D000, 8),
    (0x1000_D400, 9),
];
const SIF0: u32 = 5;
/// The scratchpad's channel towards memory (8), the memory FIFO's source.
const SPR_FROM_BASE: u32 = 0x1000_D000;
const SIF1: u32 = 6;
const SIF0_BASE: u32 = 0x1000_C000;
const SIF1_BASE: u32 = 0x1000_C400;
// Register offsets: CHCR, MADR, QWC, TADR.
const MADR: u32 = 0x10;
const QWC: u32 = 0x20;
const TADR: u32 = 0x30;

const IPU_FROM: u32 = 0x1000_B000;
const IPU_TO: u32 = 0x1000_B400;
const IPU_CMD: u32 = 0x1000_2000;
const IPU_CMD_HI: u32 = 0x1000_2004;
const IPU_CTRL: u32 = 0x1000_2010;
const IPU_BP: u32 = 0x1000_2020;
const IPU_TOP: u32 = 0x1000_2030;
const IPU_TOP_HI: u32 = 0x1000_2034;

/// The channel whose CHCR is at `pa`.
fn dma_channel(pa: u32) -> Option<u32> {
    CHANNELS.iter().find(|(base, _)| *base == pa).map(|&(_, ch)| ch)
}

impl Bus {
    /// A hardware register's value, for diagnostics.
    pub fn register(&self, pa: u32) -> u32 {
        match pa {
            D_STAT => self.d_stat,
            _ => self.reg(pa),
        }
    }

    fn reg(&self, pa: u32) -> u32 {
        self.regs.get(&pa).copied().unwrap_or(0)
    }

    fn dma_complete(&mut self, ch: u32) {
        let base = CHANNELS[ch as usize].0;
        let chcr = self.reg(base);
        self.regs.insert(base, chcr & !CHCR_STR);
        self.d_stat |= 1 << ch;
    }

    /// A DMA address: RAM, or the scratchpad (bit 31).
    fn dma_address(addr: u32) -> u32 {
        if addr & 0x8000_0000 != 0 {
            SCRATCHPAD + (addr & 0x3FF0)
        } else {
            addr & 0x01FF_FFF0
        }
    }

    /// Moves data through the SIF, both ways, as far as it goes.
    pub fn pump_sif(&mut self) {
        loop {
            let mut moved = self.pump_ee_sif1();
            moved |= self.iop.pump_sif1();
            moved |= self.iop.pump_sif0();
            moved |= self.pump_ee_sif0();
            if !moved {
                break;
            }
        }
    }

    /// Channel 5: from the SIF0 FIFO into EE memory, by destination chain:
    /// each block starts with a DMA tag (count, ID, address).
    fn pump_ee_sif0(&mut self) -> bool {
        let mut moved = false;
        while self.reg(SIF0_BASE) & CHCR_STR != 0 {
            let qwc = self.reg(SIF0_BASE + QWC) & 0xFFFF;
            if qwc == 0 && self.sif0_end {
                self.dma_complete(SIF0);
                return true;
            }
            if self.iop.sif.sif0.len() < 4 {
                break;
            }
            let w: Vec<u32> = self.iop.sif.sif0.drain(..4).collect();
            if qwc == 0 {
                self.iop.sif.log_tag("EE sif0", w[0], w[1]);
                let chcr = self.reg(SIF0_BASE);
                self.regs.insert(SIF0_BASE, (chcr & 0xFFFF) | (w[0] & 0xFFFF_0000));
                self.regs.insert(SIF0_BASE + MADR, w[1]);
                self.regs.insert(SIF0_BASE + QWC, w[0] & 0xFFFF);
                let id = (w[0] >> 28) & 7;
                self.sif0_end = id == 7 || (w[0] >> 31 != 0 && chcr & CHCR_TIE != 0);
            } else {
                let madr = self.reg(SIF0_BASE + MADR);
                let v =
                    u128::from(w[0]) | (u128::from(w[1]) << 32) | (u128::from(w[2]) << 64) | (u128::from(w[3]) << 96);
                self.write128(Self::dma_address(madr), v);
                self.regs.insert(SIF0_BASE + MADR, madr.wrapping_add(16));
                self.regs.insert(SIF0_BASE + QWC, qwc - 1);
            }
            moved = true;
        }
        moved
    }

    /// Channel 6: from EE memory into the SIF1 FIFO, in normal mode or by
    /// source chain (DMA tags in memory).
    fn pump_ee_sif1(&mut self) -> bool {
        let mut moved = false;
        while self.reg(SIF1_BASE) & CHCR_STR != 0 && self.iop.sif.sif1.len() + 4 <= FIFO_WORDS {
            let chcr = self.reg(SIF1_BASE);
            let chain = (chcr >> 2) & 3 == 1;
            let qwc = self.reg(SIF1_BASE + QWC) & 0xFFFF;
            if qwc == 0 {
                if !chain || self.chain_end[SIF1 as usize] {
                    self.dma_complete(SIF1);
                    return true;
                }
                let tag = self.chain_tag(SIF1);
                self.iop.sif.log_tag("EE sif1", tag as u32, (tag >> 32) as u32);
                if chcr & CHCR_TTE != 0 {
                    // The tag's upper half goes first.
                    self.iop.sif.sif1.push_back((tag >> 64) as u32);
                    self.iop.sif.sif1.push_back((tag >> 96) as u32);
                }
                moved = true;
                continue;
            }
            let madr = self.reg(SIF1_BASE + MADR);
            let v = self.read128(Self::dma_address(madr));
            for k in 0..4 {
                self.iop.sif.sif1.push_back((v >> (32 * k)) as u32);
            }
            self.regs.insert(SIF1_BASE + MADR, madr.wrapping_add(16));
            self.regs.insert(SIF1_BASE + QWC, qwc - 1);
            moved = true;
        }
        moved
    }

    /// Runs the IPU's command as far as its input goes, feeds its input FIFO
    /// from DMA channel 4, drains its output to channel 3; its interrupt.
    fn run_ipu(&mut self) {
        let mut ipu = std::mem::take(&mut self.ipu);
        if let Some(t) = ipu.trace.as_mut() {
            t.frame = self.frames;
        }
        ipu.run(&mut || self.ipu_pull());
        while ipu.wants_input() {
            match self.ipu_pull() {
                Some(q) => ipu.write_fifo(q),
                None => break,
            }
        }
        self.ipu = ipu;
        // Channel 3: the output into memory, in normal mode.
        while self.reg(IPU_FROM) & CHCR_STR != 0 {
            let qwc = self.reg(IPU_FROM + QWC) & 0xFFFF;
            if qwc == 0 {
                self.dma_complete(3);
                break;
            }
            let Some(q) = self.ipu.output.pop_front() else { break };
            let madr = self.reg(IPU_FROM + MADR);
            self.write128(Self::dma_address(madr), q);
            self.regs.insert(IPU_FROM + MADR, madr.wrapping_add(16));
            self.regs.insert(IPU_FROM + QWC, qwc - 1);
        }
        if std::mem::take(&mut self.ipu.irq) {
            self.intc.raise(Interrupt::Ipu);
        }
    }

    /// Channel 4's next quadword for the IPU (normal or source chain mode),
    /// if it runs and has one.
    fn ipu_pull(&mut self) -> Option<u128> {
        loop {
            let chcr = self.reg(IPU_TO);
            if chcr & CHCR_STR == 0 {
                return None;
            }
            let chain = (chcr >> 2) & 3 == 1;
            let qwc = self.reg(IPU_TO + QWC) & 0xFFFF;
            if qwc == 0 {
                if !chain || self.chain_end[4] {
                    self.dma_complete(4);
                    return None;
                }
                self.chain_tag(4);
                continue;
            }
            let madr = self.reg(IPU_TO + MADR);
            let q = self.read128(Self::dma_address(madr));
            if let Some((log, taken, next)) = &mut self.ipu_sources {
                if madr != *next {
                    log.push((*taken, madr, 0));
                }
                *taken += 1;
                *next = madr.wrapping_add(16);
            }
            self.regs.insert(IPU_TO + MADR, madr.wrapping_add(16));
            self.regs.insert(IPU_TO + QWC, qwc - 1);
            if qwc == 1 && (!chain || self.chain_end[4]) {
                self.dma_complete(4);
            }
            return Some(q);
        }
    }

    /// Reads a channel's next source chain tag: sets MADR, QWC and TADR
    /// for the block it describes, and returns it.
    fn chain_tag(&mut self, ch: u32) -> u128 {
        let base = CHANNELS[ch as usize].0;
        let chcr = self.reg(base);
        let tadr = self.mfifo_addr(ch, self.reg(base + TADR));
        let tag = self.read128(Self::dma_address(tadr));
        let lo = tag as u64;
        if ch == 4
            && let Some((log, taken, _)) = &mut self.ipu_sources
        {
            log.push((*taken, tadr, lo));
        }
        let qwc = (lo & 0xFFFF) as u32;
        let id = ((lo >> 28) & 7) as u32;
        let addr = ((lo >> 32) & 0xFFFF_FFF0) as u32;
        self.regs.insert(base, (chcr & 0xFFFF) | ((lo as u32) & 0xFFFF_0000));
        // The memory FIFO's drain: addresses that follow the tag (its data,
        // the next tag) wrap in the ring buffer, however far past its end.
        let in_ring = self.mfifo_drain() == Some(ch);
        let follow = |a: u32| {
            if in_ring {
                (a & self.reg(D_RBSR)) | self.reg(D_RBOR)
            } else {
                a
            }
        };
        let after = follow(tadr.wrapping_add(16));
        let past_data = follow(after.wrapping_add(qwc * 16));
        // TADR moves past every tag, REFE included: the kernel turns its
        // last REFE into a REF to append, and restarts without setting TADR.
        let stack = &mut self.chain_stack[ch as usize];
        let (madr, next, mut end) = match id {
            // REFE, CNT, NEXT, REF, REFS, CALL, RET, END.
            0 => (addr, after, true),
            1 => (after, past_data, false),
            2 => (after, addr, false),
            3 | 4 => (addr, after, false),
            5 => {
                stack.push(past_data);
                (after, addr, false)
            }
            6 => match stack.pop() {
                Some(ret) => (after, ret, false),
                None => (after, tadr, true),
            },
            _ => (after, tadr, true),
        };
        if lo >> 31 & 1 != 0 && chcr & CHCR_TIE != 0 {
            end = true;
        }
        self.chain_end[ch as usize] = end;
        let (madr, next) = (self.mfifo_addr(ch, madr), self.mfifo_addr(ch, next));
        self.regs.insert(base + MADR, madr);
        self.regs.insert(base + QWC, qwc);
        self.regs.insert(base + TADR, next);
        tag
    }

    /// The memory FIFO's drain, when on (D_CTRL's MFD: VIF1 or the GIF):
    /// the scratchpad's channel (8) fills a ring buffer in memory, which
    /// the drain's source chain reads, its tags and their data wrapping in
    /// the ring.
    fn mfifo_drain(&self) -> Option<u32> {
        match (self.reg(D_CTRL) >> 2) & 3 {
            2 => Some(1),
            3 => Some(2),
            _ => None,
        }
    }

    /// An address in the ring buffer, wrapped.
    fn ring(&self, addr: u32) -> u32 {
        (addr & self.reg(D_RBSR)) | self.reg(D_RBOR)
    }

    /// Channel `ch`'s address, wrapped when it is the drain's in the ring
    /// (or just past its end); others (a REF tag's data) as they are.
    fn mfifo_addr(&self, ch: u32, addr: u32) -> u32 {
        if self.mfifo_drain() != Some(ch) {
            return addr;
        }
        let (rbor, rbsr) = (self.reg(D_RBOR), self.reg(D_RBSR));
        if (rbor..=rbor.wrapping_add(rbsr).wrapping_add(16)).contains(&addr) {
            self.ring(addr)
        } else {
            addr
        }
    }

    /// Path 3 is masked, by GIF_MODE or VIF1's MSKPATH3. A mask a VIF1
    /// DMA list left on does not hold transfers started after the list:
    /// VIF1 runs its lists at once here, where on the console the list
    /// would still be running, the transfer going before it reaches the
    /// mask (games end a list masking path 3, start an upload, and wait
    /// for it before the next list, which unmasks it). One the EE writes
    /// to VIF1's FIFO holds, as on the console.
    fn path3_masked(&self) -> bool {
        self.reg(GIF_MODE) & 1 != 0 || (self.vif1.mask_path3 && !self.path3_mask_by_dma)
    }

    /// A path 3 transfer started, held back by the mask.
    fn path3_waiting(&self) -> bool {
        self.reg(GIF_BASE) & CHCR_STR != 0 && self.path3_masked()
    }

    /// Runs path 3's transfer if it waited for the mask, now lifted.
    fn resume_path3(&mut self) {
        if self.reg(GIF_BASE) & CHCR_STR != 0 && !self.path3_masked() {
            self.run_dma(2);
        }
    }

    /// Runs a transfer to VIF0, VIF1, the GIF or the scratchpad at once,
    /// in normal or source chain mode.
    fn run_dma(&mut self, ch: u32) {
        let base = CHANNELS[ch as usize].0;
        // Path 3 masked: it waits (started, its data in the FIFO for
        // GIF_STAT) until the mask is lifted (`resume_path3`).
        if ch == 2 && self.path3_masked() {
            return;
        }
        // VIF1 towards memory (reading the GS back): not emulated.
        if ch == 1 && self.reg(base) & 1 == 0 {
            self.dma_complete(ch);
            return;
        }
        // Interleave mode (the scratchpad's channels): after every TQWC
        // quadwords, memory skips SQWC (D_SQWC); the scratchpad's side is
        // contiguous.
        let interleave = (ch == 8 || ch == 9) && (self.reg(base) >> 2) & 3 == 2;
        let sqwc = self.reg(D_SQWC);
        let (skip, run) = (sqwc & 0xFF, (sqwc >> 16) & 0xFF);
        let mut in_run = 0;
        for _ in 0..0x40_0000 {
            let chcr = self.reg(base);
            let chain = (chcr >> 2) & 3 == 1 && ch != 8;
            let qwc = self.reg(base + QWC) & 0xFFFF;
            if qwc == 0 {
                if !chain || self.chain_end[ch as usize] {
                    break;
                }
                // The memory FIFO's drain, caught up with what the
                // scratchpad's channel wrote: it waits (MFIFO empty).
                if self.mfifo_drain() == Some(ch)
                    && self.mfifo_addr(ch, self.reg(base + TADR)) == self.reg(SPR_FROM_BASE + MADR)
                {
                    self.d_stat |= 1 << 14;
                    return;
                }
                let tag = self.chain_tag(ch);
                if chcr & CHCR_TTE != 0 && ch <= 1 {
                    let words = [(tag >> 64) as u32, (tag >> 96) as u32];
                    self.dma_sink(ch, &words, 0);
                } else if chcr & CHCR_TTE != 0 && ch == 9 {
                    // To the scratchpad: the whole tag, before its data.
                    self.dma_sink(ch, &[], tag);
                }
                continue;
            }
            let madr = self.mfifo_addr(ch, self.reg(base + MADR));
            let mfifo_fill = ch == 8 && self.mfifo_drain().is_some();
            let q = if ch == 8 {
                // From the scratchpad (SADR) to memory: into the ring
                // buffer when the memory FIFO is on.
                let sadr = self.reg(base + 0x80);
                self.regs.insert(base + 0x80, sadr.wrapping_add(16) & 0x3FF0);
                let v = self.read128(SCRATCHPAD + (sadr & 0x3FF0));
                let to = if mfifo_fill { self.ring(madr) } else { madr };
                self.write128(Self::dma_address(to), v);
                None
            } else {
                Some(self.read128(Self::dma_address(madr)))
            };
            let mut next = madr.wrapping_add(16);
            if mfifo_fill {
                next = self.ring(next);
            }
            if interleave && run > 0 {
                in_run += 1;
                if in_run == run {
                    in_run = 0;
                    next = next.wrapping_add(16 * skip);
                }
            }
            self.regs.insert(base + MADR, next);
            self.regs.insert(base + QWC, qwc - 1);
            if let Some(q) = q {
                if ch == 2 {
                    self.gs.dma_source = madr;
                }
                let words: [u32; 4] = std::array::from_fn(|k| (q >> (32 * k)) as u32);
                self.dma_sink(ch, &words, q);
            }
        }
        self.dma_complete(ch);
        // VIF1's data may have lifted path 3's mask; one it left on is a
        // list's (see `path3_masked`).
        if ch == 1 {
            self.path3_mask_by_dma = self.vif1.mask_path3;
            self.resume_path3();
        }
        // The memory FIFO filled: its drain goes on.
        if ch == 8
            && let Some(drain) = self.mfifo_drain()
            && self.reg(CHANNELS[drain as usize].0) & CHCR_STR != 0
        {
            self.run_dma(drain);
        }
    }

    /// Where a channel's data goes.
    fn dma_sink(&mut self, ch: u32, words: &[u32], q: u128) {
        match ch {
            0 => self.vif0.write(
                words,
                &mut Direct {
                    vu: &mut self.vu0,
                    gif: &mut self.gif,
                    gs: &mut self.gs,
                },
            ),
            1 => self.vu1q.vif1(&mut self.vif1, &mut self.vu1, words),
            2 => {
                self.sync_vu1(Why::Gif);
                self.gif.write(2, q, &mut self.gs);
            }
            _ => {
                // To the scratchpad, at SADR.
                let sadr_reg = CHANNELS[ch as usize].0 + 0x80;
                let sadr = self.reg(sadr_reg);
                self.regs.insert(sadr_reg, sadr.wrapping_add(16) & 0x3FF0);
                self.write128(SCRATCHPAD + (sadr & 0x3FF0), q);
            }
        }
    }

    /// Runs VU1's queued work (see `vu1q`): before anything reads or
    /// changes what VU1, the GIF or the GS own.
    pub fn sync_vu1(&mut self, why: Why) {
        self.vu1q.run(&mut self.vu1, &mut self.gif, &mut self.gs, why);
    }

    /// Interrupts from the GS and the VIFs, to the INTC.
    pub fn sync_irqs(&mut self) {
        if std::mem::take(&mut self.gs.irq) {
            self.intc.raise(Interrupt::Gs);
        }
        if std::mem::take(&mut self.vif0.irq) {
            self.intc.raise(Interrupt::Vif0);
        }
        if std::mem::take(&mut self.vif1.irq) {
            self.intc.raise(Interrupt::Vif1);
        }
    }
}

/// Registers kept as written: those from 0x10000000 (DMA, VIFs, GIF, IPU:
/// written per quadword moved) in an array, the others in a map.
#[derive(serde::Serialize, serde::Deserialize)]
struct HwRegs {
    hw: Box<[Option<u32>]>,
    other: BTreeMap<u32, u32>,
}

const HW_REGS: std::ops::Range<u32> = 0x1000_0000..0x1001_0000;

impl Default for HwRegs {
    fn default() -> Self {
        Self {
            hw: vec![None; HW_REGS.len() / 4].into_boxed_slice(),
            other: BTreeMap::new(),
        }
    }
}

impl HwRegs {
    #[inline]
    fn get(&self, pa: &u32) -> Option<&u32> {
        if HW_REGS.contains(pa) {
            self.hw[((pa - HW_REGS.start) / 4) as usize].as_ref()
        } else {
            self.other.get(pa)
        }
    }

    #[inline]
    fn insert(&mut self, pa: u32, v: u32) {
        if HW_REGS.contains(&pa) {
            self.hw[((pa - HW_REGS.start) / 4) as usize] = Some(v);
        } else {
            self.other.insert(pa, v);
        }
    }
}

/// Registers kept as written without being reported: those of the DMA
/// controller, the VIFs, GIF and IPU, which hold no state of their own here.
fn is_known_register(pa: u32) -> bool {
    matches!(pa, 0x1000_2000..0x1000_4000 | 0x1000_8000..0x1000_F000) || pa == D_CTRL || pa == D_PCR
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PAL timing once the game sets the CRTC's SYNCV for it (625
    /// half-lines a field): 50 frames a second, rather than 60 (GTA III's
    /// movies dropped pictures, their clock ahead of the vertical blanks).
    #[test]
    fn syncv_sets_pal_timing() {
        let mut bus = Bus::new(&[]);
        assert!(!bus.pal());
        assert_eq!(bus.frame_cycles(), EE_CLOCK / 60);
        bus.write64(0x1200_0060, 0x00A9_0005_0210_1401);
        assert!(bus.pal());
        assert_eq!(bus.frame_cycles(), EE_CLOCK / 50);
        let frames = bus.frames;
        for _ in 0..50 {
            bus.advance(EE_CLOCK / 50);
        }
        assert_eq!(bus.frames - frames, 50);
        // NTSC's values: 525 half-lines.
        bus.write64(0x1200_0060, 0x00C7_8006_01A0_1801);
        assert!(!bus.pal());
    }

    /// A source chain restarted with data left stops after it when CHCR's
    /// tag is an end tag (REFE), and goes on to the next tag otherwise.
    #[test]
    fn a_restarted_chain_ends_as_its_tag_says() {
        let to_spr = CHANNELS[9].0;
        for (chcr, quadwords) in [(0x0000_0105, 2u32), (0x3000_0105, 3)] {
            let mut bus = Bus::new(&[]);
            bus.write32(D_CTRL, 1);
            // Two quadwords left at 0x1000; the next tag: a REFE of one at 0x3000.
            for k in 0..3 {
                bus.write128(0x1000 + 16 * k, u128::from(k) + 1);
            }
            bus.write128(0x3000, 3);
            bus.write64(0x2000, 0x3000 << 32 | 1);
            bus.write32(to_spr + MADR, 0x1000);
            bus.write32(to_spr + QWC, 2);
            bus.write32(to_spr + TADR, 0x2000);
            bus.write32(to_spr + 0x80, 0);
            bus.write32(to_spr, chcr);
            let got: Vec<u128> = (0..4).map(|k| bus.read128(SCRATCHPAD + 16 * k)).collect();
            let want: Vec<u128> = [1, 2, 3, 0]
                .iter()
                .enumerate()
                .map(|(k, &v)| if k < quadwords as usize { v } else { 0 })
                .collect();
            assert_eq!(got, want, "CHCR {chcr:08X}");
            assert_eq!(bus.read32(to_spr) & CHCR_STR, 0);
        }
    }

    /// VIF1's MSKPATH3 shows in GIF_STAT (M3P), as games wait for it;
    /// GIF_MODE's mask too (M3R).
    #[test]
    fn path3_masks_show_in_gif_stat() {
        let mut bus = Bus::new(&[]);
        assert_eq!(bus.read32(GIF_STAT) & 3, 0);
        bus.write128(0x1000_5000, 0x0600_8000);
        assert_eq!(bus.read32(GIF_STAT) & 3, 2);
        bus.write128(0x1000_5000, 0x0600_0000);
        assert_eq!(bus.read32(GIF_STAT) & 3, 0);
        bus.write32(GIF_MODE, 1);
        assert_eq!(bus.read32(GIF_STAT) & 3, 1);
    }

    /// The memory FIFO: the scratchpad's channel fills the ring buffer,
    /// VIF1 drains it (a CNT tag and its quadword), then waits (MFIFO
    /// empty), still started, its TADR where the next tag will be.
    #[test]
    fn the_memory_fifo_drains_what_is_written() {
        let mut bus = Bus::new(&[]);
        bus.write32(D_CTRL, 1 | 2 << 2);
        bus.write32(D_RBOR, 0x10_0000);
        bus.write32(D_RBSR, 0xFF0);
        // The scratchpad channel's write pointer at the ring's start, then
        // VIF1's source chain, from the ring, started first: empty.
        bus.write32(SPR_FROM_BASE + MADR, 0x10_0000);
        bus.write32(0x1000_9000 + TADR, 0x10_0000);
        bus.write32(0x1000_9000, CHCR_STR | 1 << 2 | 1);
        assert_ne!(bus.read32(D_STAT) & 1 << 14, 0);
        // A CNT tag of one quadword (VIF NOPs), then the quadword, from the
        // scratchpad into the ring.
        bus.write128(SCRATCHPAD, 1 | 1 << 28);
        bus.write128(SCRATCHPAD + 16, 0);
        bus.write32(D_STAT, 1 << 14);
        bus.write32(SPR_FROM_BASE + 0x80, 0);
        bus.write32(SPR_FROM_BASE + QWC, 2);
        bus.write32(SPR_FROM_BASE, CHCR_STR);
        assert_eq!(bus.read32(SPR_FROM_BASE + MADR), 0x10_0020);
        assert_eq!(bus.read32(0x1000_9000 + TADR), 0x10_0020);
        assert_ne!(bus.read32(0x1000_9000) & CHCR_STR, 0);
        assert_ne!(bus.read32(D_STAT) & 1 << 14, 0);
    }

    /// A tag at the ring's end: its data and the next tag wrap to its
    /// start, however far past the end they would be.
    #[test]
    fn the_memory_fifo_wraps() {
        let mut bus = Bus::new(&[]);
        bus.write32(D_CTRL, 1 | 2 << 2);
        bus.write32(D_RBOR, 0x10_0000);
        // Four quadwords.
        bus.write32(D_RBSR, 0x30);
        bus.write32(SPR_FROM_BASE + MADR, 0x10_0030);
        bus.write32(0x1000_9000 + TADR, 0x10_0030);
        bus.write32(0x1000_9000, CHCR_STR | 1 << 2 | 1);
        // A CNT tag of two quadwords, in the last slot: its data in the
        // first two, the next tag in the third.
        bus.write128(SCRATCHPAD, 2 | 1 << 28);
        bus.write128(SCRATCHPAD + 16, 0);
        bus.write128(SCRATCHPAD + 32, 0);
        bus.write32(SPR_FROM_BASE + 0x80, 0);
        bus.write32(SPR_FROM_BASE + QWC, 3);
        bus.write32(SPR_FROM_BASE, CHCR_STR);
        assert_eq!(bus.read32(SPR_FROM_BASE + MADR), 0x10_0020);
        assert_eq!(bus.read32(0x1000_9000 + TADR), 0x10_0020);
        assert_ne!(bus.read32(0x1000_9000) & CHCR_STR, 0);
    }

    /// A mask a VIF1 DMA list leaves on does not hold an upload started
    /// after it (it went before the mask, on the console).
    #[test]
    fn a_mask_left_by_a_vif1_list_does_not_hold_later_uploads() {
        let mut bus = Bus::new(&[]);
        // VIF1's list: MSKPATH3 on, by DMA.
        bus.write128(0x2000, 0x0600_8000);
        bus.write32(0x1000_9000 + MADR, 0x2000);
        bus.write32(0x1000_9000 + QWC, 1);
        bus.write32(0x1000_9000, CHCR_STR | 1);
        assert_eq!(bus.read32(GIF_STAT) & 2, 2);
        // Then an upload: it runs.
        bus.write128(0x1000, 1 << 15);
        bus.write32(GIF_BASE + MADR, 0x1000);
        bus.write32(GIF_BASE + QWC, 1);
        bus.write32(GIF_BASE, CHCR_STR);
        assert_eq!(bus.read32(GIF_BASE) & CHCR_STR, 0);
    }

    /// Path 3 waits while masked: its DMA stays started, GIF_STAT shows
    /// its data queued in the FIFO; it runs when the mask is lifted.
    #[test]
    fn path3_waits_while_masked() {
        let mut bus = Bus::new(&[]);
        bus.write128(0x1000_5000, 0x0600_8000);
        // A normal transfer of one quadword: an empty GIF tag (EOP).
        bus.write128(0x1000, 1 << 15);
        bus.write32(GIF_BASE + MADR, 0x1000);
        bus.write32(GIF_BASE + QWC, 1);
        bus.write32(GIF_BASE, CHCR_STR);
        assert_ne!(bus.read32(GIF_BASE) & CHCR_STR, 0);
        assert_eq!(bus.read32(GIF_STAT) >> 24 & 0x1F, 1);
        bus.write128(0x1000_5000, 0x0600_0000);
        assert_eq!(bus.read32(GIF_BASE) & CHCR_STR, 0);
        assert_eq!(bus.read32(GIF_STAT) >> 24 & 0x1F, 0);
    }

    #[test]
    fn regions_and_widths() {
        let mut bus = Bus::new(&[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(bus.read32(0x1FC0_0000), 0x4433_2211);
        assert_eq!(bus.read16(0x1FC0_0002), 0x4433);
        bus.write32(0x1FC0_0000, 0);
        assert_eq!(bus.read32(0x1FC0_0000), 0x4433_2211);
        bus.write32(0x0000_1000, 0xDEAD_BEEF);
        assert_eq!(bus.read8(0x0000_1003), 0xDE);
        bus.write128(SCRATCHPAD + 0x10, 0x1234);
        assert_eq!(bus.read128(SCRATCHPAD + 0x10), 0x1234);
        assert_eq!(bus.read32(0x0000_0010), 0);
    }

    #[test]
    fn rdram_initialisation() {
        let mut bus = Bus::new(&[]);
        // INIT with SOP = 1, then each device answers once.
        bus.write32(0x1000_F430, (0x21 << 16) | (1 << 6));
        bus.write32(0x1000_F430, 0x21 << 16);
        assert_eq!(bus.read32(0x1000_F440), 0x1F);
        assert_eq!(bus.read32(0x1000_F440), 0x1F);
        assert_eq!(bus.read32(0x1000_F440), 0);
    }

    #[test]
    fn debug_port() {
        let mut bus = Bus::new(&[]);
        for b in b"# Init" {
            bus.write8(0x1000_F180, *b);
        }
        assert_eq!(bus.sio_out, b"# Init");
    }
}
