//! The vector units: VU0 (the EE's COP2, and a microprogram processor)
//! and VU1 (microprograms fed by VIF1, drawing through the GIF's path 1).
//!
//! Each instruction is a pair: an upper one (floating point, on four
//! lanes) and a lower one (memory, integers, branches, division...). Both
//! read the registers as they were before the pair; the upper one's result
//! wins when both write the same register. Results are available at once
//! (the hardware stalls on most hazards), except Q, which DIV, SQRT and
//! RSQRT update after their latency: counted in cycles, the stalls on VF
//! registers not written yet (an FMAC's result, four cycles) included.
//!
//! Floats follow the PS2's rules: no infinities, NaNs or denormals.

use crate::gif::Gif;
use crate::gs::Gs;

/// Cycles until an FMAC's result (or a lower instruction's VF write) can
/// be read without stalling.
const VF_LATENCY: u32 = 4;

/// A VF register and its lanes (x = 8, y = 4, z = 2, w = 1).
pub(crate) type VfUse = (usize, u8);

/// The VF registers and lanes (x = 8, y = 4, z = 2, w = 1) an instruction
/// pair reads and writes: (reads, writes). `lower` is None after an I bit.
pub(crate) fn vf_use(upper: u32, lower: Option<u32>) -> (Vec<VfUse>, Vec<VfUse>) {
    let mut reads = Vec::with_capacity(4);
    let mut writes = Vec::with_capacity(2);
    let dest = ((upper >> 21) & 15) as u8;
    let (ft, fs, fd) = (
        ((upper >> 16) & 31) as usize,
        ((upper >> 11) & 31) as usize,
        ((upper >> 6) & 31) as usize,
    );
    let lane = |f: u32| 8u8 >> (f & 3);
    match upper & 0x3F {
        // ADD, SUB, MADD, MSUB, MAX, MINI, MUL with a broadcast lane.
        op @ 0x00..=0x1B => {
            reads.extend([(fs, dest), (ft, lane(op))]);
            writes.push((fd, dest));
        }
        // With Q or I.
        0x1C..=0x27 => {
            reads.push((fs, dest));
            writes.push((fd, dest));
        }
        0x28..=0x2F => {
            reads.extend([(fs, dest), (ft, dest)]);
            writes.push((fd, dest));
        }
        0x3C..=0x3F => {
            let o2 = ((upper >> 6) & 0x1F) << 2 | (upper & 3);
            match o2 {
                // ITOF, FTOI, ABS.
                0x10..=0x17 | 0x1D => {
                    reads.push((fs, dest));
                    writes.push((ft, dest));
                }
                0x1F => reads.extend([(fs, 0b1110), (ft, 0b0001)]),
                // To ACC, with a broadcast lane.
                0x00..=0x0F | 0x18..=0x1B => reads.extend([(fs, dest), (ft, lane(o2))]),
                // To ACC, with Q or I.
                0x1C | 0x1E | 0x20..=0x27 => reads.push((fs, dest)),
                0x28..=0x2E => reads.extend([(fs, dest), (ft, dest)]),
                _ => {}
            }
        }
        _ => {}
    }
    let Some(lower) = lower else {
        return (reads, writes);
    };
    let dest = ((lower >> 21) & 15) as u8;
    let (ft, fs) = (((lower >> 16) & 31) as usize, ((lower >> 11) & 31) as usize);
    let (fsf, ftf) = ((lower >> 21) & 3, (lower >> 23) & 3);
    match lower >> 25 {
        0x00 => writes.push((ft, dest)),
        0x01 => reads.push((fs, dest)),
        0x40 if lower & 0x3C == 0x3C => match (lower & 3) | ((lower >> 4) & 0x7C) {
            0x30 => {
                reads.push((fs, dest));
                writes.push((ft, dest));
            }
            // MR32 reads each lane's neighbour.
            0x31 => {
                reads.push((fs, dest >> 1 | (dest & 1) << 3));
                writes.push((ft, dest));
            }
            0x34 | 0x36 | 0x3D | 0x40 | 0x41 | 0x64 => writes.push((ft, dest)),
            0x35 | 0x37 => reads.push((fs, dest)),
            0x38 | 0x3A => reads.extend([(fs, lane(fsf)), (ft, lane(ftf))]),
            0x39 => reads.push((ft, lane(ftf))),
            0x3C | 0x42 | 0x43 => reads.push((fs, lane(fsf))),
            // The EFU's operations.
            0x70..=0x7A | 0x7C..=0x7F => reads.push((fs, 0b1111)),
            _ => {}
        },
        _ => {}
    }
    (reads, writes)
}

/// Bits of a float the PS2 way: the largest exponent is an ordinary
/// number, denormals are zero.
#[inline]
fn val(bits: u32) -> f32 {
    match bits & 0x7F80_0000 {
        0x7F80_0000 => f32::from_bits((bits & 0x8000_0000) | 0x7F7F_FFFF),
        0 => f32::from_bits(bits & 0x8000_0000),
        _ => f32::from_bits(bits),
    }
}

/// A float's order as MAX and MINI see it: its bits as a sign-magnitude
/// integer (so -0 and +0 are equal, and denormals and the largest
/// exponent compare as their bits say).
#[inline]
pub(crate) fn order_key(bits: u32) -> i32 {
    let magnitude = (bits & 0x7FFF_FFFF) as i32;
    if bits >> 31 != 0 { -magnitude } else { magnitude }
}

/// MAX, MINI: the operand with the larger (smaller) key, its bits as they
/// are, denormals included (programs load small integers into vector
/// registers this way: MAXi from the I register); ties give `t`, as
/// compiled code has them. No flags.
fn minmax(fd: usize, mask: u32, s: [u32; 4], t: [u32; 4], max: bool) -> Option<Commit> {
    let value = std::array::from_fn(|k| {
        let (a, b) = (order_key(s[k]), order_key(t[k]));
        if (max && a > b) || (!max && a < b) { s[k] } else { t[k] }
    });
    Some(Commit {
        reg: Some(fd),
        mask,
        value,
        flags: None,
    })
}

/// A result, clamped the PS2 way; and its flags (zero, sign, underflow,
/// overflow).
#[inline]
fn res(v: f32) -> (u32, u32) {
    let bits = v.to_bits();
    let sign = bits >> 31;
    if v.is_nan() || v.is_infinite() {
        ((bits & 0x8000_0000) | 0x7F7F_FFFF, 0b1000 | sign << 1)
    } else if bits & 0x7F80_0000 == 0 {
        let underflow = u32::from(bits & 0x007F_FFFF != 0);
        (bits & 0x8000_0000, 0b0001 | sign << 1 | underflow << 2)
    } else {
        (bits, sign << 1)
    }
}

/// An upper instruction's result, committed after the lower one runs.
/// Four lanes of results, and their MAC flags (zero bits 0-3, sign 4-7,
/// underflow 8-11, overflow 12-15, with x at the top of each group, as in
/// the instructions' masks): inlined (no closure call per lane).
#[inline(always)]
fn lanes(f: impl Fn(usize) -> f32) -> ([u32; 4], u32) {
    let mut v = [0; 4];
    let mut mac = 0;
    for (k, out) in v.iter_mut().enumerate() {
        let (value, fk) = res(f(k));
        *out = value;
        let bit = 3 - k;
        mac |= (fk & 1) << bit | ((fk >> 1) & 1) << (4 + bit) | ((fk >> 2) & 1) << (8 + bit) | (fk >> 3) << (12 + bit);
    }
    (v, mac)
}

/// The MAC, status and clip flags, as the flag instructions see them.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(C)]
pub struct Flags {
    pub mac: u32,
    pub status: u32,
    pub clip: u32,
}

/// Q's next value and the instructions left before it lands (0: none
/// pending). Saved as an `Option` (as states had it).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "Option<(u32, u32)>", into = "Option<(u32, u32)>")]
#[repr(C)]
pub struct PendingQ {
    pub value: u32,
    pub left: u32,
}

impl From<Option<(u32, u32)>> for PendingQ {
    fn from(p: Option<(u32, u32)>) -> Self {
        p.map_or(Self::default(), |(value, left)| Self {
            value,
            left: left.max(1),
        })
    }
}

impl From<PendingQ> for Option<(u32, u32)> {
    fn from(p: PendingQ) -> Self {
        (p.left != 0).then_some((p.value, p.left))
    }
}

struct Commit {
    /// A VF register, or None for ACC.
    reg: Option<usize>,
    mask: u32,
    value: [u32; 4],
    flags: Option<u32>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Vu {
    pub index: usize,
    /// VF registers as bits; VF0 is (0, 0, 0, 1).
    pub vf: [[u32; 4]; 32],
    pub vi: [u16; 16],
    pub acc: [u32; 4],
    pub q: u32,
    pub p: u32,
    pub i: u32,
    pub r: u32,
    pub mac: u32,
    pub status: u32,
    pub clip: u32,
    /// The flags (MAC, status, clip) after each of the last four
    /// instructions of a microprogram: they become visible to the flag
    /// instructions four instructions after the one that set them (by
    /// slot: instruction n reads slot n % 4, then writes it).
    pub flag_history: [Flags; 4],
    /// The flags the current instruction sees.
    seen: Flags,
    pub micro: Box<[u8]>,
    pub data: Box<[u8]>,
    /// The next instruction, in 64-bit units.
    pub pc: u32,
    /// TOP and ITOP (from VIF1 and its double buffering, or VIF0's ITOP).
    pub top: u16,
    pub itop: u16,
    pub pending_q: PendingQ,
    /// Cycles run (instructions and stalls), and when each VF register's
    /// pending result is ready, with the lanes it writes: an instruction
    /// reading one sooner stalls, the hardware's way, which Q's latency
    /// counts (games time their divisions by it).
    #[serde(skip)]
    pub(crate) cycle: u32,
    #[serde(skip)]
    pub(crate) vf_ready: [u32; 32],
    #[serde(skip)]
    pub(crate) vf_mask: [u32; 32],
    branch: Option<u32>,
    /// Instructions run, and runs stopped for running too long (no end),
    /// for diagnostics.
    pub steps: u64,
    #[serde(skip)]
    pub runaways: u64,
    /// Where the running program started, and its XGKICKs when set
    /// (diagnostics).
    #[serde(skip)]
    start: u32,
    #[serde(skip)]
    pub kicks: Option<Vec<Kick>>,
    /// Traces the runs starting there (diagnostics).
    #[serde(skip)]
    pub trace_from: Option<u32>,
    #[serde(skip)]
    trace: Vec<(u32, [u16; 16])>,
    /// Microprograms compiled to WebAssembly (in browsers; see `vu_jit`),
    /// and the instructions a compiled one ran when it stopped.
    #[serde(skip)]
    pub jit: Option<Box<crate::vu_jit::VuJit>>,
    #[serde(skip)]
    pub run_n: u32,
    /// Counts writes to micro memory: compiled programs check theirs again
    /// after one.
    #[serde(skip)]
    pub micro_gen: u64,
}

/// A packet VU1 sent the GIF: where its program started and kicked it,
/// from where, and its quadwords.
#[derive(Clone, Debug)]
pub struct Kick {
    pub start: u32,
    pub pc: u32,
    pub addr: u32,
    pub packet: Vec<[u32; 4]>,
    /// The run's instructions so far (pc, VI registers), when traced.
    pub trace: Vec<(u32, [u16; 16])>,
    /// The micro memory then, when traced.
    pub micro: Vec<u8>,
}

/// Instructions a microprogram may run (one that never ends is stopped).
pub const MAX_STEPS: u64 = 4_000_000;

impl Vu {
    pub fn new(index: usize) -> Self {
        let (micro, data) = if index == 0 { (4096, 4096) } else { (16384, 16384) };
        let mut vf = [[0; 4]; 32];
        vf[0][3] = 1f32.to_bits();
        Self {
            index,
            vf,
            vi: [0; 16],
            acc: [0; 4],
            q: 0,
            p: 0,
            i: 0,
            r: 0x3F80_0000,
            mac: 0,
            status: 0,
            clip: 0,
            flag_history: [Flags::default(); 4],
            seen: Flags::default(),
            micro: vec![0; micro].into_boxed_slice(),
            data: vec![0; data].into_boxed_slice(),
            pc: 0,
            top: 0,
            itop: 0,
            pending_q: PendingQ::default(),
            cycle: 0,
            vf_ready: [0; 32],
            vf_mask: [0; 32],
            branch: None,
            steps: 0,
            runaways: 0,
            start: 0,
            kicks: None,
            trace_from: None,
            trace: Vec::new(),
            jit: None,
            run_n: 0,
            micro_gen: 0,
        }
    }

    // --- Memory ------------------------------------------------------------------

    fn qwords(&self) -> u32 {
        (self.data.len() / 16) as u32
    }

    pub fn read_q(&self, qaddr: u32) -> [u32; 4] {
        let at = ((qaddr % self.qwords()) * 16) as usize;
        std::array::from_fn(|k| {
            u32::from_le_bytes(self.data[at + 4 * k..at + 4 * k + 4].try_into().unwrap_or_default())
        })
    }

    /// Lane `k` (0: x) of data quadword `qaddr`.
    #[inline]
    pub fn write_lane(&mut self, qaddr: u32, k: usize, v: u32) {
        let at = ((qaddr % self.qwords()) * 16) as usize + 4 * k;
        self.data[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub fn write_q(&mut self, qaddr: u32, v: [u32; 4], mask: u32) {
        let at = ((qaddr % self.qwords()) * 16) as usize;
        for (k, w) in v.iter().enumerate() {
            if mask & (8 >> k) != 0 {
                self.data[at + 4 * k..at + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
    }

    fn set_vf(&mut self, r: usize, mask: u32, v: [u32; 4]) {
        if r == 0 {
            return;
        }
        for k in 0..4 {
            if mask & (8 >> k) != 0 {
                self.vf[r][k] = v[k];
            }
        }
    }

    fn set_vi(&mut self, r: usize, v: u16) {
        if r != 0 {
            self.vi[r] = v;
        }
    }

    // --- Running microprograms -------------------------------------------------

    /// The PC's mask: it wraps at the end of micro memory (2048 or 512
    /// instructions), as do branch targets and the links saved.
    pub fn wrap(&self) -> u32 {
        (self.micro.len() / 8).max(1) as u32 - 1
    }

    /// The flags as they stand.
    pub fn flags(&self) -> Flags {
        Flags {
            mac: self.mac,
            status: self.status,
            clip: self.clip,
        }
    }

    /// Runs the microprogram at `pc` (in 64-bit units) until its end:
    /// compiled, when it is (and no diagnostics are on).
    pub fn run(&mut self, pc: u32, gif: &mut Gif, gs: &mut Gs) {
        self.pc = pc & self.wrap();
        self.start = self.pc;
        self.trace.clear();
        self.branch = None;
        // The flags as they stand when the program starts.
        self.flag_history = [self.flags(); 4];
        let mut n = 0;
        if self.trace_from.is_none()
            && self.kicks.is_none()
            && let Some(mut jit) = self.jit.take()
        {
            let ran = jit.run(self, gif, gs);
            self.jit = Some(jit);
            match ran {
                // Ended: as the interpreter ends.
                Some(crate::vu_jit::Ran::Ended) => return self.finish(u64::from(self.run_n)),
                // Stopped at pc, for the interpreter to go on.
                Some(crate::vu_jit::Ran::Stopped) => n = u64::from(self.run_n),
                None => {}
            }
        }
        self.interpret(n, gif, gs);
    }

    /// Q's last value lands, and a program that never ended counts.
    fn finish(&mut self, n: u64) {
        if n >= MAX_STEPS {
            self.runaways += 1;
        }
        if self.pending_q.left != 0 {
            self.q = self.pending_q.value;
            self.pending_q = PendingQ::default();
        }
    }

    /// Runs the current program from pc, its instruction `n`, to its end.
    fn interpret(&mut self, mut n: u64, gif: &mut Gif, gs: &mut Gs) {
        let wrap = self.wrap();
        let mut ending = false;
        while n < MAX_STEPS {
            let at = self.pc as usize * 8;
            let word = u64::from_le_bytes(self.micro[at..at + 8].try_into().unwrap_or_default());
            let upper = (word >> 32) as u32;
            let lower = word as u32;
            if self.trace_from == Some(self.start) && self.trace.len() < 20_000 {
                self.trace.push((self.pc, self.vi));
            }
            self.pc = (self.pc + 1) & wrap;
            let branch = self.branch.take();
            self.stall(upper, lower);
            self.tick();
            self.seen = self.flag_history[(n % 4) as usize];
            let commit = self.upper(upper);
            if upper & (1 << 31) != 0 {
                // I bit: the lower word is the I register's new value.
                self.i = lower;
            } else {
                self.lower(lower, gif, gs);
            }
            if let Some(c) = commit {
                self.commit(c);
            }
            self.flag_history[(n % 4) as usize] = self.flags();
            self.steps += 1;
            if let Some(target) = branch {
                self.pc = target & wrap;
            }
            if ending {
                break;
            }
            // E bit: the next instruction is the last.
            if upper & (1 << 30) != 0 {
                ending = true;
            }
            n += 1;
        }
        self.finish(n);
    }

    /// Stalls on the VF registers the pair reads that are not written yet
    /// (Q's latency goes on meanwhile), then notes those it writes.
    fn stall(&mut self, upper: u32, lower: u32) {
        let (reads, writes) = vf_use(upper, (upper & (1 << 31) == 0).then_some(lower));
        let mut stall = 0i32;
        for &(r, m) in &reads {
            if r != 0 && self.vf_mask[r] & u32::from(m) != 0 {
                stall = stall.max(self.vf_ready[r].wrapping_sub(self.cycle) as i32);
            }
        }
        if stall > 0 {
            let left = self.pending_q.left;
            if left != 0 {
                if left <= stall as u32 {
                    self.q = self.pending_q.value;
                    self.pending_q = PendingQ::default();
                } else {
                    self.pending_q.left = left - stall as u32;
                }
            }
            self.cycle = self.cycle.wrapping_add(stall as u32);
        }
        for &(r, m) in &writes {
            if r != 0 {
                self.vf_ready[r] = self.cycle.wrapping_add(VF_LATENCY);
                self.vf_mask[r] = u32::from(m);
            }
        }
        self.cycle = self.cycle.wrapping_add(1);
    }

    fn tick(&mut self) {
        match self.pending_q.left {
            0 => {}
            1 => {
                self.q = self.pending_q.value;
                self.pending_q = PendingQ::default();
            }
            _ => self.pending_q.left -= 1,
        }
    }

    fn wait_q(&mut self) {
        if self.pending_q.left != 0 {
            self.q = self.pending_q.value;
            self.pending_q = PendingQ::default();
        }
    }

    // --- Upper instructions -----------------------------------------------------

    /// Runs an upper instruction alone (VU0 macro mode).
    pub fn macro_upper(&mut self, instr: u32) {
        if let Some(c) = self.upper(instr) {
            self.commit(c);
        }
    }

    fn commit(&mut self, c: Commit) {
        match c.reg {
            Some(r) => self.set_vf(r, c.mask, c.value),
            None => {
                for k in 0..4 {
                    if c.mask & (8 >> k) != 0 {
                        self.acc[k] = c.value[k];
                    }
                }
            }
        }
        if let Some(f) = c.flags {
            // The lanes written only: the mask, in each group.
            let mac = f & (c.mask * 0x1111);
            self.mac = mac;
            let now = u32::from(mac & 0xF != 0)
                | u32::from(mac & 0xF0 != 0) << 1
                | u32::from(mac & 0xF00 != 0) << 2
                | u32::from(mac & 0xF000 != 0) << 3;
            self.status = (self.status & 0xFF0) | now | (now << 6);
        }
    }

    fn upper(&mut self, instr: u32) -> Option<Commit> {
        // NOP (the most common upper instruction): nothing to prepare.
        if instr & 0x7FF == 0x2FF {
            return None;
        }
        let mask = (instr >> 21) & 0xF;
        let ft = ((instr >> 16) & 31) as usize;
        let fs = ((instr >> 11) & 31) as usize;
        let fd = ((instr >> 6) & 31) as usize;
        let funct = instr & 0x3F;
        // MAX and MINI take the registers' bits as they are (see `minmax`).
        let (raw_s, raw_t) = (self.vf[fs], self.vf[ft]);
        let s = raw_s.map(val);
        let t = raw_t.map(val);
        let acc = self.acc.map(val);
        let bc = (funct & 3) as usize;
        let broadcast = [t[bc]; 4];
        let q = [val(self.q); 4];
        let iv = [val(self.i); 4];
        let to = |reg: Option<usize>, (value, flags): ([u32; 4], u32)| {
            Some(Commit {
                reg,
                mask,
                value,
                flags: Some(flags),
            })
        };
        let fdr = Some(fd);
        match funct {
            0x00..=0x03 => to(fdr, lanes(|k| s[k] + broadcast[k])),
            0x04..=0x07 => to(fdr, lanes(|k| s[k] - broadcast[k])),
            0x08..=0x0B => to(fdr, lanes(|k| acc[k] + s[k] * broadcast[k])),
            0x0C..=0x0F => to(fdr, lanes(|k| acc[k] - s[k] * broadcast[k])),
            0x10..=0x13 => minmax(fd, mask, raw_s, [raw_t[bc]; 4], true),
            0x14..=0x17 => minmax(fd, mask, raw_s, [raw_t[bc]; 4], false),
            0x18..=0x1B => to(fdr, lanes(|k| s[k] * broadcast[k])),
            0x1C => to(fdr, lanes(|k| s[k] * q[k])),
            0x1D => minmax(fd, mask, raw_s, [self.i; 4], true),
            0x1E => to(fdr, lanes(|k| s[k] * iv[k])),
            0x1F => minmax(fd, mask, raw_s, [self.i; 4], false),
            0x20 => to(fdr, lanes(|k| s[k] + q[k])),
            0x21 => to(fdr, lanes(|k| acc[k] + s[k] * q[k])),
            0x22 => to(fdr, lanes(|k| s[k] + iv[k])),
            0x23 => to(fdr, lanes(|k| acc[k] + s[k] * iv[k])),
            0x24 => to(fdr, lanes(|k| s[k] - q[k])),
            0x25 => to(fdr, lanes(|k| acc[k] - s[k] * q[k])),
            0x26 => to(fdr, lanes(|k| s[k] - iv[k])),
            0x27 => to(fdr, lanes(|k| acc[k] - s[k] * iv[k])),
            0x28 => to(fdr, lanes(|k| s[k] + t[k])),
            0x29 => to(fdr, lanes(|k| acc[k] + s[k] * t[k])),
            0x2A => to(fdr, lanes(|k| s[k] * t[k])),
            0x2B => minmax(fd, mask, raw_s, raw_t, true),
            0x2C => to(fdr, lanes(|k| s[k] - t[k])),
            0x2D => to(fdr, lanes(|k| acc[k] - s[k] * t[k])),
            0x2E => {
                // OPMSUB: fd.xyz = ACC.xyz - fs.yzx * ft.zxy.
                let cross = [s[1] * t[2], s[2] * t[0], s[0] * t[1], 0.0];
                to(fdr, lanes(|k| acc[k] - cross[k]))
            }
            0x2F => minmax(fd, mask, raw_s, raw_t, false),
            0x3C..=0x3F => {
                let op = (funct & 3) | ((instr >> 4) & 0x7C);
                self.upper_special(op, mask, ft, fs, s, t, acc, q, iv)
            }
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn upper_special(
        &mut self,
        op: u32,
        mask: u32,
        ft: usize,
        fs: usize,
        s: [f32; 4],
        t: [f32; 4],
        acc: [f32; 4],
        q: [f32; 4],
        iv: [f32; 4],
    ) -> Option<Commit> {
        let bc = (op & 3) as usize;
        let broadcast = [t[bc]; 4];
        let to_acc = |(value, flags): ([u32; 4], u32)| {
            Some(Commit {
                reg: None,
                mask,
                value,
                flags: Some(flags),
            })
        };
        let plain = |reg: usize, value: [u32; 4]| {
            Some(Commit {
                reg: Some(reg),
                mask,
                value,
                flags: None,
            })
        };
        match op {
            0x00..=0x03 => to_acc(lanes(|k| s[k] + broadcast[k])),
            0x04..=0x07 => to_acc(lanes(|k| s[k] - broadcast[k])),
            0x08..=0x0B => to_acc(lanes(|k| acc[k] + s[k] * broadcast[k])),
            0x0C..=0x0F => to_acc(lanes(|k| acc[k] - s[k] * broadcast[k])),
            // ITOF0/4/12/15.
            0x10..=0x13 => {
                let scale = [1.0, 16.0, 4096.0, 32768.0][(op & 3) as usize];
                plain(ft, self.vf[fs].map(|w| res(w as i32 as f32 / scale).0))
            }
            // FTOI0/4/12/15: truncating, saturating.
            0x14..=0x17 => {
                let scale = [1.0f64, 16.0, 4096.0, 32768.0][(op & 3) as usize];
                plain(
                    ft,
                    s.map(|v| {
                        let x = (f64::from(v) * scale).trunc();
                        x.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32 as u32
                    }),
                )
            }
            0x18..=0x1B => to_acc(lanes(|k| s[k] * broadcast[k])),
            0x1C => to_acc(lanes(|k| s[k] * q[k])),
            0x1D => plain(ft, self.vf[fs].map(|w| w & 0x7FFF_FFFF)),
            0x1E => to_acc(lanes(|k| s[k] * iv[k])),
            0x1F => {
                // CLIP: fs.xyz against ±|ft.w|, 6 new bits under the old ones.
                let w = t[3].abs();
                let bits = u32::from(s[0] > w)
                    | u32::from(s[0] < -w) << 1
                    | u32::from(s[1] > w) << 2
                    | u32::from(s[1] < -w) << 3
                    | u32::from(s[2] > w) << 4
                    | u32::from(s[2] < -w) << 5;
                self.clip = ((self.clip << 6) | bits) & 0xFF_FFFF;
                None
            }
            0x20 => to_acc(lanes(|k| s[k] + q[k])),
            0x21 => to_acc(lanes(|k| acc[k] + s[k] * q[k])),
            0x22 => to_acc(lanes(|k| s[k] + iv[k])),
            0x23 => to_acc(lanes(|k| acc[k] + s[k] * iv[k])),
            0x24 => to_acc(lanes(|k| s[k] - q[k])),
            0x25 => to_acc(lanes(|k| acc[k] - s[k] * q[k])),
            0x26 => to_acc(lanes(|k| s[k] - iv[k])),
            0x27 => to_acc(lanes(|k| acc[k] - s[k] * iv[k])),
            0x28 => to_acc(lanes(|k| s[k] + t[k])),
            0x29 => to_acc(lanes(|k| acc[k] + s[k] * t[k])),
            0x2A => to_acc(lanes(|k| s[k] * t[k])),
            0x2C => to_acc(lanes(|k| s[k] - t[k])),
            0x2D => to_acc(lanes(|k| acc[k] - s[k] * t[k])),
            0x2E => {
                // OPMULA: ACC.xyz = fs.yzx * ft.zxy.
                let cross = [s[1] * t[2], s[2] * t[0], s[0] * t[1], 0.0];
                to_acc(lanes(|k| cross[k]))
            }
            // NOP, and the rest.
            _ => None,
        }
    }

    // --- Lower instructions -----------------------------------------------------

    fn lower(&mut self, instr: u32, gif: &mut Gif, gs: &mut Gs) {
        let op = instr >> 25;
        let mask = (instr >> 21) & 0xF;
        let it = ((instr >> 16) & 15) as usize;
        let is = ((instr >> 11) & 15) as usize;
        let imm11 = ((instr & 0x7FF) as i32) << 21 >> 21;
        let vi = |r: usize| u32::from(self.vi[r]);
        let target = |pc: u32| (pc as i32).wrapping_add(imm11) as u32;
        match op {
            0x00 => {
                let ft = ((instr >> 16) & 31) as usize;
                let v = self.read_q(vi(is).wrapping_add(imm11 as u32));
                self.set_vf(ft, mask, v);
            }
            0x01 => {
                let fs = ((instr >> 11) & 31) as usize;
                let it = ((instr >> 16) & 15) as usize;
                let v = self.vf[fs];
                self.write_q(vi(it).wrapping_add(imm11 as u32), v, mask);
            }
            0x04 => {
                // ILW: the selected lane's low halfword.
                let v = self.read_q(vi(is).wrapping_add(imm11 as u32));
                let k = (0..4).find(|k| mask & (8 >> k) != 0).unwrap_or(0);
                self.set_vi(it, v[k] as u16);
            }
            0x05 => {
                let a = vi(is).wrapping_add(imm11 as u32);
                self.write_q(a, [vi(it); 4], mask);
            }
            0x08 | 0x09 => {
                let imm15 = (instr & 0x7FF) | ((instr >> 10) & 0x7800);
                let v = if op == 0x08 {
                    vi(is).wrapping_add(imm15)
                } else {
                    vi(is).wrapping_sub(imm15)
                };
                self.set_vi(it, v as u16);
            }
            // Clip flag tests and setting, on 24 bits.
            0x10 => self.set_vi(1, u16::from(self.seen.clip & 0xFF_FFFF == instr & 0xFF_FFFF)),
            0x11 => self.clip = instr & 0xFF_FFFF,
            0x12 => self.set_vi(1, u16::from(self.seen.clip & instr & 0xFF_FFFF != 0)),
            0x13 => self.set_vi(1, u16::from((self.seen.clip | instr) & 0xFF_FFFF == 0xFF_FFFF)),
            0x1C => self.set_vi(it, (self.seen.clip & 0xFFF) as u16),
            // Status flag tests (12 bits) and setting (the sticky ones).
            0x14..=0x17 => {
                let imm12 = (instr & 0x7FF) | ((instr >> 10) & 0x800);
                let st = self.seen.status & 0xFFF;
                match op {
                    0x14 => self.set_vi(it, u16::from(st == imm12)),
                    0x15 => self.status = (self.status & 0x3F) | (imm12 & 0xFC0),
                    0x16 => self.set_vi(it, u16::from(st & imm12 != 0)),
                    _ => self.set_vi(it, (st | imm12) as u16),
                }
            }
            // MAC flag tests.
            0x18 => self.set_vi(it, u16::from(self.seen.mac & 0xFFFF == vi(is))),
            0x1A => self.set_vi(it, (self.seen.mac & vi(is)) as u16),
            0x1B => self.set_vi(it, ((self.seen.mac | vi(is)) & 0xFFFF) as u16),
            // Branches: the delay slot runs first.
            0x20 => self.branch = Some(target(self.pc)),
            0x21 => {
                self.set_vi(it, self.pc.wrapping_add(1) as u16);
                self.branch = Some(target(self.pc));
            }
            0x24 => self.branch = Some(vi(is)),
            0x25 => {
                let to = vi(is);
                self.set_vi(it, self.pc.wrapping_add(1) as u16);
                self.branch = Some(to);
            }
            0x28..=0x2F => {
                let a = self.vi[it] as i16;
                let b = self.vi[is] as i16;
                let taken = match op {
                    0x28 => a == b,
                    0x29 => a != b,
                    0x2C => b < 0,
                    0x2D => b > 0,
                    0x2E => b <= 0,
                    0x2F => b >= 0,
                    _ => false,
                };
                if taken {
                    self.branch = Some(target(self.pc));
                }
            }
            0x40 => self.lower_special(instr, gif, gs),
            _ => {}
        }
    }

    /// Runs a lower instruction alone (not a branch): compiled programs
    /// leave the rare ones (the EFU, XGKICK, the random numbers) to it.
    pub fn exec_lower(&mut self, instr: u32, gif: &mut Gif, gs: &mut Gs) {
        self.lower(instr, gif, gs);
    }

    fn lower_special(&mut self, instr: u32, gif: &mut Gif, gs: &mut Gs) {
        let funct = instr & 0x3F;
        let it = ((instr >> 16) & 15) as usize;
        let is = ((instr >> 11) & 15) as usize;
        let id = ((instr >> 6) & 15) as usize;
        match funct {
            0x30..=0x35 => self.integer(funct, it, is, id, instr),
            0x3C..=0x3F => {
                let op = (funct & 3) | ((instr >> 4) & 0x7C);
                self.special2(op, instr, Some((gif, gs)));
            }
            _ => {}
        }
    }

    /// IADD, ISUB, IADDI, IAND, IOR (VIADD... in macro mode).
    pub fn integer(&mut self, funct: u32, it: usize, is: usize, id: usize, instr: u32) {
        let a = self.vi[is];
        let b = self.vi[it];
        let v = match funct {
            0x30 => a.wrapping_add(b),
            0x31 => a.wrapping_sub(b),
            0x32 => {
                // IADDI: it = is + imm5.
                let imm5 = (((instr >> 6) & 0x1F) as i16) << 11 >> 11;
                self.set_vi(it, a.wrapping_add(imm5 as u16));
                return;
            }
            0x34 => a & b,
            0x35 => a | b,
            _ => return,
        };
        self.set_vi(id, v);
    }

    /// The lower instructions of the special table (and VU0 macro ones):
    /// moves, loads with increment, division, the EFU...
    pub fn special2(&mut self, op: u32, instr: u32, out: Option<(&mut Gif, &mut Gs)>) {
        let mask = (instr >> 21) & 0xF;
        let ft = ((instr >> 16) & 31) as usize;
        let fs = ((instr >> 11) & 31) as usize;
        let it = ft & 15;
        let is = fs & 15;
        let fsf = ((instr >> 21) & 3) as usize;
        let ftf = ((instr >> 23) & 3) as usize;
        let vfs = self.vf[fs];
        match op {
            0x30 => self.set_vf(ft, mask, vfs),
            0x31 => self.set_vf(ft, mask, [vfs[1], vfs[2], vfs[3], vfs[0]]),
            0x34 => {
                let v = self.read_q(u32::from(self.vi[is]));
                self.set_vf(ft, mask, v);
                self.set_vi(is, self.vi[is].wrapping_add(1));
            }
            0x35 => {
                self.write_q(u32::from(self.vi[it]), vfs, mask);
                self.set_vi(it, self.vi[it].wrapping_add(1));
            }
            0x36 => {
                self.set_vi(is, self.vi[is].wrapping_sub(1));
                let v = self.read_q(u32::from(self.vi[is]));
                self.set_vf(ft, mask, v);
            }
            0x37 => {
                self.set_vi(it, self.vi[it].wrapping_sub(1));
                self.write_q(u32::from(self.vi[it]), vfs, mask);
            }
            0x38..=0x3A => {
                self.wait_q();
                let s = val(vfs[fsf]);
                let t = val(self.vf[ft][ftf]);
                let (q, latency) = match op {
                    0x38 => (
                        if t == 0.0 {
                            ((vfs[fsf] ^ self.vf[ft][ftf]) & 0x8000_0000) | 0x7F7F_FFFF
                        } else {
                            res(s / t).0
                        },
                        7,
                    ),
                    0x39 => (res(t.abs().sqrt()).0, 7),
                    _ => {
                        let r = t.abs().sqrt();
                        (if r == 0.0 { 0x7F7F_FFFF } else { res(s / r).0 }, 13)
                    }
                };
                self.pending_q = PendingQ {
                    value: q,
                    left: latency,
                };
                // Macro mode (no GIF given): the EE's COP2 instructions
                // interlock, one reading Q waits for the division: Q is the
                // result for them at once (Jak II and 3 divide, then
                // multiply by Q a few COP2 instructions later; their text's
                // first vertex took the previous Q).
                if out.is_none() {
                    self.wait_q();
                }
            }
            0x3B => self.wait_q(),
            0x3C => self.set_vi(it, vfs[fsf] as u16),
            0x3D => {
                let v = self.vi[is] as i16 as i32 as u32;
                self.set_vf(ft, mask, [v; 4]);
            }
            0x3E => {
                let v = self.read_q(u32::from(self.vi[is]));
                let k = (0..4).find(|k| mask & (8 >> k) != 0).unwrap_or(0);
                self.set_vi(it, v[k] as u16);
            }
            0x3F => {
                let v = u32::from(self.vi[it]);
                self.write_q(u32::from(self.vi[is]), [v; 4], mask);
            }
            // The random number generator (23 bits of mantissa, in [1, 2)).
            0x40 => {
                let x = self.r;
                let bit = ((x >> 4) ^ (x >> 22)) & 1;
                self.r = 0x3F80_0000 | (((x << 1) | bit) & 0x7F_FFFF);
                self.set_vf(ft, mask, [self.r; 4]);
            }
            0x41 => self.set_vf(ft, mask, [self.r; 4]),
            0x42 => self.r = 0x3F80_0000 | (vfs[fsf] & 0x7F_FFFF),
            0x43 => self.r = 0x3F80_0000 | ((self.r ^ vfs[fsf]) & 0x7F_FFFF),
            0x64 => self.set_vf(ft, mask, [self.p; 4]),
            0x68 => self.set_vi(it, self.top),
            0x69 => self.set_vi(it, self.itop),
            0x6C => {
                if let Some((gif, gs)) = out {
                    let addr = u32::from(self.vi[is]);
                    let n = gif.kick_path1(self, addr, gs);
                    if self.kicks.is_some() {
                        let packet = (0..n).map(|k| self.read_q(addr + k)).collect();
                        let kick = Kick {
                            start: self.start,
                            pc: self.pc.wrapping_sub(1) & 0x7FF,
                            addr,
                            packet,
                            trace: self.trace.clone(),
                            micro: if self.trace.is_empty() {
                                Vec::new()
                            } else {
                                self.micro.to_vec()
                            },
                        };
                        if let Some(kicks) = self.kicks.as_mut() {
                            kicks.push(kick);
                        }
                    }
                }
            }
            // The EFU: results in P at once.
            0x70..=0x7E => {
                let v = vfs.map(val);
                let e = val(vfs[fsf]);
                let p = match op {
                    0x70 => v[0] * v[0] + v[1] * v[1] + v[2] * v[2],
                    0x71 => 1.0 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]),
                    0x72 => (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt(),
                    0x73 => 1.0 / (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt(),
                    0x74 => (v[1] / v[0]).atan(),
                    0x75 => (v[2] / v[0]).atan(),
                    0x76 => v[0] + v[1] + v[2] + v[3],
                    0x78 => e.abs().sqrt(),
                    0x79 => 1.0 / e.abs().sqrt(),
                    0x7A => 1.0 / e,
                    0x7C => e.sin(),
                    0x7D => e.atan(),
                    0x7E => (-e).exp(),
                    _ => val(self.p),
                };
                self.p = res(p).0;
            }
            _ => {}
        }
    }

    /// Resets the unit (FBRST).
    pub fn reset(&mut self) {
        let (micro, data) = (std::mem::take(&mut self.micro), std::mem::take(&mut self.data));
        let jit = self.jit.take();
        let generation = self.micro_gen;
        *self = Self::new(self.index);
        self.micro = micro;
        self.data = data;
        self.jit = jit;
        self.micro_gen = generation;
    }

    /// Control registers 16-31 as CFC2 reads them (VU0).
    pub fn control(&self, reg: u32) -> u32 {
        match reg {
            0..=15 => u32::from(self.vi[reg as usize]),
            16 => self.status,
            17 => self.mac,
            18 => self.clip,
            20 => self.r,
            21 => self.i,
            22 => self.q,
            26 => self.pc,
            _ => 0,
        }
    }

    pub fn set_control(&mut self, reg: u32, v: u32) {
        match reg {
            1..=15 => self.vi[reg as usize] = v as u16,
            16 => self.status = (self.status & 0x3F) | (v & 0xFC0),
            17 => self.mac = v & 0xFFFF,
            18 => self.clip = v & 0xFF_FFFF,
            20 => self.r = v,
            21 => self.i = v,
            22 => self.q = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles (upper, lower) pairs into micro memory and runs them.
    fn run(prog: &[(u32, u32)], setup: impl FnOnce(&mut Vu)) -> Vu {
        let mut vu = Vu::new(1);
        for (k, (u, l)) in prog.iter().enumerate() {
            vu.micro[k * 8..k * 8 + 4].copy_from_slice(&l.to_le_bytes());
            vu.micro[k * 8 + 4..k * 8 + 8].copy_from_slice(&u.to_le_bytes());
        }
        setup(&mut vu);
        let mut gif = Gif::default();
        let mut gs = Gs::default();
        vu.run(0, &mut gif, &mut gs);
        vu
    }

    const NOP: u32 = 0x2FF;
    const LNOP: u32 = 0x8000_033C;
    const E: u32 = 1 << 30;

    fn f(v: [f32; 4]) -> [u32; 4] {
        v.map(f32::to_bits)
    }

    /// MAXi and MINI keep the operands' bits, denormals included:
    /// programs load small integers (a GIF tag's NLOOP) with MAXi from the
    /// I register, and the tag must not lose them.
    #[test]
    fn max_and_mini_keep_the_bits() {
        // I = 3 (lower word, I bit), then MAXi.x vf1, vf0; MINI.y vf2, vf3,
        // vf4 (-1.0 against a negative denormal: -1.0); MAX.z vf5, vf6, vf7
        // (-0 against +0: a tie, the second).
        let maxi = (0x8 << 21) | (1 << 6) | 0x1D;
        let mini = (0x4 << 21) | (4 << 16) | (3 << 11) | (2 << 6) | 0x2F;
        let max = (0x2 << 21) | (7 << 16) | (6 << 11) | (5 << 6) | 0x2B;
        let vu = run(
            &[
                (NOP | 1 << 31, 3),
                (maxi, LNOP),
                (mini, LNOP),
                (max | E, LNOP),
                (NOP, LNOP),
            ],
            |vu| {
                vu.vf[3][1] = (-1.0f32).to_bits();
                vu.vf[4][1] = 0x8000_0005;
                vu.vf[6][2] = 0x8000_0000;
                vu.vf[7][2] = 0;
            },
        );
        assert_eq!(vu.vf[1][0], 3);
        assert_eq!(vu.vf[2][1], (-1.0f32).to_bits());
        assert_eq!(vu.vf[5][2], 0);
    }

    #[test]
    fn upper_ops_and_end_bit() {
        // ADD.xyzw vf3, vf1, vf2; MULx.xyzw vf4, vf1, vf2x (E bit), then a
        // NOP in the delay slot.
        let add = (0xF << 21) | (2 << 16) | (1 << 11) | (3 << 6) | 0x28;
        let mulx = (0xF << 21) | (2 << 16) | (1 << 11) | (4 << 6) | 0x18 | E;
        let vu = run(&[(add, LNOP), (mulx, LNOP), (NOP, LNOP), (NOP, LNOP)], |vu| {
            vu.vf[1] = f([1.0, 2.0, 3.0, 4.0]);
            vu.vf[2] = f([10.0, 20.0, 30.0, 40.0]);
        });
        assert_eq!(vu.vf[3], f([11.0, 22.0, 33.0, 44.0]));
        assert_eq!(vu.vf[4], f([10.0, 20.0, 30.0, 40.0]));
        assert_eq!(vu.steps, 3);
    }

    #[test]
    fn lower_reads_before_upper_writes() {
        // ADD vf1, vf1, vf1 with SQ vf1 to 0 in the same pair.
        let add = (0xF << 21) | (1 << 16) | (1 << 11) | (1 << 6) | 0x28 | E;
        let sq = (0x01 << 25) | (0xF << 21) | (1 << 11);
        let vu = run(&[(add, sq), (NOP, LNOP)], |vu| vu.vf[1] = f([1.0; 4]));
        assert_eq!(vu.read_q(0), f([1.0; 4]));
        assert_eq!(vu.vf[1], f([2.0; 4]));
    }

    /// An instruction reading a VF register an FMAC wrote less than four
    /// cycles before stalls, and Q's latency counts the stall: a MULQ four
    /// instructions after a DIV gets its Q when a stall came between
    /// (Tekken 5's glyphs: the old Q put their first vertex at the far
    /// plane).
    #[test]
    fn stalls_count_toward_q() {
        let div = (0x40 << 25) | (2 << 16) | (1 << 11) | ((0x38 >> 2) << 6) | 0x3C;
        let add = |fd: u32, fs: u32, ft: u32| (0xF << 21) | (ft << 16) | (fs << 11) | (fd << 6) | 0x28;
        let mulq = |fd: u32, fs: u32| (0xF << 21) | (fs << 11) | (fd << 6) | 0x1C;
        for (reads_vf3, want) in [(true, 0.5), (false, 2.0)] {
            // ADD vf5 = vf3 + vf3 (stalls three cycles), or vf4 + vf4.
            let second = if reads_vf3 { add(5, 3, 3) } else { add(5, 4, 4) };
            let vu = run(
                &[
                    (NOP, div),
                    (add(3, 4, 4), LNOP),
                    (second, LNOP),
                    (NOP, LNOP),
                    (mulq(6, 7) | E, LNOP),
                    (NOP, LNOP),
                ],
                |vu| {
                    vu.vf[1] = f([1.0; 4]);
                    vu.vf[2] = f([4.0; 4]);
                    vu.vf[7] = f([2.0; 4]);
                    vu.q = 1f32.to_bits();
                },
            );
            assert_eq!(vu.vf[6], f([want; 4]), "stall: {reads_vf3}");
        }
    }

    /// In macro mode (the EE's COP2), a VMULQ right after a VDIV reads the
    /// division's result: the EE interlocks.
    #[test]
    fn macro_divisions_land_at_once() {
        let mut vu = Vu::new(0);
        vu.vf[1] = f([1.0; 4]);
        vu.vf[2] = f([4.0; 4]);
        vu.vf[7] = f([2.0; 4]);
        vu.q = 1f32.to_bits();
        // VDIV Q, vf1x, vf2x; VMULQ.xyzw vf6, vf7.
        let div = (2 << 16) | (1 << 11) | ((0x38 >> 2) << 6) | 0x3C;
        vu.special2(0x38, div, None);
        vu.macro_upper((0xF << 21) | (7 << 11) | (6 << 6) | 0x1C);
        assert_eq!(vu.vf[6], f([0.5; 4]));
    }

    #[test]
    fn integer_loop_and_division() {
        // vi1 = 3; loop: vi1 -= 1, vi2 += 2 until vi1 == 0; then DIV Q = 1/4.
        let iaddiu = |it: u32, is: u32, imm: u32| (0x08 << 25) | (it << 16) | (is << 11) | imm;
        let isubiu = |it: u32, is: u32, imm: u32| (0x09 << 25) | (it << 16) | (is << 11) | imm;
        let ibne = |it: u32, is: u32, off: i32| (0x29 << 25) | (it << 16) | (is << 11) | (off as u32 & 0x7FF);
        // DIV Q, vf1x, vf2x.
        // DIV: special op 0x38, as 0x3C with 0x38 >> 2 in bits 6-10.
        let div = (0x40 << 25) | (2 << 16) | (1 << 11) | ((0x38 >> 2) << 6) | 0x3C;
        let waitq = (0x40 << 25) | (0x3B & 3) | ((0x3B >> 2) << 6) | 0x3C;
        let vu = run(
            &[
                (NOP, iaddiu(1, 0, 3)),
                (NOP, isubiu(1, 1, 1)),
                (NOP, iaddiu(2, 2, 2)),
                (NOP, ibne(1, 0, -3)),
                (NOP, LNOP),
                (NOP, div),
                (NOP | E, waitq),
                (NOP, LNOP),
            ],
            |vu| {
                vu.vf[1] = f([1.0, 0.0, 0.0, 0.0]);
                vu.vf[2] = f([4.0, 0.0, 0.0, 0.0]);
            },
        );
        assert_eq!(vu.vi[1], 0);
        assert_eq!(vu.vi[2], 6);
        assert_eq!(f32::from_bits(vu.q), 0.25);
    }
}
