//! The SPU2, at 0x1F900000 on the IOP: two cores of 24 ADPCM voices (the
//! PlayStation's, see `texel_ps1::spu`), 2 MiB of sound memory (addressed in
//! halfwords), 48 kHz stereo output. Core 0's output goes into core 1's
//! external input; core 1's is the console's.
//!
//! Each core mixes its voices (dry and to effects, per voice: VMIX*), its
//! sound data input (MEMIN, fed by auto-DMA: streamed PCM) and its external
//! input, as MMIX says. Effects (reverb) are not emulated yet: what goes to
//! them is not heard. Transfers: the data port, manual DMA (both ways) and
//! auto-DMA. Interrupts: when a core with them enabled (ATTR bit 6) touches
//! its IRQA address (voices, transfers), IRQINFO says which, and the IOP
//! gets interrupt 9.
//!
//! STATX bit 10: a transfer is under way; bit 7: ready (the last transfer
//! finished). The libraries wait for bit 10 to drop before starting one, and
//! for bit 7 after.
//!
//! Register layout: from ps2sdk's libsd headers and PCSX2's register names.

use std::collections::VecDeque;

use texel_ps1::spu::{Phase, Sweep, decode_adpcm, envelope_tick};

/// Sound memory, in bytes.
pub const RAM_SIZE: usize = 2 * 1024 * 1024;
/// IOP cycles per output sample (36.864 MHz / 48 kHz).
const CYCLES_PER_SAMPLE: u64 = 768;
/// Output samples kept when nobody drains them (one second, stereo).
const MAX_OUTPUT: usize = 48_000 * 2;
/// Auto-DMA: samples (per channel) a core plays per block of 1024 bytes.
const ADMA_BLOCK: usize = 256;

/// Per core, from its base (core 1 is 0x400 further).
mod reg {
    pub const PMON: u32 = 0x180;
    pub const NON: u32 = 0x184;
    pub const VMIXL: u32 = 0x188;
    pub const VMIXEL: u32 = 0x18C;
    pub const VMIXR: u32 = 0x190;
    pub const VMIXER: u32 = 0x194;
    pub const MMIX: u32 = 0x198;
    pub const ATTR: u32 = 0x19A;
    pub const IRQA: u32 = 0x19C;
    pub const KON: u32 = 0x1A0;
    pub const KOFF: u32 = 0x1A4;
    pub const TSA: u32 = 0x1A8;
    pub const DATA: u32 = 0x1AC;
    pub const ADMAS: u32 = 0x1B0;
    /// Voice addresses: 0x1C0 + voice * 0xC (SSA, LSAX, NAX: high then low
    /// halfword each).
    pub const VADDR: u32 = 0x1C0;
    pub const VADDR_END: u32 = 0x2E0;
    pub const ENDX: u32 = 0x340;
    pub const STATX: u32 = 0x344;
}

/// Master volumes, from 0x760 + core * 0x28: MVOL, EVOL (effects), AVOL
/// (sound data input), BVOL (external input), then MVOLX (current).
const MASTER: u32 = 0x760;
const MASTER_STRIDE: u32 = 0x28;
/// Interrupt status: bit 2 + core.
const IRQINFO: u32 = 0x7C2;

/// ATTR bits: interrupts enabled; core enabled; the transfer mode (bits 4-5:
/// 1 data port, 2 DMA write, 3 DMA read).
const ATTR_IRQ: u16 = 1 << 6;
const ATTR_ENABLE: u16 = 1 << 15;
const ATTR_DMA_READ: u16 = 3 << 4;

/// MMIX bits: which sources go to the dry (L, R) and effects (EL, ER) sums:
/// the external input (SIN*), the sound data input (MIN*), the voices
/// (MSND*).
mod mmix {
    pub const SINER: u16 = 1 << 0;
    pub const SINEL: u16 = 1 << 1;
    pub const SINR: u16 = 1 << 2;
    pub const SINL: u16 = 1 << 3;
    pub const MINER: u16 = 1 << 4;
    pub const MINEL: u16 = 1 << 5;
    pub const MINR: u16 = 1 << 6;
    pub const MINL: u16 = 1 << 7;
    pub const MSNDER: u16 = 1 << 8;
    pub const MSNDEL: u16 = 1 << 9;
    pub const MSNDR: u16 = 1 << 10;
    pub const MSNDL: u16 = 1 << 11;
}

/// Reverb: its work area (ESA, then EEA: the high bits of its end), and the
/// addresses of its buffers in it (register pairs, in halfwords, relative
/// to the current position); its coefficients, after each core's master
/// volumes. The structure is the PlayStation's (same order of registers,
/// see `texel_ps1::spu`).
mod rv {
    pub const ESA: u32 = 0x2E0;
    pub const FB_SRC_A: u32 = 0x2E4;
    pub const FB_SRC_B: u32 = 0x2E8;
    pub const IIR_DEST_A0: u32 = 0x2EC;
    pub const IIR_DEST_A1: u32 = 0x2F0;
    pub const ACC_SRC_A0: u32 = 0x2F4;
    pub const ACC_SRC_A1: u32 = 0x2F8;
    pub const ACC_SRC_B0: u32 = 0x2FC;
    pub const ACC_SRC_B1: u32 = 0x300;
    pub const IIR_SRC_A0: u32 = 0x304;
    pub const IIR_SRC_A1: u32 = 0x308;
    pub const IIR_DEST_B0: u32 = 0x30C;
    pub const IIR_DEST_B1: u32 = 0x310;
    pub const ACC_SRC_C0: u32 = 0x314;
    pub const ACC_SRC_C1: u32 = 0x318;
    pub const ACC_SRC_D0: u32 = 0x31C;
    pub const ACC_SRC_D1: u32 = 0x320;
    pub const IIR_SRC_B1: u32 = 0x324;
    pub const IIR_SRC_B0: u32 = 0x328;
    pub const MIX_DEST_A0: u32 = 0x32C;
    pub const MIX_DEST_A1: u32 = 0x330;
    pub const MIX_DEST_B0: u32 = 0x334;
    pub const MIX_DEST_B1: u32 = 0x338;
    pub const EEA: u32 = 0x33C;
    /// Coefficients, from the core's master volumes (0x760 + core * 0x28).
    pub const IIR_ALPHA: u32 = 0x14;
    pub const ACC_COEF_A: u32 = 0x16;
    pub const ACC_COEF_B: u32 = 0x18;
    pub const ACC_COEF_C: u32 = 0x1A;
    pub const ACC_COEF_D: u32 = 0x1C;
    pub const IIR_COEF: u32 = 0x1E;
    pub const FB_ALPHA: u32 = 0x20;
    pub const FB_X: u32 = 0x22;
    pub const IN_COEF_L: u32 = 0x24;
    pub const IN_COEF_R: u32 = 0x26;
}

/// ATTR: effects (reverb) write their work area.
const ATTR_EFFECTS: u16 = 1 << 7;

const STATX_READY: u16 = 0x80;
const STATX_BUSY: u16 = 0x400;

#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
struct Voice {
    phase: Phase,
    env: i32,
    env_counter: u32,
    sweep_l: Sweep,
    sweep_r: Sweep,
    /// The current block's address, in halfwords (NAX).
    addr: u32,
    /// Loop address (LSAX), in halfwords.
    loop_addr: u32,
    counter: u32,
    /// The decoded block, after the last 3 samples of the previous one.
    buf: [i16; 31],
    hist: [i32; 2],
    flags: u8,
    /// This sample's output, for the next voice's pitch modulation.
    last: i32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Core {
    voices: [Voice; 24],
    /// Voices that reached a block with the end flag (ENDX).
    endx: u32,
    /// The transfer address (TSA's value, advancing), in halfwords.
    transfer: u32,
    statx: u16,
    /// Auto-DMA: the PCM waiting to be played (left, right).
    adma: VecDeque<(i16, i16)>,
    master_l: Sweep,
    master_r: Sweep,
    /// This sample's output.
    out: (i32, i32),
    /// Reverb: the position in its work area (from ESA), the input summed
    /// over two samples (it runs at 24 kHz), its last output.
    reverb_at: u32,
    reverb_odd: bool,
    reverb_in: (i32, i32),
    reverb_out: (i32, i32),
}

impl Default for Core {
    fn default() -> Self {
        Self {
            voices: [Voice::default(); 24],
            endx: 0,
            transfer: 0,
            statx: STATX_READY,
            adma: VecDeque::new(),
            master_l: Sweep::default(),
            master_r: Sweep::default(),
            out: (0, 0),
            reverb_at: 0,
            reverb_odd: false,
            reverb_in: (0, 0),
            reverb_out: (0, 0),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Spu2 {
    pub ram: Box<[u8]>,
    /// Registers as written (0x800 bytes).
    regs: Box<[u16]>,
    cores: [Core; 2],
    irqinfo: u16,
    /// The IOP's interrupt 9 is due.
    pub irq: bool,
    noise: u16,
    frac: u64,
    /// Output, interleaved stereo at 48 kHz.
    #[serde(skip)]
    output: Vec<i16>,
    /// Where the cores write their outputs in sound memory (see
    /// `write_outputs`). Not in save states (their format is positional):
    /// it only shifts when the next lap comes.
    #[serde(skip)]
    out_pos: u32,
}

impl std::fmt::Debug for Spu2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spu2")
            .field("irqinfo", &self.irqinfo)
            .finish_non_exhaustive()
    }
}

impl Default for Spu2 {
    fn default() -> Self {
        Self {
            ram: vec![0; RAM_SIZE].into_boxed_slice(),
            regs: vec![0; 0x800 / 2].into_boxed_slice(),
            cores: [Core::default(), Core::default()],
            irqinfo: 0,
            irq: false,
            out_pos: 0,
            noise: 1,
            frac: 0,
            output: Vec::new(),
        }
    }
}

/// Halfword addresses wrap in the 2 MiB.
const ADDR_MASK: u32 = (RAM_SIZE / 2 - 1) as u32;

/// The core and its register of an offset from 0x1F900000 (below the
/// master volumes, at 0x760).
fn split(offset: u32) -> (usize, u32) {
    if (0x400..MASTER).contains(&offset) {
        (1, offset - 0x400)
    } else {
        (0, offset)
    }
}

impl Spu2 {
    /// Diagnostics: each core's ATTR, IRQA and ENDX, and its voices that
    /// play (envelope phase and level, address, loop address).
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for core in 0..2 {
            out += &format!(
                "SPU2 core {core}: ATTR {:04x} IRQA {:05x} ENDX {:06x} STATX {:04x} ADMAS {:04x}, auto-DMA samples queued {}\n",
                self.core_reg(core, reg::ATTR),
                self.core_addr(core, reg::IRQA),
                self.cores[core].endx,
                self.cores[core].statx,
                self.core_reg(core, reg::ADMAS),
                self.cores[core].adma.len()
            );
            for (i, v) in self.cores[core].voices.iter().enumerate() {
                if v.phase != Phase::Off {
                    out += &format!(
                        "  voice {i}: {:?} env {:04x} at {:05x} loop {:05x}\n",
                        v.phase, v.env, v.addr, v.loop_addr
                    );
                }
            }
        }
        out
    }

    fn core_reg(&self, core: usize, r: u32) -> u16 {
        self.regs[((r + 0x400 * core as u32) / 2) as usize]
    }

    /// A pair of registers (high halfword first), as a 20-bit address.
    fn core_addr(&self, core: usize, r: u32) -> u32 {
        ((u32::from(self.core_reg(core, r)) << 16) | u32::from(self.core_reg(core, r + 2))) & ADDR_MASK
    }

    /// A pair of registers (bits 0-15 then 16-23): one bit per voice.
    fn core_bits(&self, core: usize, r: u32) -> u32 {
        u32::from(self.core_reg(core, r)) | (u32::from(self.core_reg(core, r + 2) & 0xFF) << 16)
    }

    fn voice_reg(&self, core: usize, voice: usize, r: u32) -> u16 {
        self.core_reg(core, voice as u32 * 0x10 + r)
    }

    fn master(&self, core: usize, r: u32) -> u16 {
        self.regs[((MASTER + MASTER_STRIDE * core as u32 + r) / 2) as usize]
    }

    pub fn read16(&mut self, offset: u32) -> u16 {
        if offset == IRQINFO {
            // Read and acknowledged.
            return std::mem::take(&mut self.irqinfo);
        }
        if offset >= MASTER {
            let base = offset - MASTER;
            let (core, r) = ((base / MASTER_STRIDE) as usize, base % MASTER_STRIDE);
            if core < 2 && (r == 0x10 || r == 0x12) {
                let c = &self.cores[core];
                let level = if r == 0x10 { c.master_l } else { c.master_r };
                return (level.level() >> 1) as u16;
            }
            return self.regs[(offset as usize / 2) % self.regs.len()];
        }
        let (core, r) = split(offset);
        let c = &self.cores[core];
        match r {
            reg::STATX => c.statx,
            reg::ENDX => c.endx as u16,
            r if r == reg::ENDX + 2 => (c.endx >> 16) as u16,
            reg::ADMAS => self.core_reg(core, r),
            r if r < 0x180 && (r & 0xF == 0xA) => c.voices[(r / 0x10) as usize].env as u16,
            r if r < 0x180 && (r & 0xF == 0xC) => (c.voices[(r / 0x10) as usize].sweep_l.level() >> 1) as u16,
            r if r < 0x180 && (r & 0xF == 0xE) => (c.voices[(r / 0x10) as usize].sweep_r.level() >> 1) as u16,
            r if (reg::VADDR..reg::VADDR_END).contains(&r) => {
                let (voice, k) = (((r - reg::VADDR) / 0xC) as usize, (r - reg::VADDR) % 0xC);
                let v = &c.voices[voice];
                match k {
                    4 => (v.loop_addr >> 16) as u16,
                    6 => v.loop_addr as u16,
                    8 => (v.addr >> 16) as u16,
                    10 => v.addr as u16,
                    _ => self.core_reg(core, r),
                }
            }
            reg::DATA => self.port_read(core),
            _ => self.regs[(offset as usize / 2) % self.regs.len()],
        }
    }

    pub fn write16(&mut self, offset: u32, v: u16) {
        let old = self.regs[(offset as usize / 2) % self.regs.len()];
        self.regs[(offset as usize / 2) % self.regs.len()] = v;
        if offset >= MASTER {
            return;
        }
        let (core, r) = split(offset);
        match r {
            reg::ATTR => {
                let (old_mode, mode) = ((old >> 4) & 3, (v >> 4) & 3);
                let statx = &mut self.cores[core].statx;
                // As PCSX2 has it.
                if mode == 0 && *statx & STATX_BUSY == 0 {
                    *statx &= !STATX_READY;
                } else if old_mode == 0 && mode != 0 {
                    *statx |= STATX_READY;
                }
            }
            r if r == reg::TSA || r == reg::TSA + 2 => {
                self.cores[core].transfer = self.core_addr(core, reg::TSA);
            }
            reg::DATA => self.port_write(core, v),
            r if r == reg::KON || r == reg::KON + 2 => {
                let bits = if r == reg::KON {
                    u32::from(v)
                } else {
                    u32::from(v & 0xFF) << 16
                };
                for voice in (0..24).filter(|&i| bits & (1 << i) != 0) {
                    self.key_on(core, voice);
                }
            }
            r if r == reg::KOFF || r == reg::KOFF + 2 => {
                let bits = if r == reg::KOFF {
                    u32::from(v)
                } else {
                    u32::from(v & 0xFF) << 16
                };
                for voice in (0..24).filter(|&i| bits & (1 << i) != 0) {
                    let vo = &mut self.cores[core].voices[voice];
                    if vo.phase != Phase::Off {
                        vo.phase = Phase::Release;
                        vo.env_counter = 0;
                    }
                }
            }
            // Writing ENDX clears it.
            r if r == reg::ENDX || r == reg::ENDX + 2 => self.cores[core].endx = 0,
            r if (reg::VADDR..reg::VADDR_END).contains(&r) => {
                let (voice, k) = (((r - reg::VADDR) / 0xC) as usize, (r - reg::VADDR) % 0xC);
                let base = reg::VADDR + voice as u32 * 0xC;
                let value = |s: &Self, at: u32| s.core_addr(core, base + at);
                match k {
                    4 | 6 => self.cores[core].voices[voice].loop_addr = value(self, 4),
                    8 | 10 => self.cores[core].voices[voice].addr = value(self, 8),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn key_on(&mut self, core: usize, voice: usize) {
        let start = self.core_addr(core, reg::VADDR + voice as u32 * 0xC);
        let v = &mut self.cores[core].voices[voice];
        v.phase = Phase::Attack;
        v.env = 0;
        v.env_counter = 0;
        v.addr = start;
        v.counter = 0;
        v.hist = [0; 2];
        v.buf = [0; 31];
        self.cores[core].endx &= !(1 << voice);
        self.decode_block(core, voice);
    }

    /// A transfer touched `len` halfwords from `addr`: the interrupt, if a
    /// core waits for one of them.
    fn check_irq(&mut self, addr: u32, len: u32) {
        for core in 0..2 {
            if self.core_reg(core, reg::ATTR) & ATTR_IRQ == 0 {
                continue;
            }
            let at = self.core_addr(core, reg::IRQA);
            if at.wrapping_sub(addr) & ADDR_MASK < len {
                self.irqinfo |= 4 << core;
                self.irq = true;
            }
        }
    }

    fn port_write(&mut self, core: usize, v: u16) {
        let at = self.cores[core].transfer;
        self.ram[2 * at as usize..2 * at as usize + 2].copy_from_slice(&v.to_le_bytes());
        self.check_irq(at, 1);
        self.cores[core].transfer = (at + 1) & ADDR_MASK;
    }

    fn port_read(&mut self, core: usize) -> u16 {
        let at = self.cores[core].transfer;
        let v = u16::from_le_bytes([self.ram[2 * at as usize], self.ram[2 * at as usize + 1]]);
        self.check_irq(at, 1);
        self.cores[core].transfer = (at + 1) & ADDR_MASK;
        v
    }

    /// Whether `core`'s DMA goes to its sound data input (auto-DMA).
    pub fn auto_dma(&self, core: usize) -> bool {
        self.core_reg(core, reg::ADMAS) & (1 << core) != 0
    }

    /// Whether `core`'s DMA reads sound memory.
    pub fn dma_reads(&self, core: usize) -> bool {
        self.core_reg(core, reg::ATTR) & ATTR_DMA_READ == ATTR_DMA_READ
    }

    /// A DMA transfer to or from `core` started.
    pub fn dma_started(&mut self, core: usize) {
        self.cores[core].statx = (self.cores[core].statx & !STATX_READY) | STATX_BUSY;
    }

    /// It finished.
    pub fn dma_done(&mut self, core: usize) {
        self.cores[core].statx = (self.cores[core].statx & !STATX_BUSY) | STATX_READY;
    }

    /// Manual DMA: `data` into sound memory at the transfer address.
    pub fn dma_write(&mut self, core: usize, data: &[u8]) {
        let halves = data.len() as u32 / 2;
        let start = self.cores[core].transfer;
        for (k, h) in data.as_chunks::<2>().0.iter().enumerate() {
            let at = (start + k as u32) & ADDR_MASK;
            self.ram[2 * at as usize..2 * at as usize + 2].copy_from_slice(h);
        }
        self.check_irq(start, halves);
        self.cores[core].transfer = (start + halves) & ADDR_MASK;
    }

    /// Manual DMA: `len` bytes from sound memory at the transfer address.
    pub fn dma_read(&mut self, core: usize, len: usize) -> Vec<u8> {
        let start = self.cores[core].transfer;
        let halves = len as u32 / 2;
        let out = (0..halves)
            .flat_map(|k| {
                let at = 2 * ((start + k) & ADDR_MASK) as usize;
                [self.ram[at], self.ram[at + 1]]
            })
            .collect();
        self.check_irq(start, halves);
        self.cores[core].transfer = (start + halves) & ADDR_MASK;
        out
    }

    /// Auto-DMA: blocks of 1024 bytes (256 left samples, then 256 right),
    /// queued for the core's sound data input.
    pub fn adma_write(&mut self, core: usize, data: &[u8]) {
        let samples = |b: &[u8]| -> Vec<i16> { b.as_chunks::<2>().0.iter().map(|h| i16::from_le_bytes(*h)).collect() };
        for block in data.chunks(4 * ADMA_BLOCK) {
            let (l, r) = block.split_at(block.len() / 2);
            let (l, r) = (samples(l), samples(r));
            self.cores[core].adma.extend(l.into_iter().zip(r));
        }
    }

    /// Auto-DMA: whether the core has room for another block (its transfer
    /// completes once it has played what it had but a block).
    pub fn adma_wants(&self, core: usize) -> bool {
        self.cores[core].adma.len() <= ADMA_BLOCK
    }

    /// Decodes the block at the voice's address.
    fn decode_block(&mut self, core: usize, voice: usize) {
        let addr = self.cores[core].voices[voice].addr & !7 & ADDR_MASK;
        self.check_irq(addr, 8);
        let at = 2 * addr as usize;
        let block: [u8; 16] = self.ram[at..at + 16].try_into().expect("16 bytes");
        let v = &mut self.cores[core].voices[voice];
        v.flags = block[1];
        if block[1] & 4 != 0 {
            v.loop_addr = addr;
        }
        let tail = [v.buf[28], v.buf[29], v.buf[30]];
        v.buf[..3].copy_from_slice(&tail);
        decode_adpcm(&block, &mut v.hist, &mut v.buf[3..]);
    }

    /// The reverb work area: start and end (halfwords, end included).
    fn reverb_area(&self, core: usize) -> (u32, u32) {
        let start = self.core_addr(core, rv::ESA);
        let end = ((u32::from(self.core_reg(core, rv::EEA)) << 16) | 0xFFFF) & ADDR_MASK;
        (start, end.max(start))
    }

    /// The address of buffer register `r` (plus `delta`) from the reverb's
    /// position, wrapped in its work area.
    fn rv_addr(&self, core: usize, r: u32, delta: i32) -> usize {
        let (start, end) = self.reverb_area(core);
        let size = i64::from(end - start + 1);
        let off = i64::from(self.cores[core].reverb_at) + i64::from(self.core_addr(core, r)) + i64::from(delta);
        2 * (start + off.rem_euclid(size) as u32) as usize
    }

    fn rv_load(&self, core: usize, r: u32, delta: i32) -> i32 {
        let a = self.rv_addr(core, r, delta);
        i32::from(i16::from_le_bytes([self.ram[a], self.ram[a + 1]]))
    }

    fn rv_store(&mut self, core: usize, r: u32, v: i32) {
        let a = self.rv_addr(core, r, 0);
        self.ram[a..a + 2].copy_from_slice(&(v.clamp(-0x8000, 0x7FFF) as i16).to_le_bytes());
    }

    fn rv_coef(&self, core: usize, r: u32) -> i32 {
        i32::from(self.master(core, r) as i16)
    }

    /// One reverb step of a core (at 24 kHz: every other sample): takes
    /// this sample's input, returns its output.
    fn reverb(&mut self, core: usize, in_l: i32, in_r: i32) -> (i32, i32) {
        let c = &mut self.cores[core];
        c.reverb_in.0 += in_l;
        c.reverb_in.1 += in_r;
        c.reverb_odd = !c.reverb_odd;
        if c.reverb_odd {
            return c.reverb_out;
        }
        let (in_l, in_r) = (c.reverb_in.0 / 2, c.reverb_in.1 / 2);
        c.reverb_in = (0, 0);
        let mul = |a: i32, b: i32| ((a * b) >> 15).clamp(-0x8000, 0x7FFF);
        let lin = mul(in_l, self.rv_coef(core, rv::IN_COEF_L));
        let rin = mul(in_r, self.rv_coef(core, rv::IN_COEF_R));
        let (v_iir, v_wall) = (self.rv_coef(core, rv::IIR_ALPHA), self.rv_coef(core, rv::IIR_COEF));
        let (v_apf1, v_apf2) = (self.rv_coef(core, rv::FB_ALPHA), self.rv_coef(core, rv::FB_X));
        let write = self.core_reg(core, reg::ATTR) & ATTR_EFFECTS != 0;
        // Same-side and cross reflections.
        let reflect = |s: &Self, input: i32, m: u32, d: u32| -> i32 {
            let prev = s.rv_load(core, m, -1);
            mul(input + mul(s.rv_load(core, d, 0), v_wall) - prev, v_iir) + prev
        };
        let l_same = reflect(self, lin, rv::IIR_DEST_A0, rv::IIR_SRC_A0);
        let r_same = reflect(self, rin, rv::IIR_DEST_A1, rv::IIR_SRC_A1);
        let l_diff = reflect(self, lin, rv::IIR_DEST_B0, rv::IIR_SRC_B0);
        let r_diff = reflect(self, rin, rv::IIR_DEST_B1, rv::IIR_SRC_B1);
        if write {
            self.rv_store(core, rv::IIR_DEST_A0, l_same);
            self.rv_store(core, rv::IIR_DEST_A1, r_same);
            self.rv_store(core, rv::IIR_DEST_B0, l_diff);
            self.rv_store(core, rv::IIR_DEST_B1, r_diff);
        }
        let comb = |s: &Self, srcs: [u32; 4]| -> i32 {
            let coefs = [rv::ACC_COEF_A, rv::ACC_COEF_B, rv::ACC_COEF_C, rv::ACC_COEF_D];
            srcs.iter()
                .zip(coefs)
                .map(|(&src, coef)| mul(s.rv_load(core, src, 0), s.rv_coef(core, coef)))
                .sum::<i32>()
                .clamp(-0x8000, 0x7FFF)
        };
        let mut out = [
            comb(self, [rv::ACC_SRC_A0, rv::ACC_SRC_B0, rv::ACC_SRC_C0, rv::ACC_SRC_D0]),
            comb(self, [rv::ACC_SRC_A1, rv::ACC_SRC_B1, rv::ACC_SRC_C1, rv::ACC_SRC_D1]),
        ];
        let apfs = [
            [
                (rv::MIX_DEST_A0, rv::FB_SRC_A, v_apf1),
                (rv::MIX_DEST_B0, rv::FB_SRC_B, v_apf2),
            ],
            [
                (rv::MIX_DEST_A1, rv::FB_SRC_A, v_apf1),
                (rv::MIX_DEST_B1, rv::FB_SRC_B, v_apf2),
            ],
        ];
        for (o, side) in out.iter_mut().zip(apfs) {
            for (m, d, v) in side {
                let delay = self.core_addr(core, d) as i32;
                let delayed = self.rv_load(core, m, -delay);
                let x = (*o - mul(v, delayed)).clamp(-0x8000, 0x7FFF);
                if write {
                    self.rv_store(core, m, x);
                }
                *o = (mul(x, v) + delayed).clamp(-0x8000, 0x7FFF);
            }
        }
        let (start, end) = self.reverb_area(core);
        let c = &mut self.cores[core];
        c.reverb_out = (out[0], out[1]);
        c.reverb_at = (c.reverb_at + 1) % (end - start + 1);
        c.reverb_out
    }

    /// Advances by `cycles` IOP cycles; returns whether interrupt 9 is due.
    pub fn tick(&mut self, cycles: u64) -> bool {
        self.frac += cycles;
        while self.frac >= CYCLES_PER_SAMPLE {
            self.frac -= CYCLES_PER_SAMPLE;
            self.sample();
        }
        std::mem::take(&mut self.irq)
    }

    /// The output since the last call, interleaved stereo at 48 kHz.
    pub fn drain_output(&mut self, out: &mut Vec<i16>) {
        out.append(&mut self.output);
    }

    fn sample(&mut self) {
        let bit = ((self.noise >> 15) ^ (self.noise >> 12) ^ (self.noise >> 11) ^ (self.noise >> 10) ^ 1) & 1;
        self.noise = (self.noise << 1) | bit;
        for core in 0..2 {
            self.mix_core(core);
        }
        self.write_outputs();
        let (l, r) = self.cores[1].out;
        if self.output.len() + 2 > MAX_OUTPUT {
            self.output.drain(..2);
        }
        self.output.push(l.clamp(-0x8000, 0x7FFF) as i16);
        self.output.push(r.clamp(-0x8000, 0x7FFF) as i16);
    }

    /// Each core writes its outputs to sound memory, a halfword a sample in
    /// areas of 512 that it goes round: its dry left and right, then voices
    /// 1 and 3 (core 0 from 0x400, core 1 from 0xC00). The writes reach
    /// IRQA as any access does: games aim it there for a regular interrupt
    /// (RenderWare Audio's tick, at 0x400).
    fn write_outputs(&mut self) {
        let pos = self.out_pos;
        for core in 0..2 {
            let base = if core == 0 { 0x400 } else { 0xC00 };
            let c = &self.cores[core];
            let values = [c.out.0, c.out.1, c.voices[1].last, c.voices[3].last];
            for (k, v) in values.into_iter().enumerate() {
                let at = base + 0x200 * k as u32 + pos;
                let v = v.clamp(-0x8000, 0x7FFF) as i16;
                self.ram[2 * at as usize..2 * at as usize + 2].copy_from_slice(&v.to_le_bytes());
                self.check_irq(at, 1);
            }
        }
        self.out_pos = (pos + 1) & 0x1FF;
    }

    /// One sample of a core: its voices, inputs and master volume.
    fn mix_core(&mut self, core: usize) {
        let enabled = self.core_reg(core, reg::ATTR) & ATTR_ENABLE != 0;
        let (pmon, non) = (self.core_bits(core, reg::PMON), self.core_bits(core, reg::NON));
        let (vl, vr) = (self.core_bits(core, reg::VMIXL), self.core_bits(core, reg::VMIXR));
        let (vel, ver) = (self.core_bits(core, reg::VMIXEL), self.core_bits(core, reg::VMIXER));
        let (mut dry_l, mut dry_r) = (0i32, 0i32);
        let (mut wet_l, mut wet_r) = (0i32, 0i32);
        let mut prev = 0i32;
        for i in 0..24 {
            if self.cores[core].voices[i].phase == Phase::Off {
                self.cores[core].voices[i].last = 0;
                prev = 0;
                continue;
            }
            let regs = [0x0, 0x2, 0x4, 0x6, 0x8].map(|r| self.voice_reg(core, i, r));
            let [vol_l, vol_r, pitch, adsr1, adsr2] = regs;
            let v = &mut self.cores[core].voices[i];
            let idx = (v.counter >> 12) as usize;
            let frac = (v.counter & 0xFFF) as i32;
            let (a, b) = (i32::from(v.buf[idx + 2]), i32::from(v.buf[idx + 3]));
            let raw = if non & (1 << i) != 0 {
                i32::from(self.noise as i16)
            } else {
                a + (((b - a) * frac) >> 12)
            };
            let s = (raw * v.env) >> 15;
            let l = (s * v.sweep_l.step(vol_l)) >> 15;
            let r = (s * v.sweep_r.step(vol_r)) >> 15;
            if vl & (1 << i) != 0 {
                dry_l += l;
            }
            if vr & (1 << i) != 0 {
                dry_r += r;
            }
            if vel & (1 << i) != 0 {
                wet_l += l;
            }
            if ver & (1 << i) != 0 {
                wet_r += r;
            }
            let mut step = u32::from(pitch.min(0x3FFF));
            if i > 0 && pmon & (1 << i) != 0 {
                step = ((step as i32 * (prev + 0x8000)) >> 15).clamp(0, 0x3FFF) as u32;
            }
            v.last = s;
            prev = s;
            envelope_tick(&mut v.phase, &mut v.env, &mut v.env_counter, adsr1, adsr2);
            v.counter += step;
            while self.cores[core].voices[i].counter >= 28 << 12 {
                let v = &mut self.cores[core].voices[i];
                v.counter -= 28 << 12;
                if v.flags & 1 != 0 {
                    v.addr = v.loop_addr;
                    if v.flags & 2 == 0 {
                        v.phase = Phase::Off;
                        v.env = 0;
                    }
                    self.cores[core].endx |= 1 << i;
                } else {
                    v.addr = (v.addr + 8) & ADDR_MASK;
                }
                self.decode_block(core, i);
            }
        }
        let mmix = self.core_reg(core, reg::MMIX);
        // EVOL, AVOL, BVOL: signed, -0x8000 to 0x7FFF.
        let vol = |v: i32, reg: u16| (v * i32::from(reg as i16)) >> 15;
        // Dry (l, r) and to effects (el, er).
        let (mut l, mut r, mut el, mut er) = (0i32, 0i32, 0i32, 0i32);
        let on = |bit: u16| mmix & bit != 0;
        if on(mmix::MSNDL) {
            l += dry_l;
        }
        if on(mmix::MSNDR) {
            r += dry_r;
        }
        if on(mmix::MSNDEL) {
            el += wet_l;
        }
        if on(mmix::MSNDER) {
            er += wet_r;
        }
        // Sound data input (auto-DMA), at AVOL.
        let (in_l, in_r) = self.cores[core].adma.pop_front().unwrap_or((0, 0));
        let (in_l, in_r) = (
            vol(i32::from(in_l), self.master(core, 0x8)),
            vol(i32::from(in_r), self.master(core, 0xA)),
        );
        if on(mmix::MINL) {
            l += in_l;
        }
        if on(mmix::MINR) {
            r += in_r;
        }
        if on(mmix::MINEL) {
            el += in_l;
        }
        if on(mmix::MINER) {
            er += in_r;
        }
        // External input (core 0's output, for core 1), at BVOL.
        if core == 1 {
            let (ext_l, ext_r) = self.cores[0].out;
            let (ext_l, ext_r) = (vol(ext_l, self.master(core, 0xC)), vol(ext_r, self.master(core, 0xE)));
            if on(mmix::SINL) {
                l += ext_l;
            }
            if on(mmix::SINR) {
                r += ext_r;
            }
            if on(mmix::SINEL) {
                el += ext_l;
            }
            if on(mmix::SINER) {
                er += ext_r;
            }
        }
        // Effects (reverb), back at EVOL.
        let (fx_l, fx_r) = self.reverb(core, el, er);
        l += vol(fx_l, self.master(core, 0x4));
        r += vol(fx_r, self.master(core, 0x6));
        let (ml, mr) = (self.master(core, 0x0), self.master(core, 0x2));
        let c = &mut self.cores[core];
        let (l, r) = (l.clamp(-0x8000, 0x7FFF), r.clamp(-0x8000, 0x7FFF));
        c.out = if enabled {
            ((l * c.master_l.step(ml)) >> 15, (r * c.master_r.step(mr)) >> 15)
        } else {
            (0, 0)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cores write their outputs round areas of 512 halfwords: IRQA
    /// there interrupts once a lap.
    #[test]
    fn outputs_written_to_memory_reach_irqa() {
        let mut s = Spu2::default();
        // Core 0: interrupts on, IRQA at 0x405 (its dry left, sample 5).
        s.write16(reg::ATTR, ATTR_ENABLE | ATTR_IRQ);
        s.write16(reg::IRQA, 0);
        s.write16(reg::IRQA + 2, 0x405);
        assert!(!s.tick(5 * CYCLES_PER_SAMPLE));
        assert!(s.tick(CYCLES_PER_SAMPLE));
        assert_eq!(s.read16(IRQINFO), 4);
        // Not again before the next lap.
        assert!(!s.tick(511 * CYCLES_PER_SAMPLE));
        assert!(s.tick(CYCLES_PER_SAMPLE));
    }

    #[test]
    fn statx_follows_dma() {
        let mut s = Spu2::default();
        assert_eq!(s.read16(0x744), STATX_READY);
        s.write16(0x400 + reg::ATTR, 0x20);
        s.dma_started(1);
        assert_eq!(s.read16(0x744), STATX_BUSY);
        s.dma_done(1);
        assert_eq!(s.read16(0x744), STATX_READY);
        s.write16(0x40, 0x1234);
        assert_eq!(s.read16(0x40), 0x1234);
    }

    /// A voice of core 1 playing a looped block of a constant, at full
    /// volumes: sound comes out.
    #[test]
    fn a_voice_sounds() {
        let mut s = Spu2::default();
        // At halfword 0x5000: shift 0, filter 0, nibbles 7 (7 << 12),
        // looping on itself (flags: start, repeat, end).
        let at = 0x5000;
        s.write16(0x400 + reg::TSA, 0);
        s.write16(0x400 + reg::TSA + 2, at as u16);
        s.write16(0x400 + reg::DATA, 0x0700);
        for _ in 0..7 {
            s.write16(0x400 + reg::DATA, 0x7777);
        }
        // Block flags in byte 1: loop start (4), repeat (2), end (1).
        s.ram[2 * at + 1] = 7;
        s.write16(0x400 + reg::ATTR, ATTR_ENABLE);
        s.write16(0x400 + reg::MMIX, mmix::MSNDL | mmix::MSNDR);
        s.write16(0x400 + reg::VMIXL, 1);
        s.write16(0x400 + reg::VMIXR, 1);
        s.write16(MASTER + MASTER_STRIDE, 0x3FFF);
        s.write16(MASTER + MASTER_STRIDE + 2, 0x3FFF);
        s.write16(0x400, 0x3FFF);
        s.write16(0x402, 0x3FFF);
        s.write16(0x404, 0x1000);
        // Fastest attack, sustain at the top.
        s.write16(0x406, 0x000F);
        s.write16(0x408, 0x0000);
        s.write16(0x400 + reg::VADDR, 0);
        s.write16(0x400 + reg::VADDR + 2, at as u16);
        s.write16(0x400 + reg::KON, 1);
        s.tick(CYCLES_PER_SAMPLE * 200);
        let mut out = Vec::new();
        s.drain_output(&mut out);
        assert_eq!(out.len(), 400);
        assert!(out[300..].iter().all(|&v| v > 0x2000));
        // The loop flag set ENDX.
        assert_eq!(s.read16(0x400 + reg::ENDX), 1);
    }

    #[test]
    fn transfers_raise_the_interrupt() {
        let mut s = Spu2::default();
        s.write16(reg::IRQA, 0);
        s.write16(reg::IRQA + 2, 0x100);
        s.write16(reg::ATTR, ATTR_IRQ);
        s.write16(reg::TSA, 0);
        s.write16(reg::TSA + 2, 0xF0);
        s.dma_write(0, &[0; 64]);
        assert!(s.irq);
        assert_eq!(s.read16(IRQINFO), 4);
        assert_eq!(s.read16(IRQINFO), 0);
    }
}
