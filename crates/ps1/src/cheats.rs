//! Cheat and patch codes, in the GameShark format DuckStation's database
//! uses (its `cheat-format.md`): lines of `AAAAAAAA VVVV[VVVV]`, the first
//! byte the operation, the rest an address in RAM (`$80XXXXXX`) or
//! operands. Codes run once a frame (at its end, or its start), in order;
//! conditionals skip the next code, block conditionals the codes up to an
//! `00000000 FFFF` line. Some write with restore: when such a code is
//! turned off, what it replaced is put back.
//!
//! Codes change what the console computes (they write its memory): a
//! console runs the same with the same codes, enabled at the same frames.

use crate::bus::Bus;

/// When a code runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    /// At the end of each frame (the start of the vertical blank).
    EndFrame,
    /// At the start of each frame.
    StartFrame,
    /// Once, when it is enabled.
    Manual,
}

/// A code: its name (what identifies it), its lines, when it runs.
#[derive(Clone, Debug)]
pub struct Code {
    pub name: String,
    lines: Vec<(u32, u32)>,
    pub activation: Activation,
    /// Frames it ran (`C1`: activate after a delay).
    frames: u32,
}

impl Code {
    /// A code from its body (`?` options substituted already); an error
    /// says what is not supported.
    pub fn parse(name: &str, body: &str, activation: Activation) -> Result<Self, String> {
        let mut lines = Vec::new();
        for raw in body.lines() {
            let line = raw.split(['#', ';']).next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(a), Some(v), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(format!("not a code line: {raw}"));
            };
            if a.len() != 8 || v.is_empty() || v.len() > 8 {
                return Err(format!("not a code line: {raw}"));
            }
            let a = u32::from_str_radix(a, 16).map_err(|_| format!("not hexadecimal: {raw}"))?;
            let v = u32::from_str_radix(v, 16).map_err(|_| format!("not hexadecimal: {raw}"))?;
            lines.push((a, v));
        }
        if lines.is_empty() {
            return Err("no code lines".into());
        }
        // Every operation known, with as many lines as it takes.
        let mut i = 0;
        while i < lines.len() {
            i += length(&lines, i)?;
        }
        if i != lines.len() {
            return Err("a code's lines run past its end".into());
        }
        Ok(Self {
            name: name.to_owned(),
            lines,
            activation,
            frames: 0,
        })
    }
}

/// How many lines the code at `i` takes, if it is one known.
fn length(lines: &[(u32, u32)], i: usize) -> Result<usize, String> {
    let (a, v) = lines[i];
    let op = a >> 24;
    Ok(match op {
        0x00 if a == 0 && v & 0xffff == 0xffff => 1,
        0x10 | 0x11 | 0x1f | 0x20 | 0x21 | 0x30..=0x32 | 0x60 | 0x61 | 0x80..=0x82 | 0x90..=0x92 => 1,
        0xa0..=0xa8 | 0xc0 | 0xc1 | 0xc3..=0xc6 | 0xd0..=0xd4 | 0xd7 | 0xe0..=0xe3 | 0xf0..=0xf2 | 0xf5 => 1,
        0x51 | 0x52 => 1,
        0x50 | 0x53 | 0xc2 | 0xf3 => 2,
        0xf4 => 5,
        0xa9 => 1 + v.div_ceil(2) as usize,
        0xaa | 0xab => 1 + v as usize,
        _ => return Err(format!("code type {op:02X} is not supported")),
    })
}

/// What the codes read of the controller: port 1's buttons (the
/// controller protocol's bits, 1: pressed) and sticks.
#[derive(Clone, Copy, Default)]
pub struct Pad {
    pub buttons: u16,
    pub axes: [u8; 4],
}

impl Pad {
    /// The buttons as `D4` codes give them: the protocol's bytes swapped.
    fn joker(self) -> u32 {
        u32::from(self.buttons.rotate_left(8))
    }

    /// The buttons and sticks as `D7` codes give them.
    fn bit_joker(self) -> u32 {
        let [lx, ly, rx, ry] = self.axes;
        let dir = |v: u8, low: u32, high: u32| {
            if v < 0x50 {
                low
            } else if v > 0xb0 {
                high
            } else {
                0
            }
        };
        self.joker()
            | dir(ry, 0x01_0000, 0x04_0000)
            | dir(rx, 0x08_0000, 0x02_0000)
            | dir(ly, 0x10_0000, 0x40_0000)
            | dir(lx, 0x80_0000, 0x20_0000)
    }
}

/// The codes enabled, and the cheat registers they share.
#[derive(Default)]
pub struct Cheats {
    codes: Vec<Code>,
    regs: Vec<u32>,
}

impl Cheats {
    /// The codes enabled from now on, in order: those no longer among them
    /// put back what they replaced (`A7`, `A8`, `AA`), and new `Manual`
    /// ones run once.
    pub fn set(&mut self, codes: Vec<Code>, bus: &mut Bus, pad: Pad) {
        let old = std::mem::take(&mut self.codes);
        for code in old.iter().filter(|o| !codes.iter().any(|c| c.name == o.name)) {
            restore(&code.lines, bus);
        }
        self.codes = codes;
        if self.regs.is_empty() {
            self.regs = vec![0; 256];
        }
        for k in 0..self.codes.len() {
            let manual = self.codes[k].activation == Activation::Manual;
            if manual && !old.iter().any(|o| o.name == self.codes[k].name) {
                let lines = std::mem::take(&mut self.codes[k].lines);
                run(&lines, bus, pad, &mut self.regs, u32::MAX);
                self.codes[k].lines = lines;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// What running the codes changes (their registers, the frames each
    /// ran), for rollback to go back to (see `crate::rewind`).
    pub(crate) fn save(&self, out: &mut Vec<u32>) {
        out.clear();
        out.extend_from_slice(&self.regs);
        out.extend(self.codes.iter().map(|c| c.frames));
    }

    /// Back to what `save` kept, with the same codes.
    pub(crate) fn restore(&mut self, saved: &[u32]) {
        if saved.len() != self.regs.len() + self.codes.len() {
            return;
        }
        let (regs, frames) = saved.split_at(self.regs.len());
        self.regs.copy_from_slice(regs);
        for (code, &f) in self.codes.iter_mut().zip(frames) {
            code.frames = f;
        }
    }

    /// Runs the codes with this activation.
    pub fn run(&mut self, activation: Activation, bus: &mut Bus, pad: Pad) {
        for code in &mut self.codes {
            if code.activation == activation {
                code.frames = code.frames.saturating_add(1);
                run(&code.lines, bus, pad, &mut self.regs, code.frames);
            }
        }
    }
}

/// RAM's address of a code's `XXXXXX`, in the CPU's address space.
fn ram(addr: u32) -> u32 {
    0x8000_0000 | (addr & 0x00ff_ffff)
}

fn r8(bus: &mut Bus, a: u32) -> u32 {
    if plain(a) { u32::from(bus.load8(a)) } else { 0 }
}

fn r16(bus: &mut Bus, a: u32) -> u32 {
    if plain(a) { u32::from(bus.load16(a & !1)) } else { 0 }
}

fn r32(bus: &mut Bus, a: u32) -> u32 {
    if plain(a) { bus.load32(a & !3) } else { 0 }
}

fn w8(bus: &mut Bus, a: u32, v: u32) {
    if plain(a) {
        bus.store8(a, v as u8);
    }
}

fn w16(bus: &mut Bus, a: u32, v: u32) {
    if plain(a) {
        bus.store16(a & !1, v as u16);
    }
}

fn w32(bus: &mut Bus, a: u32, v: u32) {
    if plain(a) {
        bus.store32(a & !3, v);
    }
}

/// Whether codes may touch an address: RAM and the scratchpad only (not
/// I/O ports, whose reads and writes do things).
fn plain(a: u32) -> bool {
    let p = a & 0x1fff_ffff;
    p < 0x0080_0000 || (0x1f80_0000..0x1f80_0400).contains(&p)
}

/// Skips the code at `i` and those its block holds, to after its
/// `00000000 FFFF` line (or the end).
fn block_end(lines: &[(u32, u32)], mut i: usize) -> usize {
    while i < lines.len() {
        if lines[i] == (0, 0xffff) || lines[i].0 == 0 && lines[i].1 & 0xffff == 0xffff {
            return i + 1;
        }
        i += length(lines, i).unwrap_or(1);
    }
    i
}

/// Runs a code's lines (`frames`: how many frames it ran, for `C1`).
fn run(lines: &[(u32, u32)], bus: &mut Bus, pad: Pad, regs: &mut [u32], frames: u32) {
    let mut i = 0;
    while i < lines.len() {
        let (a, v) = lines[i];
        let op = a >> 24;
        let x = ram(a);
        let n = length(lines, i).unwrap_or(1);
        // One-line conditionals: skip the next code when false.
        let cond = match op {
            0xa0 => Some(r32(bus, x) == v),
            0xa1 => Some(r32(bus, x) != v),
            0xa2 => Some(r32(bus, x) < v),
            0xa3 => Some(r32(bus, x) > v),
            0xd0 => Some(r16(bus, x) == v & 0xffff),
            0xd1 => Some(r16(bus, x) != v & 0xffff),
            0xd2 => Some(r16(bus, x) < v & 0xffff),
            0xd3 => Some(r16(bus, x) > v & 0xffff),
            0xd4 => Some(pad.joker() == v & 0xffff),
            0xe0 => Some(r8(bus, x) == v & 0xff),
            0xe1 => Some(r8(bus, x) != v & 0xff),
            0xe2 => Some(r8(bus, x) < v & 0xff),
            0xe3 => Some(r8(bus, x) > v & 0xff),
            _ => None,
        };
        if let Some(c) = cond {
            i += 1;
            if !c && i < lines.len() {
                i += length(lines, i).unwrap_or(1);
            }
            continue;
        }
        // Block conditionals: skip to the block's end when false.
        let block = match op {
            0xa4 => Some(r32(bus, x) == v),
            0xc0 => Some(r16(bus, x) == v & 0xffff),
            0xc3 => Some(r8(bus, x) < v & 0xff),
            0xc4 => Some(r8(bus, x) > v & 0xff),
            0xc5 => Some(r16(bus, x) < v & 0xffff),
            0xc6 => Some(r16(bus, x) > v & 0xffff),
            0xd7 => Some(bit_joker(a, v, pad, regs)),
            0x52 => Some(register_condition(a, v, bus, regs)),
            _ => None,
        };
        if let Some(c) = block {
            i = if c { i + 1 } else { block_end(lines, i + 1) };
            continue;
        }
        match op {
            // The end of a block.
            0x00 => {}
            0x90 => w32(bus, x, v),
            0x91 => {
                let cur = r32(bus, x);
                w32(bus, x, cur | v);
            }
            0x92 => {
                let cur = r32(bus, x);
                w32(bus, x, cur & !v);
            }
            0x80 => w16(bus, x, v),
            0x81 => {
                let cur = r16(bus, x);
                w16(bus, x, cur | v);
            }
            0x82 => {
                let cur = r16(bus, x);
                w16(bus, x, cur & !v);
            }
            0x30 => w8(bus, x, v),
            0x31 => {
                let cur = r8(bus, x);
                w8(bus, x, cur | v);
            }
            0x32 => {
                let cur = r8(bus, x);
                w8(bus, x, cur & !v);
            }
            0x10 => {
                let cur = r16(bus, x);
                w16(bus, x, cur.wrapping_add(v));
            }
            0x11 => {
                let cur = r16(bus, x);
                w16(bus, x, cur.wrapping_sub(v));
            }
            0x20 => {
                let cur = r8(bus, x);
                w8(bus, x, cur.wrapping_add(v));
            }
            0x21 => {
                let cur = r8(bus, x);
                w8(bus, x, cur.wrapping_sub(v));
            }
            0x60 => {
                let cur = r32(bus, x);
                w32(bus, x, cur.wrapping_add(v));
            }
            0x61 => {
                let cur = r32(bus, x);
                w32(bus, x, cur.wrapping_sub(v));
            }
            0x1f => w16(bus, 0x1f80_0000 | (a & 0x3ff), v),
            0xa5 => w32(bus, 0x1f80_0000 | (a & 0x3ff), v),
            0xa6 | 0xa7 => {
                if r16(bus, x) == v >> 16 {
                    w16(bus, x, v & 0xffff);
                }
            }
            0xa8 => {
                if r8(bus, x) == (v >> 8) & 0xff {
                    w8(bus, x, v & 0xff);
                }
            }
            0xf0 => {
                let (hh, ss, ul, ll) = (v >> 24, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff);
                let cur = r8(bus, x);
                if cur < ll {
                    w8(bus, x, ss);
                } else if cur > ul {
                    w8(bus, x, hh);
                }
            }
            0xf1 | 0xf2 => {
                let (ul, ll) = (v >> 16, v & 0xffff);
                let cur = r16(bus, x);
                let (low, high) = if op == 0xf1 { (ll, ul) } else { (ul, ll) };
                if cur < ll {
                    w16(bus, x, low);
                } else if cur > ul {
                    w16(bus, x, high);
                }
            }
            0xf3 => {
                let (ul, ll) = (v >> 16, v & 0xffff);
                let (hh, ss) = (lines[i + 1].1 >> 16, lines[i + 1].1 & 0xffff);
                let cur = r16(bus, x);
                if cur < ll {
                    w16(bus, x, ss);
                } else if cur > ul {
                    w16(bus, x, hh);
                }
            }
            0xf5 => {
                let (yy, zz) = (v >> 16, v & 0xffff);
                let cur = r16(bus, x);
                if cur == yy {
                    w16(bus, x, zz);
                } else if cur == zz {
                    w16(bus, x, yy);
                }
            }
            0xf4 => find_replace(&lines[i..i + 5], bus),
            0xc1 => {
                // Activate the following codes after a delay (frames).
                if frames < v & 0xffff {
                    return;
                }
            }
            0xc2 => {
                let to = ram(lines[i + 1].0);
                for k in 0..(v & 0xffff) {
                    let b = r8(bus, x.wrapping_add(k));
                    w8(bus, to.wrapping_add(k), b);
                }
            }
            0x50 => {
                let (count, step, vstep) = ((a >> 8) & 0xff, a & 0xff, v & 0xffff);
                let (next_a, next_v) = lines[i + 1];
                let byte = next_a >> 24 == 0x30;
                for k in 0..count {
                    let at = ram(next_a).wrapping_add(k * step);
                    let val = next_v.wrapping_add(k * vstep);
                    if byte {
                        w8(bus, at, val);
                    } else {
                        w16(bus, at, val);
                    }
                }
            }
            0x53 => {
                let (down_a, down_v, count) = ((a >> 20) & 0xf != 0, (a >> 16) & 0xf != 0, a & 0xffff);
                let (step, vstep) = (v >> 16, v & 0xffff);
                let (next_a, next_v) = lines[i + 1];
                for k in 0..count {
                    let at = if down_a {
                        ram(next_a).wrapping_sub(k * step)
                    } else {
                        ram(next_a).wrapping_add(k * step)
                    };
                    let val = if down_v {
                        next_v.wrapping_sub(k * vstep)
                    } else {
                        next_v.wrapping_add(k * vstep)
                    };
                    match next_a >> 24 {
                        0x30 => w8(bus, at, val),
                        0x80 => w16(bus, at, val),
                        _ => w32(bus, at, val),
                    }
                }
            }
            0xa9 => {
                for k in 0..v {
                    let line = lines[i + 1 + k as usize / 2];
                    w32(bus, x.wrapping_add(4 * k), if k % 2 == 0 { line.0 } else { line.1 });
                }
            }
            0xaa | 0xab => {
                let data = &lines[i + 1..i + 1 + v as usize];
                if data
                    .iter()
                    .enumerate()
                    .all(|(k, &(from, _))| r32(bus, x.wrapping_add(4 * k as u32)) == from)
                {
                    for (k, &(_, to)) in data.iter().enumerate() {
                        w32(bus, x.wrapping_add(4 * k as u32), to);
                    }
                }
            }
            0x51 => register_op(a, v, bus, regs),
            _ => {}
        }
        i += n;
    }
}

/// What a code turned off puts back: `A7`, `A8`, `AA` codes' originals,
/// where their replacements still are.
fn restore(lines: &[(u32, u32)], bus: &mut Bus) {
    let mut i = 0;
    while i < lines.len() {
        let (a, v) = lines[i];
        let x = ram(a);
        match a >> 24 {
            0xa7 => {
                if r16(bus, x) == v & 0xffff {
                    w16(bus, x, v >> 16);
                }
            }
            0xa8 => {
                if r8(bus, x) == v & 0xff {
                    w8(bus, x, (v >> 8) & 0xff);
                }
            }
            0xaa => {
                let data = &lines[i + 1..i + 1 + v as usize];
                if data
                    .iter()
                    .enumerate()
                    .all(|(k, &(_, to))| r32(bus, x.wrapping_add(4 * k as u32)) == to)
                {
                    for (k, &(from, _)) in data.iter().enumerate() {
                        w32(bus, x.wrapping_add(4 * k as u32), from);
                    }
                }
            }
            _ => {}
        }
        i += length(lines, i).unwrap_or(1);
    }
}

/// `F4`: finds 16 bytes (a wildcard byte matching any) around an address,
/// and replaces them (but where the replacement is the wildcard).
fn find_replace(lines: &[(u32, u32)], bus: &mut Bus) {
    let (a, v) = lines[0];
    let wild = (v >> 16) & 0xff;
    let size = (v & 0xffff) * 4;
    let bytes = |l: &[(u32, u32)]| -> Vec<u32> {
        l.iter()
            .flat_map(|&(p, q)| [p, q])
            .flat_map(|w| [w >> 24, (w >> 16) & 0xff, (w >> 8) & 0xff, w & 0xff])
            .collect()
    };
    let (find, replace) = (bytes(&lines[1..3]), bytes(&lines[3..5]));
    let centre = ram(a & 0x00ff_fffe);
    let start = centre.wrapping_sub(size / 2);
    for off in 0..size {
        let at = start.wrapping_add(off);
        if find
            .iter()
            .enumerate()
            .all(|(k, &f)| f == wild || r8(bus, at.wrapping_add(k as u32)) == f)
        {
            for (k, &r) in replace.iter().enumerate() {
                if r != wild {
                    w8(bus, at.wrapping_add(k as u32), r);
                }
            }
            return;
        }
    }
}

/// `D7PQRRRR TTYYYYYY`: the buttons and sticks in `YYYYYY` all on (`P`:
/// 0) or all off (1), held (`Q`: counted in register `TT`) for exactly,
/// at least, less than, or other than `RRRR` frames.
fn bit_joker(a: u32, v: u32, pad: Pad, regs: &mut [u32]) -> bool {
    let (p, q, count) = ((a >> 20) & 0xf, (a >> 16) & 0xf, a & 0xffff);
    let (reg, bits) = ((v >> 24) as usize, v & 0x00ff_ffff);
    let held = pad.bit_joker() & bits;
    let on = if p == 0 { held == bits } else { held == 0 };
    if reg != 0 {
        regs[reg] = if on { regs[reg].wrapping_add(1) } else { 0 };
    }
    let frames = if reg != 0 { regs[reg] } else { u32::from(on) };
    on && match q {
        1 => frames == count,
        2 => frames >= count,
        3 => frames < count,
        4 => frames != count,
        _ => true,
    }
}

/// Reading and writing memory at a width.
type Read = fn(&mut Bus, u32) -> u32;
type Write = fn(&mut Bus, u32, u32);

/// `51`: the cheat registers' operations.
fn register_op(a: u32, v: u32, bus: &mut Bus, regs: &mut [u32]) {
    let sub = (a >> 16) & 0xff;
    let (yy, xx) = (((a >> 8) & 0xff) as usize, (a & 0xff) as usize);
    // The width: 8 (0x0_), 16 (0x4_) or 32 bits (0x8_), and the operation.
    let (width, opn) = (sub & 0xf0, sub & 0x0f);
    let (mask, read, write): (u32, Read, Write) = match width {
        0x00 => (0xff, r8, w8),
        0x40 => (0xffff, r16, w16),
        0x80 => (u32::MAX, r32, w32),
        _ => (0, r32, w32),
    };
    if mask != 0 {
        match opn {
            0 => write(bus, v, regs[xx] & mask),
            1 => regs[xx] = read(bus, v),
            2 => write(bus, regs[xx], v & mask),
            3 => regs[xx] = (regs[yy] & mask).wrapping_add(v) & mask,
            4 => write(bus, regs[xx], (regs[yy] & mask).wrapping_add(v) & mask),
            5 => regs[xx] = v & mask,
            6 => regs[xx] = read(bus, regs[yy].wrapping_add(v)),
            7 => {
                // An element of an array of arrays: registers XX..XX+3 hold
                // the base, the stride, the index chosen, a check (D0D0).
                let (index, value) = (v >> 16, v & mask);
                let (base, stride, chosen, check) = (
                    regs[xx],
                    regs[(xx + 1) % 256],
                    regs[(xx + 2) % 256],
                    regs[(xx + 3) % 256],
                );
                if check == 0xd0d0 {
                    let at = ram(base)
                        .wrapping_add(stride.wrapping_mul(chosen.wrapping_sub(1)))
                        .wrapping_add(index);
                    write(bus, at, value);
                }
            }
            _ => {}
        }
        return;
    }
    let rr = (v & 0xff) as usize;
    match sub {
        0xc0 => regs[rr] = regs[yy].wrapping_add(regs[xx]),
        0xc1 => regs[rr] = regs[yy].wrapping_sub(regs[xx]),
        0xc2 => regs[rr] = regs[yy].wrapping_mul(regs[xx]),
        0xc3 => regs[rr] = regs[yy].checked_div(regs[xx]).unwrap_or(0),
        0xc4 => regs[rr] = regs[yy].checked_rem(regs[xx]).unwrap_or(regs[yy]),
        0xc5 => regs[rr] = regs[yy] & regs[xx],
        0xc6 => regs[rr] = regs[yy] | regs[xx],
        0xc7 => regs[rr] = regs[yy] ^ regs[xx],
        0xc8 => regs[rr] = !regs[xx],
        0xc9 => regs[rr] = regs[xx].wrapping_shl(yy as u32),
        0xca => regs[rr] = regs[xx].wrapping_shr(yy as u32),
        0xd0 => regs[xx] = regs[yy] & v,
        0xd1 => regs[xx] = regs[yy] | v,
        0xd2 => regs[xx] = regs[yy] ^ v,
        _ => {}
    }
}

/// `52`: the cheat registers' block conditionals.
fn register_condition(a: u32, v: u32, bus: &mut Bus, regs: &[u32]) -> bool {
    let sub = (a >> 16) & 0xff;
    let (yy, xx) = (((a >> 8) & 0xff) as usize, (a & 0xff) as usize);
    // Width (bits 6-7: 8, 16, 32 bits), then the kind of operands (bits
    // 4-5), then the comparison (bits 0-3).
    let (mask, read): (u32, fn(&mut Bus, u32) -> u32) = match sub & 0xc0 {
        0x00 => (0xff, r8),
        0x40 => (0xffff, r16),
        _ => (u32::MAX, r32),
    };
    let kind = (sub >> 4) & 0x3;
    let cmp = sub & 0xf;
    let (left, right, upper) = match kind {
        // RegYY against RegXX.
        0 => (regs[yy] & mask, regs[xx] & mask, 0),
        // RegXX against a value (and an upper bound, for ranges).
        1 => (regs[xx] & mask, v & mask, (v >> 16) & mask),
        // (RegYY) against (RegXX), or (RegXX) against a value.
        2 => {
            if matches!(cmp, 6..=0xb) {
                (read(bus, regs[xx]) & mask, v & mask, (v >> 16) & mask)
            } else {
                (read(bus, regs[yy]) & mask, read(bus, regs[xx]) & mask, 0)
            }
        }
        // RegXX against (TTTTTTTT).
        _ => (regs[xx] & mask, read(bus, v) & mask, 0),
    };
    match cmp {
        0 => left == right,
        1 => left != right,
        2 => left > right,
        3 => left >= right,
        4 => left < right,
        5 => left <= right,
        6 => left & right == right,
        7 => left & right != right,
        8 => left > right && left < upper,
        9 => left >= right && left <= upper,
        0xa => left & right == left,
        0xb => left & right != left,
        _ => false,
    }
}

/// DuckStation's code files (`.cht`): codes in `[Name]` sections, each
/// with its `Key = Value` metadata, then its lines.
pub mod file {
    use super::Activation;

    /// A code as a file gives it.
    #[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct FileCode {
        /// Its name, `\` separating groups ("Group\Code").
        pub name: String,
        pub description: Option<String>,
        pub author: Option<String>,
        /// `EndFrame` (the default), `StartFrame` or `Manual`.
        pub activation: String,
        /// `Gameshark` (the default) or `Assembly` (not supported).
        pub kind: String,
        /// Choices for its `?` digits: named values, or a range.
        pub options: Vec<(String, u32)>,
        pub range: Option<(u32, u32)>,
        /// Settings it needs (patches only).
        pub overrides: Overrides,
        /// Its lines, `?` digits unfilled.
        pub body: String,
    }

    /// What a patch asks of the emulator while it is enabled.
    #[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Overrides {
        /// The display's aspect ratio, e.g. "16:9".
        pub aspect_ratio: Option<String>,
        /// The CPU's speed, in percent.
        pub cpu_overclock: Option<u32>,
        /// RAM, in MiB (8: development consoles').
        pub ram_mib: Option<u32>,
        /// No widescreen rendering by the GTE (the patch does its own).
        pub disable_widescreen: bool,
    }

    impl FileCode {
        pub fn activation(&self) -> Activation {
            match self.activation.as_str() {
                "StartFrame" => Activation::StartFrame,
                "Manual" => Activation::Manual,
                _ => Activation::EndFrame,
            }
        }

        /// Its lines with the `?` digits filled with `value`.
        pub fn filled(&self, value: u32) -> String {
            fill(&self.body, value)
        }
    }

    /// A number as codes' metadata write them: decimal, or `0x` hex, `0b`
    /// binary.
    fn number(s: &str) -> Option<u32> {
        let s = s.trim();
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u32::from_str_radix(h, 16).ok()
        } else if let Some(b) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
            u32::from_str_radix(b, 2).ok()
        } else {
            s.parse().ok()
        }
    }

    /// Fills each line's run of `?` digits with `value`, in hexadecimal,
    /// as many digits as there are `?`.
    pub fn fill(body: &str, value: u32) -> String {
        body.lines()
            .map(|line| {
                let Some(start) = line.find('?') else {
                    return line.to_owned();
                };
                let width = line[start..].chars().take_while(|&c| c == '?').count();
                let mask = if width >= 8 { u32::MAX } else { (1 << (4 * width)) - 1 };
                format!("{}{:0width$X}{}", &line[..start], value & mask, &line[start + width..])
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The codes of a file, in order (those marked `Ignore` left out).
    pub fn parse(text: &str) -> Vec<FileCode> {
        let mut out = Vec::new();
        let mut current: Option<(FileCode, bool)> = None;
        let finish = |c: Option<(FileCode, bool)>, out: &mut Vec<FileCode>| {
            if let Some((mut code, ignore)) = c
                && !ignore
            {
                if code.activation.is_empty() {
                    code.activation = "EndFrame".into();
                }
                if code.kind.is_empty() {
                    code.kind = "Gameshark".into();
                }
                code.body = code.body.trim_end().to_owned();
                out.push(code);
            }
        };
        for raw in text.lines() {
            let line = raw.trim();
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                finish(current.take(), &mut out);
                current = Some((
                    FileCode {
                        name: name.to_owned(),
                        ..FileCode::default()
                    },
                    false,
                ));
                continue;
            }
            let Some((code, ignore)) = &mut current else { continue };
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            // Metadata, before the lines or after them (code lines have
            // no `=`).
            if let Some((key, value)) = line.split_once('=')
                && key.trim().chars().all(|c| c.is_ascii_alphanumeric())
            {
                let value = value.trim();
                let yes = value.eq_ignore_ascii_case("true");
                match key.trim() {
                    "Type" => code.kind = value.to_owned(),
                    "Activation" => code.activation = value.to_owned(),
                    "Description" => code.description = Some(value.to_owned()),
                    "Author" => code.author = Some(value.to_owned()),
                    "Ignore" => *ignore = yes,
                    "Option" => {
                        if let Some((name, v)) = value.rsplit_once(':')
                            && let Some(v) = number(v)
                        {
                            code.options.push((name.trim().to_owned(), v));
                        }
                    }
                    "OptionRange" => {
                        if let Some((lo, hi)) = value.split_once(':') {
                            code.range = number(lo).zip(number(hi));
                        }
                    }
                    "OverrideAspectRatio" => code.overrides.aspect_ratio = Some(value.to_owned()),
                    "OverrideCPUOverclock" => code.overrides.cpu_overclock = number(value),
                    "DisableWidescreenRendering" => code.overrides.disable_widescreen = yes,
                    "Enable8MBRAM" if yes => code.overrides.ram_mib = Some(8),
                    "Enable16MBRAM" if yes => code.overrides.ram_mib = Some(16),
                    "OverrideRAMSize" => {
                        code.overrides.ram_mib = value.trim_end_matches("Forced").trim_end_matches("MB").parse().ok();
                    }
                    _ => {}
                }
                continue;
            }
            code.body.push_str(raw.trim());
            code.body.push('\n');
        }
        finish(current.take(), &mut out);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_file_and_its_options() {
            let text = "# A game\n[Group\\Lives]\nType = Gameshark\nOptionRange = 1:0x63\nAuthor = Someone\n80001234 00??\n\n[Fast]\nOverrideCPUOverclock = 200\nEnable8MBRAM = True\nA7001000 10002000\n";
            let codes = parse(text);
            assert_eq!(codes.len(), 2);
            assert_eq!(codes[0].name, "Group\\Lives");
            assert_eq!(codes[0].range, Some((1, 99)));
            assert_eq!(codes[0].filled(0x42), "80001234 0042");
            assert_eq!(codes[1].overrides.cpu_overclock, Some(200));
            assert_eq!(codes[1].overrides.ram_mib, Some(8));
            assert_eq!(codes[1].activation(), Activation::EndFrame);
        }

        /// Every code of a DuckStation database (`TEXEL_CHTDB=dir`, with
        /// `cheats/` and `patches/`), its first choice filled in, is
        /// supported but the kinds the interpreter refuses (counted).
        #[test]
        #[ignore = "needs a database"]
        fn a_whole_database() {
            let Ok(dir) = std::env::var("TEXEL_CHTDB") else { return };
            let mut counts = std::collections::BTreeMap::<String, usize>::new();
            let (mut total, mut refused) = (0, 0);
            for kind in ["patches", "cheats"] {
                for entry in std::fs::read_dir(format!("{dir}/{kind}")).expect("the directory") {
                    let text = std::fs::read_to_string(entry.expect("an entry").path()).unwrap_or_default();
                    for code in parse(&text) {
                        total += 1;
                        let value = code
                            .options
                            .first()
                            .map(|o| o.1)
                            .or(code.range.map(|r| r.0))
                            .unwrap_or(0);
                        if let Err(e) = super::super::Code::parse(&code.name, &code.filled(value), code.activation()) {
                            refused += 1;
                            *counts.entry(e.chars().take(40).collect()).or_default() += 1;
                        }
                    }
                }
            }
            let mut sorted: Vec<_> = counts.into_iter().collect();
            sorted.sort_by_key(|c| std::cmp::Reverse(c.1));
            println!(
                "{refused} of {total} codes refused: {:#?}",
                &sorted[..sorted.len().min(25)]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> Bus {
        Bus::new(vec![0; 512 * 1024].into_boxed_slice())
    }

    fn code(body: &str) -> Code {
        Code::parse("test", body, Activation::EndFrame).expect("a code")
    }

    #[test]
    fn writes_and_conditionals() {
        let mut bus = bus();
        let mut c = Cheats::default();
        c.set(
            vec![code(
                "80001000 1234\n30001002 0056\nD0001000 1234\n90001004 CAFEBABE\nD0001000 9999\n90001008 DEADBEEF",
            )],
            &mut bus,
            Pad::default(),
        );
        c.run(Activation::EndFrame, &mut bus, Pad::default());
        assert_eq!(bus.load16(0x8000_1000), 0x1234);
        assert_eq!(bus.load8(0x8000_1002), 0x56);
        assert_eq!(bus.load32(0x8000_1004), 0xcafe_babe);
        // The second conditional was false: the next code skipped.
        assert_eq!(bus.load32(0x8000_1008), 0);
    }

    #[test]
    fn block_conditionals_skip_to_their_end() {
        let mut bus = bus();
        let mut c = Cheats::default();
        c.set(
            vec![code(
                "C0002000 0001\n80002004 1111\n80002006 2222\n00000000 FFFF\n80002008 3333",
            )],
            &mut bus,
            Pad::default(),
        );
        c.run(Activation::EndFrame, &mut bus, Pad::default());
        assert_eq!(bus.load16(0x8000_2004), 0);
        assert_eq!(bus.load16(0x8000_2008), 0x3333);
    }

    #[test]
    fn restores_when_turned_off() {
        let mut bus = bus();
        bus.store16(0x8000_3000, 0x1000);
        let mut c = Cheats::default();
        c.set(vec![code("A7003000 10002000")], &mut bus, Pad::default());
        c.run(Activation::EndFrame, &mut bus, Pad::default());
        assert_eq!(bus.load16(0x8000_3000), 0x2000);
        c.set(Vec::new(), &mut bus, Pad::default());
        assert_eq!(bus.load16(0x8000_3000), 0x1000);
    }

    #[test]
    fn block_writes() {
        let mut bus = bus();
        let mut c = Cheats::default();
        c.set(
            vec![code("A9010000 00000003\n3C028001 8C420010\n24420001 00000000")],
            &mut bus,
            Pad::default(),
        );
        c.run(Activation::EndFrame, &mut bus, Pad::default());
        assert_eq!(bus.load32(0x8001_0000), 0x3c02_8001);
        assert_eq!(bus.load32(0x8001_0004), 0x8c42_0010);
        assert_eq!(bus.load32(0x8001_0008), 0x2442_0001);
        assert_eq!(bus.load32(0x8001_000c), 0);
    }

    #[test]
    fn unsupported_codes_are_refused() {
        assert!(Code::parse("x", "D5000000 0001", Activation::EndFrame).is_err());
        assert!(Code::parse("x", "A9010000 00000004\n00000000 00000000", Activation::EndFrame).is_err());
        assert!(Code::parse("x", "8000?000 0001", Activation::EndFrame).is_err());
    }
}
