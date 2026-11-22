//! COP1: the R5900's FPU. Single precision only, and not IEEE 754: there
//! are no infinities, NaNs or denormals. Values with the largest exponent
//! are ordinary (huge) numbers, results that overflow saturate at ±max,
//! and denormal results are flushed to zero. Here, f32 arithmetic with
//! inputs and outputs clamped to match.

use super::{Ee, sext32};

/// FCR0: implementation and revision.
const FCR0: u32 = 0x2E30;
/// FCR31: the condition bit.
const C: u32 = 1 << 23;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Fpu {
    pub fpr: [u32; 32],
    pub acc: u32,
    pub fcr31: u32,
}

/// The PS2's view of a bit pattern: what IEEE calls infinity or NaN is the
/// largest magnitude; denormals are zero.
fn value(bits: u32) -> f32 {
    match bits & 0x7F80_0000 {
        0x7F80_0000 => f32::from_bits((bits & 0x8000_0000) | 0x7F7F_FFFF),
        0 => f32::from_bits(bits & 0x8000_0000),
        _ => f32::from_bits(bits),
    }
}

/// Clamps a result the way the PS2 does.
fn result(v: f32) -> u32 {
    let bits = v.to_bits();
    if v.is_nan() || v.is_infinite() {
        (bits & 0x8000_0000) | 0x7F7F_FFFF
    } else if bits & 0x7F80_0000 == 0 {
        bits & 0x8000_0000
    } else {
        bits
    }
}

impl Ee {
    pub(super) fn fpu_op(&mut self, i: u32, rs: u32, rt: u32, rd: u32, sa: u32) {
        let (ft, fs, fd) = (rt as usize, rd as usize, sa as usize);
        match rs {
            0x00 => self.set64(rt, sext32(self.fpu.fpr[fs])),
            0x02 => {
                let v = match rd {
                    0 => FCR0,
                    31 => self.fpu.fcr31,
                    _ => 0,
                };
                self.set64(rt, sext32(v));
            }
            0x04 => self.fpu.fpr[fs] = self.r32(rt),
            0x06 => {
                if rd == 31 {
                    self.fpu.fcr31 = self.r32(rt) & 0x0083_C078;
                }
            }
            0x08 => {
                let cond = self.fpu.fcr31 & C != 0;
                let taken = if rt & 1 == 0 { !cond } else { cond };
                self.cond_branch(taken, super::sext16(i as u16) as u32, rt & 2 != 0);
            }
            0x10 => self.fpu_single(i & 0x3F, ft, fs, fd),
            0x14 => {
                if i & 0x3F == 0x20 {
                    // CVT.S.W
                    self.fpu.fpr[fd] = result(self.fpu.fpr[fs] as i32 as f32);
                } else {
                    self.unimplemented("COP1.W ?");
                }
            }
            _ => self.unimplemented("COP1 ?"),
        }
    }

    fn fpu_single(&mut self, funct: u32, ft: usize, fs: usize, fd: usize) {
        let f = &mut self.fpu;
        let s = value(f.fpr[fs]);
        let t = value(f.fpr[ft]);
        let acc = value(f.acc);
        match funct {
            0x00 => f.fpr[fd] = result(s + t),
            0x01 => f.fpr[fd] = result(s - t),
            0x02 => f.fpr[fd] = result(s * t),
            0x03 => {
                f.fpr[fd] = if t == 0.0 {
                    ((f.fpr[fs] ^ f.fpr[ft]) & 0x8000_0000) | 0x7F7F_FFFF
                } else {
                    result(s / t)
                };
            }
            0x04 => f.fpr[fd] = result(t.abs().sqrt()),
            0x05 => f.fpr[fd] = f.fpr[fs] & 0x7FFF_FFFF,
            0x06 => f.fpr[fd] = f.fpr[fs],
            0x07 => f.fpr[fd] = f.fpr[fs] ^ 0x8000_0000,
            0x16 => {
                let r = t.abs().sqrt();
                f.fpr[fd] = if r == 0.0 { 0x7F7F_FFFF } else { result(s / r) };
            }
            0x18 => f.acc = result(s + t),
            0x19 => f.acc = result(s - t),
            0x1A => f.acc = result(s * t),
            0x1C => f.fpr[fd] = result(acc + s * t),
            0x1D => f.fpr[fd] = result(acc - s * t),
            0x1E => f.acc = result(acc + s * t),
            0x1F => f.acc = result(acc - s * t),
            0x24 => {
                // CVT.W.S: truncates, saturating.
                let v = s.trunc();
                f.fpr[fd] = if v >= 2_147_483_648.0 {
                    i32::MAX as u32
                } else if v < -2_147_483_648.0 {
                    i32::MIN as u32
                } else {
                    v as i32 as u32
                };
            }
            0x28 => f.fpr[fd] = result(s.max(t)),
            0x29 => f.fpr[fd] = result(s.min(t)),
            0x30 => f.fcr31 &= !C,
            0x32 | 0x34 | 0x36 => {
                let cond = match funct {
                    0x32 => s == t,
                    0x34 => s < t,
                    _ => s <= t,
                };
                f.fcr31 = (f.fcr31 & !C) | if cond { C } else { 0 };
            }
            _ => self.unimplemented("COP1.S ?"),
        }
    }
}
