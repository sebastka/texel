//! Sound Processing Unit: 24 ADPCM voices with ADSR envelopes and volume
//! sweeps, CD audio input, reverb, and 44.1 kHz stereo output.

use std::collections::VecDeque;

pub const SPU_RAM_SIZE: usize = 512 * 1024;
const CYCLES_PER_SAMPLE: u32 = 768;
/// Output samples kept when nobody drains them (one second, stereo).
const MAX_OUTPUT: usize = 44_100 * 2;

const FILTER_POS: [i32; 5] = [0, 60, 115, 98, 122];
const FILTER_NEG: [i32; 5] = [0, 0, -52, -55, -60];

/// An envelope's phase.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    Attack,
    Decay,
    Sustain,
    Release,
    #[default]
    Off,
}

#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct Voice {
    vol_l: u16,
    vol_r: u16,
    pitch: u16,
    start: u16,
    adsr_lo: u16,
    adsr_hi: u16,
    repeat: u16,
    /// Current block address, in bytes.
    addr: u32,
    counter: u32,
    /// Decoded block, preceded by the last 3 samples of the previous one.
    buf: [i16; 31],
    hist: [i32; 2],
    block_flags: u8,
    phase: Phase,
    env: i32,
    env_counter: u32,
    /// Last output sample, for pitch modulation of the next voice.
    last: i32,
    sweep_l: Sweep,
    sweep_r: Sweep,
}

/// A volume register: fixed, or sweeping up or down over time.
#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct Sweep {
    level: i32,
    counter: u32,
}

impl Sweep {
    /// The current volume (-0x8000..0x7fff) for register value `reg`,
    /// advanced by one sample.
    /// The current volume.
    pub fn level(&self) -> i32 {
        self.level
    }

    pub fn step(&mut self, reg: u16) -> i32 {
        if reg & 0x8000 == 0 {
            self.level = i32::from((reg << 1) as i16);
            return self.level;
        }
        // Sweep mode: bit 14 exponential, bit 13 decrease, bit 12 negative
        // phase, bits 0-6 rate.
        let exponential = reg & 0x4000 != 0;
        let decrease = reg & 0x2000 != 0;
        let rate = u32::from(reg & 0x7f);
        let shift = rate >> 2;
        let mut step: i32 = if decrease {
            -8 + (rate & 3) as i32
        } else {
            7 - (rate & 3) as i32
        };
        let mut cycles = 1u32 << shift.saturating_sub(11);
        step <<= 11u32.saturating_sub(shift);
        let magnitude = self.level.abs();
        if exponential && !decrease && magnitude > 0x6000 {
            cycles *= 4;
        }
        if exponential && decrease {
            step = (step * magnitude) >> 15;
        }
        self.counter += 1;
        if self.counter >= cycles {
            self.counter = 0;
            let next = (magnitude + step).clamp(0, 0x7fff);
            self.level = if reg & 0x1000 != 0 { -next } else { next };
        }
        self.level
    }
}

/// Reverb registers, as offsets from 0x1F801C00.
mod rv {
    pub const OUT_L: u32 = 0x184;
    pub const OUT_R: u32 = 0x186;
    pub const BASE: u32 = 0x1a2;
    pub const D_APF1: u32 = 0x1c0;
    pub const D_APF2: u32 = 0x1c2;
    pub const V_IIR: u32 = 0x1c4;
    pub const V_COMB1: u32 = 0x1c6;
    pub const V_COMB2: u32 = 0x1c8;
    pub const V_COMB3: u32 = 0x1ca;
    pub const V_COMB4: u32 = 0x1cc;
    pub const V_WALL: u32 = 0x1ce;
    pub const V_APF1: u32 = 0x1d0;
    pub const V_APF2: u32 = 0x1d2;
    pub const M_SAME: u32 = 0x1d4;
    pub const M_COMB1: u32 = 0x1d8;
    pub const M_COMB2: u32 = 0x1dc;
    pub const D_SAME: u32 = 0x1e0;
    pub const M_DIFF: u32 = 0x1e4;
    pub const M_COMB3: u32 = 0x1e8;
    pub const M_COMB4: u32 = 0x1ec;
    pub const D_DIFF: u32 = 0x1f0;
    pub const M_APF1: u32 = 0x1f4;
    pub const M_APF2: u32 = 0x1f8;
    pub const V_IN_L: u32 = 0x1fc;
    pub const V_IN_R: u32 = 0x1fe;
}

/// One envelope step size and rate.
struct EnvStep {
    exponential: bool,
    decrease: bool,
    shift: u32,
    step: i32,
}

/// The rate of the envelope's current phase, from the ADSR registers.
fn env_params(phase: Phase, lo: u16, hi: u16) -> Option<EnvStep> {
    Some(match phase {
        Phase::Attack => EnvStep {
            exponential: lo & 0x8000 != 0,
            decrease: false,
            shift: u32::from((lo >> 10) & 0x1f),
            step: 7 - i32::from((lo >> 8) & 3),
        },
        Phase::Decay => EnvStep {
            exponential: true,
            decrease: true,
            shift: u32::from((lo >> 4) & 0xf),
            step: -8,
        },
        Phase::Sustain => {
            let decrease = hi & 0x4000 != 0;
            let raw = i32::from((hi >> 6) & 3);
            EnvStep {
                exponential: hi & 0x8000 != 0,
                decrease,
                shift: u32::from((hi >> 8) & 0x1f),
                step: if decrease { -8 + raw } else { 7 - raw },
            }
        }
        Phase::Release => EnvStep {
            exponential: hi & 0x20 != 0,
            decrease: true,
            shift: u32::from(hi & 0x1f),
            step: -8,
        },
        Phase::Off => return None,
    })
}

/// Advances an ADSR envelope (level `env`, 0..0x7fff) by one sample, with
/// ADSR registers `lo` and `hi`: the PlayStation's and the SPU2's voices.
pub fn envelope_tick(phase: &mut Phase, env: &mut i32, counter: &mut u32, lo: u16, hi: u16) {
    let Some(p) = env_params(*phase, lo, hi) else { return };
    let mut cycles = 1u32 << p.shift.saturating_sub(11);
    let mut step = p.step << 11u32.saturating_sub(p.shift);
    if p.exponential && !p.decrease && *env > 0x6000 {
        cycles *= 4;
    }
    if p.exponential && p.decrease {
        step = (step * *env) >> 15;
    }
    *counter += 1;
    if *counter < cycles {
        return;
    }
    *counter = 0;
    *env = (*env + step).clamp(0, 0x7fff);
    let sustain = (i32::from(lo & 0xf) + 1) * 0x800;
    match *phase {
        Phase::Attack if *env >= 0x7fff => *phase = Phase::Decay,
        Phase::Decay if *env <= sustain => *phase = Phase::Sustain,
        Phase::Release if *env == 0 => *phase = Phase::Off,
        _ => {}
    }
}

/// Decodes a 16-byte ADPCM block (header, flags, 28 samples) into `out`,
/// with the last two samples decoded in `hist`.
pub fn decode_adpcm(block: &[u8; 16], hist: &mut [i32; 2], out: &mut [i16]) {
    let shift = match block[0] & 0xf {
        s @ 0..=12 => s,
        _ => 9,
    };
    let filter = usize::from((block[0] >> 4) & 7).min(4);
    for (n, o) in out.iter_mut().enumerate().take(28) {
        let nibble = (block[2 + n / 2] >> ((n & 1) * 4)) & 0xf;
        let t = i32::from((u16::from(nibble) << 12) as i16) >> shift;
        let s = t + ((hist[0] * FILTER_POS[filter] + hist[1] * FILTER_NEG[filter] + 32) >> 6);
        let s = s.clamp(-0x8000, 0x7fff);
        *hist = [s, hist[0]];
        *o = s as i16;
    }
}

impl Voice {
    fn tick_envelope(&mut self) {
        envelope_tick(
            &mut self.phase,
            &mut self.env,
            &mut self.env_counter,
            self.adsr_lo,
            self.adsr_hi,
        );
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Spu {
    pub ram: Box<[u8]>,
    #[serde(with = "serde_big_array::BigArray")]
    regs: [u16; 0x200],
    voices: [Voice; 24],
    control: u16,
    irq_addr: u32,
    transfer_addr: u32,
    irq_flag: bool,
    endx: u32,
    pmon: u32,
    noise_on: u32,
    noise: u16,
    frac: u32,
    pending_irq: bool,
    main_l: Sweep,
    main_r: Sweep,
    /// Reverb work area position (bytes), input accumulated over two
    /// samples, and the last output: reverb runs at 22.05 kHz.
    reverb_addr: u32,
    reverb_in: (i32, i32),
    reverb_out: (i32, i32),
    reverb_odd: bool,
    #[serde(skip)]
    output: Vec<i16>,
    cd: VecDeque<(i16, i16)>,
    /// Where the next samples go in the capture buffers (in samples, of
    /// 512): CD audio left and right, voices 1 and 3, written to RAM at
    /// 0x000, 0x400, 0x800 and 0xC00 as they play (games read them, and
    /// set the IRQ address there: CTR fades its music on that interrupt).
    /// Saved apart (`crate::Timing`).
    #[serde(skip)]
    pub(crate) capture: u32,
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

impl Spu {
    pub fn new() -> Self {
        Self {
            ram: vec![0; SPU_RAM_SIZE].into_boxed_slice(),
            regs: [0; 0x200],
            voices: [Voice::default(); 24],
            control: 0,
            irq_addr: 0,
            transfer_addr: 0,
            irq_flag: false,
            endx: 0,
            pmon: 0,
            noise_on: 0,
            noise: 1,
            frac: 0,
            pending_irq: false,
            main_l: Sweep::default(),
            main_r: Sweep::default(),
            reverb_addr: 0,
            reverb_in: (0, 0),
            reverb_out: (0, 0),
            reverb_odd: false,
            output: Vec::with_capacity(MAX_OUTPUT),
            cd: VecDeque::with_capacity(44_100),
            capture: 0,
        }
    }

    /// Takes the stereo samples produced since the last call.
    pub fn drain_output(&mut self, out: &mut Vec<i16>) {
        out.append(&mut self.output);
    }

    pub fn push_cd_audio(&mut self, l: i16, r: i16) {
        if self.cd.len() < 44_100 {
            self.cd.push_back((l, r));
        }
    }

    fn check_irq(&mut self, addr: u32, len: u32) {
        if self.control & 0x40 != 0 && !self.irq_flag && addr <= self.irq_addr && self.irq_addr < addr + len {
            self.irq_flag = true;
            self.pending_irq = true;
        }
    }

    pub fn read16(&self, off: u32) -> u16 {
        let off = off - 0xc00;
        match off {
            0x000..=0x17f if off & 0xf == 0xc => self.voices[(off >> 4) as usize].env as u16,
            0x19c => self.endx as u16,
            0x19e => (self.endx >> 16) as u16,
            0x1a6 => (self.transfer_addr / 8) as u16,
            0x1aa => self.control,
            0x1ae => {
                (self.control & 0x3f)
                    | (u16::from(self.irq_flag) << 6)
                    | (((self.control >> 5) & 1) << 7)
                    // Writing the capture buffers' second half.
                    | (u16::from(self.capture >= 0x100) << 11)
            }
            0x1b8 => self.main_l.level as u16,
            0x1ba => self.main_r.level as u16,
            0x200..=0x25f => {
                let v = &self.voices[((off - 0x200) >> 2) as usize];
                let level = if off & 2 == 0 { v.sweep_l.level } else { v.sweep_r.level };
                level as u16
            }
            _ => self.regs.get((off >> 1) as usize).copied().unwrap_or(0),
        }
    }

    pub fn write16(&mut self, off: u32, v: u16) {
        let off = off - 0xc00;
        if let Some(r) = self.regs.get_mut((off >> 1) as usize) {
            *r = v;
        }
        match off {
            0x000..=0x17f => {
                let voice = &mut self.voices[(off >> 4) as usize];
                match off & 0xf {
                    0x0 => voice.vol_l = v,
                    0x2 => voice.vol_r = v,
                    0x4 => voice.pitch = v,
                    0x6 => voice.start = v,
                    0x8 => voice.adsr_lo = v,
                    0xa => voice.adsr_hi = v,
                    0xc => voice.env = i32::from(v as i16).max(0),
                    0xe => voice.repeat = v,
                    _ => {}
                }
            }
            0x188 => self.key_on(u32::from(v)),
            0x18a => self.key_on(u32::from(v) << 16),
            0x18c => self.key_off(u32::from(v)),
            0x18e => self.key_off(u32::from(v) << 16),
            0x190 => self.pmon = (self.pmon & 0xffff_0000) | u32::from(v),
            0x192 => self.pmon = (self.pmon & 0xffff) | (u32::from(v) << 16),
            0x194 => self.noise_on = (self.noise_on & 0xffff_0000) | u32::from(v),
            0x196 => self.noise_on = (self.noise_on & 0xffff) | (u32::from(v) << 16),
            0x1a2 => self.reverb_addr = u32::from(v) * 8,
            0x1a4 => self.irq_addr = u32::from(v) * 8,
            0x1a6 => self.transfer_addr = u32::from(v) * 8,
            0x1a8 => self.write_ram(v),
            0x1aa => {
                self.control = v;
                if v & 0x40 == 0 {
                    self.irq_flag = false;
                }
            }
            _ => {}
        }
    }

    fn key_on(&mut self, bits: u32) {
        for i in 0..24 {
            if bits & (1 << i) != 0 {
                let voice = &mut self.voices[i];
                voice.addr = u32::from(voice.start) * 8;
                voice.counter = 0;
                voice.phase = Phase::Attack;
                voice.env = 0;
                voice.env_counter = 0;
                voice.hist = [0; 2];
                voice.buf = [0; 31];
                self.endx &= !(1 << i);
                self.decode_block(i);
            }
        }
    }

    fn key_off(&mut self, bits: u32) {
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if bits & (1 << i) != 0 && voice.phase != Phase::Off {
                voice.phase = Phase::Release;
                voice.env_counter = 0;
            }
        }
    }

    fn write_ram(&mut self, v: u16) {
        let a = (self.transfer_addr as usize) & (SPU_RAM_SIZE - 2);
        self.ram[a..a + 2].copy_from_slice(&v.to_le_bytes());
        self.check_irq(a as u32, 2);
        self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_SIZE as u32 - 1);
    }

    pub fn dma_write(&mut self, v: u32) {
        self.write_ram(v as u16);
        self.write_ram((v >> 16) as u16);
    }

    pub fn dma_read(&mut self) -> u32 {
        let mut out = 0;
        for half in 0..2 {
            let a = (self.transfer_addr as usize) & (SPU_RAM_SIZE - 2);
            out |= u32::from(u16::from_le_bytes([self.ram[a], self.ram[a + 1]])) << (16 * half);
            self.check_irq(a as u32, 2);
            self.transfer_addr = (self.transfer_addr + 2) & (SPU_RAM_SIZE as u32 - 1);
        }
        out
    }

    /// Decodes the block at the voice's address into its buffer.
    fn decode_block(&mut self, i: usize) {
        let addr = self.voices[i].addr as usize & (SPU_RAM_SIZE - 16);
        self.check_irq(addr as u32, 16);
        let block: [u8; 16] = self.ram[addr..addr + 16].try_into().expect("16 bytes");
        let voice = &mut self.voices[i];
        voice.block_flags = block[1];
        if block[1] & 4 != 0 {
            voice.repeat = (addr / 8) as u16;
        }
        let tail = [voice.buf[28], voice.buf[29], voice.buf[30]];
        voice.buf[..3].copy_from_slice(&tail);
        decode_adpcm(&block, &mut voice.hist, &mut voice.buf[3..]);
    }

    /// Advances by `cycles`; returns whether the SPU interrupt fires.
    /// The CPU cycles until the SPU may raise its interrupt (raised, or
    /// enabled: at the next sample), if it can.
    pub fn next_event(&self) -> Option<u32> {
        if self.pending_irq {
            Some(1)
        } else if self.control & 0x40 != 0 && !self.irq_flag {
            Some(CYCLES_PER_SAMPLE - self.frac)
        } else {
            None
        }
    }

    pub fn tick(&mut self, cycles: u32) -> bool {
        self.frac += cycles;
        while self.frac >= CYCLES_PER_SAMPLE {
            self.frac -= CYCLES_PER_SAMPLE;
            self.sample();
        }
        std::mem::take(&mut self.pending_irq)
    }

    fn sample(&mut self) {
        // Noise generator, clocked once per sample (rate not emulated).
        let bit = ((self.noise >> 15) ^ (self.noise >> 12) ^ (self.noise >> 11) ^ (self.noise >> 10) ^ 1) & 1;
        self.noise = (self.noise << 1) | bit;

        let (mut left, mut right) = (0i32, 0i32);
        let (mut rev_l, mut rev_r) = (0i32, 0i32);
        let eon = u32::from(self.regs[0x198 >> 1]) | (u32::from(self.regs[0x19a >> 1]) << 16);
        let mut prev_out = 0i32;
        for i in 0..24 {
            if self.voices[i].phase == Phase::Off {
                self.voices[i].last = 0;
                prev_out = 0;
                continue;
            }
            let voice = &self.voices[i];
            let idx = (voice.counter >> 12) as usize;
            let frac = (voice.counter & 0xfff) as i32;
            let (a, b) = (i32::from(voice.buf[idx + 2]), i32::from(voice.buf[idx + 3]));
            let raw = if self.noise_on & (1 << i) != 0 {
                i32::from(self.noise as i16)
            } else {
                a + (((b - a) * frac) >> 12)
            };
            let sample = (raw * voice.env) >> 15;
            let (reg_l, reg_r) = (voice.vol_l, voice.vol_r);
            let voice = &mut self.voices[i];
            let l = (sample * voice.sweep_l.step(reg_l)) >> 15;
            let r = (sample * voice.sweep_r.step(reg_r)) >> 15;
            left += l;
            right += r;
            if eon & (1 << i) != 0 {
                rev_l += l;
                rev_r += r;
            }

            let mut step = u32::from(voice.pitch.min(0x3fff));
            if i > 0 && self.pmon & (1 << i) != 0 {
                step = ((step as i32 * (prev_out + 0x8000)) >> 15).clamp(0, 0x3fff) as u32;
            }
            voice.last = sample;
            prev_out = sample;
            voice.tick_envelope();
            voice.counter += step;
            while self.voices[i].counter >= 28 << 12 {
                let voice = &mut self.voices[i];
                voice.counter -= 28 << 12;
                if voice.block_flags & 1 != 0 {
                    voice.addr = u32::from(voice.repeat) * 8;
                    if voice.block_flags & 2 == 0 {
                        voice.phase = Phase::Off;
                        voice.env = 0;
                    }
                    self.endx |= 1 << i;
                } else {
                    voice.addr += 16;
                }
                self.decode_block(i);
            }
        }
        let (cl, cr) = self.cd.pop_front().unwrap_or((0, 0));
        // The capture buffers: CD audio as it comes in, voices 1 and 3
        // after their envelopes.
        let at = self.capture * 2;
        let v1 = self.voices[1].last.clamp(-0x8000, 0x7fff) as i16;
        let v3 = self.voices[3].last.clamp(-0x8000, 0x7fff) as i16;
        for (base, v) in [(0x000, cl), (0x400, cr), (0x800, v1), (0xc00, v3)] {
            let a = (base + at) as usize;
            self.ram[a..a + 2].copy_from_slice(&v.to_le_bytes());
            self.check_irq(base + at, 2);
        }
        self.capture = (self.capture + 1) & 0x1ff;
        if self.control & 1 != 0 {
            // CD input volumes: signed, -0x8000 to 0x7FFF.
            let cd_l = (i32::from(cl) * i32::from(self.regs[0x1b0 >> 1] as i16)) >> 15;
            let cd_r = (i32::from(cr) * i32::from(self.regs[0x1b2 >> 1] as i16)) >> 15;
            left += cd_l;
            right += cd_r;
            // Bit 2: CD audio goes through reverb too.
            if self.control & 4 != 0 {
                rev_l += cd_l;
                rev_r += cd_r;
            }
        }
        let (out_l, out_r) = self.reverb(rev_l, rev_r);
        left += out_l;
        right += out_r;
        let main_l = self.main_l.step(self.regs[0x180 >> 1]);
        let main_r = self.main_r.step(self.regs[0x182 >> 1]);
        let muted = self.control & 0x4000 == 0;
        let (l, r) = if muted {
            (0, 0)
        } else {
            ((left * main_l) >> 15, (right * main_r) >> 15)
        };
        if self.output.len() + 2 > MAX_OUTPUT {
            self.output.drain(..2);
        }
        self.output.push(l.clamp(-0x8000, 0x7fff) as i16);
        self.output.push(r.clamp(-0x8000, 0x7fff) as i16);
    }

    fn reg(&self, off: u32) -> u16 {
        self.regs[(off >> 1) as usize]
    }

    fn reg_i(&self, off: u32) -> i32 {
        i32::from(self.reg(off) as i16)
    }

    /// A reverb work area address: register (in 8-byte units) plus `delta`
    /// bytes, relative to the current position, wrapped in the area.
    fn rv_addr(&self, reg: u32, delta: i32) -> usize {
        let base = u32::from(self.reg(rv::BASE)) * 8;
        let size = (SPU_RAM_SIZE as u32).saturating_sub(base).max(2);
        let off =
            (self.reverb_addr - base.min(self.reverb_addr)) as i64 + i64::from(self.reg(reg)) * 8 + i64::from(delta);
        (base + (off.rem_euclid(i64::from(size)) as u32)) as usize & (SPU_RAM_SIZE - 2)
    }

    fn rv_load(&self, reg: u32, delta: i32) -> i32 {
        let a = self.rv_addr(reg, delta);
        i32::from(i16::from_le_bytes([self.ram[a], self.ram[a + 1]]))
    }

    fn rv_store(&mut self, reg: u32, delta: i32, v: i32) {
        let a = self.rv_addr(reg, delta);
        self.ram[a..a + 2].copy_from_slice(&(v.clamp(-0x8000, 0x7fff) as i16).to_le_bytes());
    }

    /// One reverb step: takes this sample's input, returns its output.
    fn reverb(&mut self, in_l: i32, in_r: i32) -> (i32, i32) {
        self.reverb_in.0 += in_l;
        self.reverb_in.1 += in_r;
        self.reverb_odd = !self.reverb_odd;
        if self.reverb_odd {
            return self.reverb_out;
        }
        let (in_l, in_r) = (self.reverb_in.0 / 2, self.reverb_in.1 / 2);
        self.reverb_in = (0, 0);
        let mul = |a: i32, b: i32| ((a * b) >> 15).clamp(-0x8000, 0x7fff);
        let lin = mul(in_l, self.reg_i(rv::V_IN_L));
        let rin = mul(in_r, self.reg_i(rv::V_IN_R));
        let (v_iir, v_wall) = (self.reg_i(rv::V_IIR), self.reg_i(rv::V_WALL));
        let (v_apf1, v_apf2) = (self.reg_i(rv::V_APF1), self.reg_i(rv::V_APF2));
        // Writes only happen with the reverb master enable bit.
        let write = self.control & 0x80 != 0;

        // Same-side and cross reflections; registers come in L/R pairs.
        let reflect = |s: &Self, input: i32, m: u32, d: u32| -> i32 {
            let prev = s.rv_load(m, -2);
            mul(input + mul(s.rv_load(d, 0), v_wall) - prev, v_iir) + prev
        };
        let l_same = reflect(self, lin, rv::M_SAME, rv::D_SAME);
        let r_same = reflect(self, rin, rv::M_SAME + 2, rv::D_SAME + 2);
        let l_diff = reflect(self, lin, rv::M_DIFF, rv::D_DIFF + 2);
        let r_diff = reflect(self, rin, rv::M_DIFF + 2, rv::D_DIFF);
        if write {
            self.rv_store(rv::M_SAME, 0, l_same);
            self.rv_store(rv::M_SAME + 2, 0, r_same);
            self.rv_store(rv::M_DIFF, 0, l_diff);
            self.rv_store(rv::M_DIFF + 2, 0, r_diff);
        }

        let comb = |s: &Self, side: u32| -> i32 {
            mul(s.rv_load(rv::M_COMB1 + side, 0), s.reg_i(rv::V_COMB1))
                + mul(s.rv_load(rv::M_COMB2 + side, 0), s.reg_i(rv::V_COMB2))
                + mul(s.rv_load(rv::M_COMB3 + side, 0), s.reg_i(rv::V_COMB3))
                + mul(s.rv_load(rv::M_COMB4 + side, 0), s.reg_i(rv::V_COMB4))
        };
        let mut out = [
            comb(self, 0).clamp(-0x8000, 0x7fff),
            comb(self, 2).clamp(-0x8000, 0x7fff),
        ];
        for (side, o) in out.iter_mut().enumerate() {
            let side = side as u32 * 2;
            for (m, d, v) in [(rv::M_APF1, rv::D_APF1, v_apf1), (rv::M_APF2, rv::D_APF2, v_apf2)] {
                let delayed = self.rv_load(m + side, -(i32::from(self.reg(d)) * 8));
                let x = (*o - mul(v, delayed)).clamp(-0x8000, 0x7fff);
                if write {
                    self.rv_store(m + side, 0, x);
                }
                *o = (mul(x, v) + delayed).clamp(-0x8000, 0x7fff);
            }
        }
        self.reverb_out = (mul(out[0], self.reg_i(rv::OUT_L)), mul(out[1], self.reg_i(rv::OUT_R)));

        let base = u32::from(self.reg(rv::BASE)) * 8;
        self.reverb_addr = ((self.reverb_addr + 2) & 0x7_fffe).max(base);
        if self.reverb_addr < base {
            self.reverb_addr = base;
        }
        self.reverb_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_sweeps() {
        let mut s = Sweep::default();
        assert_eq!(s.step(0x3fff), 0x7ffe);
        // Linear increase, fastest rate.
        let mut s = Sweep::default();
        let first = s.step(0x8000);
        let later = (0..100).map(|_| s.step(0x8000)).last().unwrap();
        assert!(first > 0 && later > first);
        // Linear decrease reaches 0 and stays there.
        let level = (0..20_000).map(|_| s.step(0xa000)).last().unwrap();
        assert_eq!(level, 0);
    }

    #[test]
    fn reverb_is_silent_without_input_and_echoes_with_it() {
        let mut spu = Spu::new();
        spu.control = 0xc080;
        for (reg, v) in [
            (rv::BASE, 0xe000u16),
            (rv::OUT_L, 0x4000),
            (rv::OUT_R, 0x4000),
            (rv::V_IN_L, 0x7fff),
            (rv::V_IN_R, 0x7fff),
            (rv::V_IIR, 0x7000),
            (rv::V_COMB1, 0x4000),
            (rv::M_SAME, 0x100),
            (rv::M_SAME + 2, 0x100),
            (rv::M_COMB1, 0x80),
            (rv::M_COMB1 + 2, 0x80),
        ] {
            spu.regs[(reg >> 1) as usize] = v;
        }
        spu.reverb_addr = 0xe000 * 8;
        assert_eq!(spu.reverb(0, 0), (0, 0));
        // An impulse, then silence: the comb filter echoes it later.
        let mut heard = false;
        for n in 0..4000 {
            let input = if n < 4 { 0x4000 } else { 0 };
            let (l, _) = spu.reverb(input, input);
            heard |= n > 8 && l != 0;
        }
        assert!(heard);
    }

    #[test]
    fn a_voice_plays_and_stops_at_loop_end() {
        let mut spu = Spu::new();
        // Block at 0x1000 with a constant non-zero sample and the
        // "loop end, no repeat" flag.
        let a = 0x1000;
        spu.ram[a] = 0x00; // shift 0, filter 0
        spu.ram[a + 1] = 0x01;
        for b in &mut spu.ram[a + 2..a + 16] {
            *b = 0x77;
        }
        let base = 0xc00;
        spu.write16(0xd80, 0x3fff);
        spu.write16(0xd82, 0x3fff);
        spu.write16(0xdaa, 0xc000);
        spu.write16(base, 0x3fff);
        spu.write16(base + 2, 0x3fff);
        spu.write16(base + 4, 0x1000);
        spu.write16(base + 6, (a / 8) as u16);
        spu.write16(base + 8, 0x00ff); // fastest attack, sustain max
        spu.write16(0xd88, 1);
        spu.tick(CYCLES_PER_SAMPLE * 20);
        let mut out = Vec::new();
        spu.drain_output(&mut out);
        assert!(out.iter().any(|&s| s != 0));
        spu.tick(CYCLES_PER_SAMPLE * 40);
        assert_ne!(spu.read16(0xd9c) & 1, 0, "ENDX set");
        assert_eq!(spu.voices[0].phase, Phase::Off);
    }

    #[test]
    fn cd_audio_at_full_volume() {
        let mut spu = Spu::new();
        // Main volume at its fixed maximum, the SPU on, CD audio on, and the
        // CD input volume at its maximum (0x7FFF, as Tekken 3 sets it).
        spu.write16(0xd80, 0x3fff);
        spu.write16(0xd82, 0x3fff);
        spu.write16(0xdaa, 0xc001);
        spu.write16(0xdb0, 0x7fff);
        spu.write16(0xdb2, 0x7fff);
        for _ in 0..64 {
            spu.push_cd_audio(10_000, -10_000);
        }
        spu.tick(CYCLES_PER_SAMPLE * 32);
        let mut out = Vec::new();
        spu.drain_output(&mut out);
        let (l, r) = (out[out.len() - 2], out[out.len() - 1]);
        assert!(l > 9_000 && r < -9_000, "CD audio at full volume: {l}, {r}");
    }

    #[test]
    fn capture_buffers_raise_the_interrupt() {
        // As Crash Team Racing: the interrupt at 0x200, in the CD audio's
        // left capture buffer, which the SPU reaches after 256 samples.
        let mut spu = Spu::new();
        spu.write16(0xda4, 0x200 / 8);
        spu.write16(0xdaa, 0xc041);
        for _ in 0..300 {
            spu.push_cd_audio(1234, -1234);
        }
        assert!(!spu.tick(CYCLES_PER_SAMPLE * 200), "not before its sample");
        assert_eq!(spu.read16(0xdae) & 0x800, 0, "writing the first half");
        assert!(!spu.tick(CYCLES_PER_SAMPLE * 56), "not before its sample");
        assert!(spu.tick(CYCLES_PER_SAMPLE), "the interrupt at its sample");
        assert_ne!(spu.read16(0xdae) & 0x800, 0, "writing the second half");
        assert_eq!(&spu.ram[0x200..0x202], &1234i16.to_le_bytes());
        assert_eq!(&spu.ram[0x600..0x602], &(-1234i16).to_le_bytes());
    }
}
