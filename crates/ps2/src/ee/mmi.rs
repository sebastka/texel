//! The R5900's multimedia instructions (MMI): SIMD on the 128-bit
//! registers, in 8, 16, 32 or 64-bit lanes, and the second multiply
//! pipeline (MULT1, DIV1...).

use super::{Ee, div_signed, div_unsigned, sext32};

fn words(v: u128) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}

fn from_words(w: [u32; 4]) -> u128 {
    w.iter()
        .enumerate()
        .fold(0, |acc, (i, &x)| acc | (u128::from(x) << (32 * i)))
}

fn halves(v: u128) -> [u16; 8] {
    std::array::from_fn(|i| (v >> (16 * i)) as u16)
}

fn from_halves(h: [u16; 8]) -> u128 {
    h.iter()
        .enumerate()
        .fold(0, |acc, (i, &x)| acc | (u128::from(x) << (16 * i)))
}

fn bytes(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

fn from_bytes(b: [u8; 16]) -> u128 {
    u128::from_le_bytes(b)
}

fn dwords(v: u128) -> [u64; 2] {
    [v as u64, (v >> 64) as u64]
}

fn from_dwords(d: [u64; 2]) -> u128 {
    u128::from(d[0]) | (u128::from(d[1]) << 64)
}

fn map_w(a: u128, b: u128, f: impl Fn(u32, u32) -> u32) -> u128 {
    let (a, b) = (words(a), words(b));
    from_words(std::array::from_fn(|i| f(a[i], b[i])))
}

fn map_h(a: u128, b: u128, f: impl Fn(u16, u16) -> u16) -> u128 {
    let (a, b) = (halves(a), halves(b));
    from_halves(std::array::from_fn(|i| f(a[i], b[i])))
}

fn map_b(a: u128, b: u128, f: impl Fn(u8, u8) -> u8) -> u128 {
    let (a, b) = (bytes(a), bytes(b));
    from_bytes(std::array::from_fn(|i| f(a[i], b[i])))
}

fn mask<T: From<u8> + std::ops::Not<Output = T>>(c: bool) -> T {
    if c { !T::from(0) } else { T::from(0) }
}

/// Interleaves the lower (or upper) halves of two registers' lanes:
/// rt's lane first, then rs's.
fn interleave<const N: usize, T: Copy + Default>(rs: [T; N], rt: [T; N], upper: bool) -> [T; N] {
    let base = if upper { N / 2 } else { 0 };
    std::array::from_fn(|i| if i % 2 == 0 { rt[base + i / 2] } else { rs[base + i / 2] })
}

/// Packs the even lanes of rt (low half) then rs (high half).
fn pack<const N: usize, T: Copy + Default>(rs: [T; N], rt: [T; N]) -> [T; N] {
    std::array::from_fn(|i| if i < N / 2 { rt[2 * i] } else { rs[2 * (i - N / 2)] })
}

fn shuffle_h(v: u128, order: [usize; 8]) -> u128 {
    let h = halves(v);
    from_halves(std::array::from_fn(|i| h[order[i]]))
}

fn shuffle_w(v: u128, order: [usize; 4]) -> u128 {
    let w = words(v);
    from_words(std::array::from_fn(|i| w[order[i]]))
}

/// Leading bits equal to the sign bit, minus one (PLZCW).
fn leading_sign_bits(v: u32) -> u32 {
    let v = if v & 0x8000_0000 != 0 { !v } else { v };
    v.leading_zeros().saturating_sub(1)
}

impl Ee {
    pub(super) fn mmi(&mut self, i: u32, rs: u32, rt: u32, rd: u32, sa: u32) {
        let s = self.gpr[rs as usize];
        let t = self.gpr[rt as usize];
        match i & 0x3F {
            // MADD, MADDU: (HI, LO) += rs × rt; rd = LO.
            0x00 | 0x01 | 0x20 | 0x21 => {
                let pipe1 = i & 0x20 != 0;
                let (lo, hi) = if pipe1 {
                    ((self.lo >> 64) as u64, (self.hi >> 64) as u64)
                } else {
                    (self.lo as u64, self.hi as u64)
                };
                let acc = (u64::from(hi as u32) << 32) | u64::from(lo as u32);
                let p = if i & 1 == 0 {
                    (i64::from(s as u32 as i32) * i64::from(t as u32 as i32)) as u64
                } else {
                    u64::from(s as u32) * u64::from(t as u32)
                };
                self.set_mult(rd, acc.wrapping_add(p), pipe1);
            }
            0x04 => {
                let w = words(s);
                let v = u128::from(leading_sign_bits(w[0])) | (u128::from(leading_sign_bits(w[1])) << 32);
                self.set64(rd, v as u64);
            }
            0x08 => self.mmi0(i, rd, s, t),
            0x09 => self.mmi2(i, rs, rd, s, t),
            0x10 => self.set64(rd, (self.hi >> 64) as u64),
            0x11 => self.set_hi1(s as u64),
            0x12 => self.set64(rd, (self.lo >> 64) as u64),
            0x13 => self.set_lo1(s as u64),
            0x18 => {
                let p = i64::from(s as u32 as i32) * i64::from(t as u32 as i32);
                self.set_mult(rd, p as u64, true);
            }
            0x19 => {
                let p = u64::from(s as u32) * u64::from(t as u32);
                self.set_mult(rd, p, true);
            }
            0x1A | 0x1B => {
                let (q, r) = if i & 1 == 0 {
                    div_signed(s as u32, t as u32)
                } else {
                    div_unsigned(s as u32, t as u32)
                };
                self.set_lo1(sext32(q));
                self.set_hi1(sext32(r));
            }
            0x28 => self.mmi1(i, rd, s, t),
            0x29 => self.mmi3(i, rs, rd, s, t),
            0x30 => self.pmfhl(sa, rd),
            0x31 => {
                // PMTHL.LW
                if sa == 0 {
                    let w = words(s);
                    let (lo, hi) = (words(self.lo), words(self.hi));
                    self.lo = from_words([w[0], lo[1], w[2], lo[3]]);
                    self.hi = from_words([w[1], hi[1], w[3], hi[3]]);
                } else {
                    self.unimplemented("PMTHL");
                }
            }
            0x34 => self.set128(rd, from_halves(halves(t).map(|h| h << (sa & 15)))),
            0x36 => self.set128(rd, from_halves(halves(t).map(|h| h >> (sa & 15)))),
            0x37 => self.set128(rd, from_halves(halves(t).map(|h| ((h as i16) >> (sa & 15)) as u16))),
            0x3C => self.set128(rd, from_words(words(t).map(|w| w << sa))),
            0x3E => self.set128(rd, from_words(words(t).map(|w| w >> sa))),
            0x3F => self.set128(rd, from_words(words(t).map(|w| ((w as i32) >> sa) as u32))),
            _ => self.unimplemented("MMI ?"),
        }
    }

    fn pmfhl(&mut self, fmt: u32, rd: u32) {
        let (lo, hi) = (words(self.lo), words(self.hi));
        let v = match fmt {
            // LW, UW: the lower (upper) words of LO and HI, interleaved.
            0 => from_words([lo[0], hi[0], lo[2], hi[2]]),
            1 => from_words([lo[1], hi[1], lo[3], hi[3]]),
            // SLW: HI:LO as 64-bit values, saturated to 32 bits.
            2 => {
                let sat = |h: u32, l: u32| {
                    let v = ((u64::from(h) << 32) | u64::from(l)) as i64;
                    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as u64
                };
                from_dwords([sat(hi[0], lo[0]), sat(hi[2], lo[2])])
            }
            // LH: the lower halves of each word.
            3 => {
                let (l, h) = (halves(self.lo), halves(self.hi));
                from_halves([l[0], l[2], h[0], h[2], l[4], l[6], h[4], h[6]])
            }
            // SH: each word saturated to 16 bits.
            4 => {
                let sat = |w: u32| (w as i32).clamp(-0x8000, 0x7FFF) as u16;
                from_halves([
                    sat(lo[0]),
                    sat(lo[1]),
                    sat(hi[0]),
                    sat(hi[1]),
                    sat(lo[2]),
                    sat(lo[3]),
                    sat(hi[2]),
                    sat(hi[3]),
                ])
            }
            _ => {
                self.unimplemented("PMFHL ?");
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi0(&mut self, i: u32, rd: u32, s: u128, t: u128) {
        let v = match (i >> 6) & 31 {
            0x00 => map_w(s, t, u32::wrapping_add),
            0x01 => map_w(s, t, u32::wrapping_sub),
            0x02 => map_w(s, t, |a, b| mask((a as i32) > (b as i32))),
            0x03 => map_w(s, t, |a, b| (a as i32).max(b as i32) as u32),
            0x04 => map_h(s, t, u16::wrapping_add),
            0x05 => map_h(s, t, u16::wrapping_sub),
            0x06 => map_h(s, t, |a, b| mask((a as i16) > (b as i16))),
            0x07 => map_h(s, t, |a, b| (a as i16).max(b as i16) as u16),
            0x08 => map_b(s, t, u8::wrapping_add),
            0x09 => map_b(s, t, u8::wrapping_sub),
            0x0A => map_b(s, t, |a, b| mask((a as i8) > (b as i8))),
            0x10 => map_w(s, t, |a, b| (a as i32).saturating_add(b as i32) as u32),
            0x11 => map_w(s, t, |a, b| (a as i32).saturating_sub(b as i32) as u32),
            0x12 => from_words(interleave(words(s), words(t), false)),
            0x13 => from_words(pack(words(s), words(t))),
            0x14 => map_h(s, t, |a, b| (a as i16).saturating_add(b as i16) as u16),
            0x15 => map_h(s, t, |a, b| (a as i16).saturating_sub(b as i16) as u16),
            0x16 => from_halves(interleave(halves(s), halves(t), false)),
            0x17 => from_halves(pack(halves(s), halves(t))),
            0x18 => map_b(s, t, |a, b| (a as i8).saturating_add(b as i8) as u8),
            0x19 => map_b(s, t, |a, b| (a as i8).saturating_sub(b as i8) as u8),
            0x1A => from_bytes(interleave(bytes(s), bytes(t), false)),
            0x1B => from_bytes(pack(bytes(s), bytes(t))),
            _ => {
                self.unimplemented("MMI0 ?");
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi1(&mut self, i: u32, rd: u32, s: u128, t: u128) {
        let v = match (i >> 6) & 31 {
            0x01 => from_words(words(t).map(|w| (w as i32).saturating_abs() as u32)),
            0x02 => map_w(s, t, |a, b| mask(a == b)),
            0x03 => map_w(s, t, |a, b| (a as i32).min(b as i32) as u32),
            0x04 => {
                let (a, b) = (halves(s), halves(t));
                from_halves(std::array::from_fn(|i| {
                    if i < 4 {
                        a[i].wrapping_sub(b[i])
                    } else {
                        a[i].wrapping_add(b[i])
                    }
                }))
            }
            0x05 => from_halves(halves(t).map(|h| (h as i16).saturating_abs() as u16)),
            0x06 => map_h(s, t, |a, b| mask(a == b)),
            0x07 => map_h(s, t, |a, b| (a as i16).min(b as i16) as u16),
            0x0A => map_b(s, t, |a, b| mask(a == b)),
            0x10 => map_w(s, t, u32::saturating_add),
            0x11 => map_w(s, t, u32::saturating_sub),
            0x12 => from_words(interleave(words(s), words(t), true)),
            0x14 => map_h(s, t, u16::saturating_add),
            0x15 => map_h(s, t, u16::saturating_sub),
            0x16 => from_halves(interleave(halves(s), halves(t), true)),
            0x18 => map_b(s, t, u8::saturating_add),
            0x19 => map_b(s, t, u8::saturating_sub),
            0x1A => from_bytes(interleave(bytes(s), bytes(t), true)),
            0x1B => {
                // QFSRV: rs:rt shifted right by SA bytes.
                let shift = self.sa * 8;
                if shift == 0 {
                    t
                } else {
                    (t >> shift) | (s << (128 - shift))
                }
            }
            _ => {
                self.unimplemented("MMI1 ?");
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi2(&mut self, i: u32, rs: u32, rd: u32, s: u128, t: u128) {
        let v = match (i >> 6) & 31 {
            f @ (0x00 | 0x04) => {
                // PMADDW, PMSUBW: words 0 and 2's products, added to or
                // subtracted from HI:LO's.
                let (a, b) = (words(s), words(t));
                let (lo, hi) = (words(self.lo), words(self.hi));
                let mut out = [0u64; 2];
                let (mut new_lo, mut new_hi) = ([0u64; 2], [0u64; 2]);
                for k in 0..2 {
                    let acc = (i64::from(hi[2 * k]) << 32) | i64::from(lo[2 * k]);
                    let p = i64::from(a[2 * k] as i32) * i64::from(b[2 * k] as i32);
                    let r = if f == 0x00 {
                        acc.wrapping_add(p)
                    } else {
                        acc.wrapping_sub(p)
                    };
                    out[k] = r as u64;
                    new_lo[k] = sext32(r as u32);
                    new_hi[k] = sext32((r >> 32) as u32);
                }
                self.lo = from_dwords(new_lo);
                self.hi = from_dwords(new_hi);
                from_dwords(out)
            }
            f @ (0x10 | 0x14) => {
                // PMADDH, PMSUBH: the halves' products added to or subtracted
                // from LO and HI's words, laid out as PMULTH's.
                let (a, b) = (halves(s), halves(t));
                let p: [i32; 8] = std::array::from_fn(|k| i32::from(a[k] as i16) * i32::from(b[k] as i16));
                let (lo, hi) = (words(self.lo), words(self.hi));
                let op = |x: u32, y: i32| {
                    let v = if f == 0x10 {
                        (x as i32).wrapping_add(y)
                    } else {
                        (x as i32).wrapping_sub(y)
                    };
                    v as u32
                };
                let new_lo = [op(lo[0], p[0]), op(lo[1], p[1]), op(lo[2], p[4]), op(lo[3], p[5])];
                let new_hi = [op(hi[0], p[2]), op(hi[1], p[3]), op(hi[2], p[6]), op(hi[3], p[7])];
                self.lo = from_words(new_lo);
                self.hi = from_words(new_hi);
                from_words([new_lo[0], new_hi[0], new_lo[2], new_hi[2]])
            }
            f @ (0x11 | 0x15) => {
                // PHMADH, PHMSBH: pairs of products, summed (or the upper
                // less the lower), into LO and HI's words 0 and 2.
                let (a, b) = (halves(s), halves(t));
                let p: [i32; 8] = std::array::from_fn(|k| i32::from(a[k] as i16) * i32::from(b[k] as i16));
                let q: [u32; 4] = std::array::from_fn(|k| {
                    let v = if f == 0x11 {
                        p[2 * k + 1].wrapping_add(p[2 * k])
                    } else {
                        p[2 * k + 1].wrapping_sub(p[2 * k])
                    };
                    v as u32
                });
                let (mut lo, mut hi) = (words(self.lo), words(self.hi));
                lo[0] = q[0];
                hi[0] = q[1];
                lo[2] = q[2];
                hi[2] = q[3];
                self.lo = from_words(lo);
                self.hi = from_words(hi);
                from_words(q)
            }
            0x1D => {
                // PDIVBW: each word of rs by rt's lower half: quotients in LO,
                // remainders in HI.
                let a = words(s);
                let d = halves(t)[0] as i16 as i32 as u32;
                let (mut lo, mut hi) = ([0u32; 4], [0u32; 4]);
                for k in 0..4 {
                    (lo[k], hi[k]) = div_signed(a[k], d);
                }
                self.lo = from_words(lo);
                self.hi = from_words(hi);
                return;
            }
            0x02 | 0x03 => {
                // PSLLVW, PSRLVW: on words 0 and 2, sign-extended.
                let (a, b) = (words(s), words(t));
                let left = (i >> 6) & 31 == 0x02;
                let f = |x: u32, n: u32| if left { x << (n & 31) } else { x >> (n & 31) };
                from_dwords([sext32(f(b[0], a[0])), sext32(f(b[2], a[2]))])
            }
            0x08 => self.hi,
            0x09 => self.lo,
            0x0A => {
                // PINTH: rt's lower halves with rs's upper ones.
                let (a, b) = (halves(s), halves(t));
                from_halves(std::array::from_fn(
                    |i| if i % 2 == 0 { b[i / 2] } else { a[4 + i / 2] },
                ))
            }
            0x0C | 0x0D => {
                // PMULTW: words 0 and 2, 64-bit products.
                let (a, b) = (words(s), words(t));
                let mut out = [0u64; 2];
                let (mut lo, mut hi) = ([0u64; 2], [0u64; 2]);
                for k in 0..2 {
                    let p = i64::from(a[2 * k] as i32) * i64::from(b[2 * k] as i32);
                    out[k] = p as u64;
                    lo[k] = sext32(p as u32);
                    hi[k] = sext32((p >> 32) as u32);
                }
                if (i >> 6) & 31 == 0x0C {
                    self.lo = from_dwords(lo);
                    self.hi = from_dwords(hi);
                    from_dwords(out)
                } else {
                    // PDIVW
                    for k in 0..2 {
                        let (q, r) = div_signed(a[2 * k], b[2 * k]);
                        lo[k] = sext32(q);
                        hi[k] = sext32(r);
                    }
                    self.lo = from_dwords(lo);
                    self.hi = from_dwords(hi);
                    return;
                }
            }
            0x0E => from_dwords([dwords(t)[0], dwords(s)[0]]),
            0x12 => s & t,
            0x13 => s ^ t,
            0x1A => shuffle_h(t, [2, 1, 0, 3, 6, 5, 4, 7]),
            0x1B => shuffle_h(t, [3, 2, 1, 0, 7, 6, 5, 4]),
            0x1C => {
                // PMULTH
                let (a, b) = (halves(s), halves(t));
                let p: [u32; 8] = std::array::from_fn(|k| (i32::from(a[k] as i16) * i32::from(b[k] as i16)) as u32);
                self.lo = from_words([p[0], p[1], p[4], p[5]]);
                self.hi = from_words([p[2], p[3], p[6], p[7]]);
                from_words([p[0], p[2], p[4], p[6]])
            }
            0x1E => shuffle_w(t, [2, 1, 0, 3]),
            0x1F => shuffle_w(t, [1, 2, 0, 3]),
            _ => {
                let _ = rs;
                self.unimplemented("MMI2 ?");
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi3(&mut self, i: u32, rs: u32, rd: u32, s: u128, t: u128) {
        let v = match (i >> 6) & 31 {
            0x03 => {
                // PSRAVW
                let (a, b) = (words(s), words(t));
                let f = |x: u32, n: u32| ((x as i32) >> (n & 31)) as u32;
                from_dwords([sext32(f(b[0], a[0])), sext32(f(b[2], a[2]))])
            }
            0x08 => {
                self.hi = s;
                return;
            }
            0x09 => {
                self.lo = s;
                return;
            }
            0x0A => {
                // PINTEH: the even halves of rt and rs, interleaved.
                let (a, b) = (halves(s), halves(t));
                from_halves(std::array::from_fn(|i| if i % 2 == 0 { b[i] } else { a[i - 1] }))
            }
            0x0C => {
                // PMULTUW
                let (a, b) = (words(s), words(t));
                let mut out = [0u64; 2];
                let (mut lo, mut hi) = ([0u64; 2], [0u64; 2]);
                for k in 0..2 {
                    let p = u64::from(a[2 * k]) * u64::from(b[2 * k]);
                    out[k] = p;
                    lo[k] = sext32(p as u32);
                    hi[k] = sext32((p >> 32) as u32);
                }
                self.lo = from_dwords(lo);
                self.hi = from_dwords(hi);
                from_dwords(out)
            }
            0x0D => {
                // PDIVUW
                let (a, b) = (words(s), words(t));
                let (mut lo, mut hi) = ([0u64; 2], [0u64; 2]);
                for k in 0..2 {
                    let (q, r) = div_unsigned(a[2 * k], b[2 * k]);
                    lo[k] = sext32(q);
                    hi[k] = sext32(r);
                }
                self.lo = from_dwords(lo);
                self.hi = from_dwords(hi);
                return;
            }
            0x0E => from_dwords([dwords(s)[1], dwords(t)[1]]),
            0x12 => s | t,
            0x13 => !(s | t),
            0x1A => shuffle_h(t, [0, 2, 1, 3, 4, 6, 5, 7]),
            0x1B => shuffle_h(t, [0, 0, 0, 0, 4, 4, 4, 4]),
            0x1E => shuffle_w(t, [0, 2, 1, 3]),
            _ => {
                let _ = rs;
                self.unimplemented("MMI3 ?");
                return;
            }
        };
        self.set128(rd, v);
    }
}
