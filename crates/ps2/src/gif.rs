//! The GIF: the GS's front end. Packets come from three paths: VU1's
//! XGKICK (path 1), VIF1's DIRECT (path 2) and DMA channel 2 or its FIFO
//! (path 3). Each starts with a GIF tag (a count of loops, a format and a
//! list of registers); the data that follows becomes GS register writes.
//! The paths keep their own state, since a packet may span transfers.

use crate::gs::{Gs, reg};
use crate::vu::Vu;

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Path {
    /// Loops left in the current packet (0: a tag is expected).
    nloop: u32,
    nreg: u32,
    regs: u64,
    format: u32,
    /// The register within the loop.
    index: u32,
    eop: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Gif {
    paths: [Path; 3],
    /// Q of the last ST in packed mode, for the next RGBAQ.
    q: u32,
    /// Packets processed, for diagnostics.
    pub packets: [u64; 3],
    /// Quadwords received, by path, for diagnostics.
    #[serde(skip)]
    pub qwords: [u64; 3],
    /// When set (diagnostics): the writes to FRAME_1 and XYOFFSET_1, with
    /// their path and how many commands the GS had recorded before.
    #[serde(skip)]
    pub context_log: Option<Vec<(usize, u8, u64, usize)>>,
    #[serde(skip)]
    path: usize,
    /// Diagnostics: a vertex position (x, y) to look for, and the paths it
    /// came through.
    #[serde(skip)]
    pub find: Option<(u32, u32)>,
    #[serde(skip)]
    pub found: Vec<usize>,
    /// In a `Vu1Engine`: the quadwords received, recorded rather than
    /// sent to the GS (their tags are still read, for where an XGKICK's
    /// packet ends).
    #[serde(skip)]
    pub(crate) capture: Option<crate::vu1q::Stream>,
}

impl Gif {
    /// Whether the path is between packets.
    pub fn idle(&self, path: usize) -> bool {
        self.paths[path].nloop == 0
    }

    /// One quadword on a path (0, 1, 2 for paths 1, 2, 3).
    pub fn write(&mut self, path: usize, qword: u128, gs: &mut Gs) {
        self.path = path;
        self.qwords[path] += 1;
        if let Some(c) = self.capture.as_mut() {
            c.path(path, qword);
            Self::skip(&mut self.paths[path], qword);
            return;
        }
        let p = &mut self.paths[path];
        if p.nloop == 0 {
            self.packets[path] += 1;
            if let Some(prim) = Self::tag(p, qword) {
                gs.write_reg(reg::PRIM, prim);
            }
            return;
        }
        match p.format {
            0 => {
                let desc = ((p.regs >> (4 * p.index)) & 15) as u8;
                Self::advance(p);
                self.packed(desc, qword, gs);
            }
            1 => {
                // REGLIST: two registers per quadword.
                for half in 0..2 {
                    let p = &mut self.paths[path];
                    if p.nloop == 0 {
                        break;
                    }
                    let desc = ((p.regs >> (4 * p.index)) & 15) as u8;
                    Self::advance(p);
                    let data = (qword >> (64 * half)) as u64;
                    if desc != 0xE && desc != 0xF {
                        gs.write_reg(desc, data);
                    }
                }
            }
            _ => {
                // IMAGE: transfer data.
                p.nloop -= 1;
                gs.write_reg(reg::HWREG, qword as u64);
                gs.write_reg(reg::HWREG, (qword >> 64) as u64);
            }
        }
    }

    /// A tag: the packet's loops, format and registers; the PRIM it sets,
    /// if any.
    fn tag(p: &mut Path, qword: u128) -> Option<u64> {
        let lo = qword as u64;
        p.nloop = (lo & 0x7FFF) as u32;
        p.eop = lo & 0x8000 != 0;
        p.format = ((lo >> 58) & 3) as u32;
        p.nreg = match ((lo >> 60) & 15) as u32 {
            0 => 16,
            n => n,
        };
        p.regs = (qword >> 64) as u64;
        p.index = 0;
        (lo & (1 << 46) != 0).then_some((lo >> 47) & 0x7FF)
    }

    /// Where a quadword leaves its path, as `write` would, writing nothing.
    fn skip(p: &mut Path, qword: u128) {
        if p.nloop == 0 {
            Self::tag(p, qword);
            return;
        }
        match p.format {
            0 => Self::advance(p),
            1 => {
                for _ in 0..2 {
                    if p.nloop == 0 {
                        break;
                    }
                    Self::advance(p);
                }
            }
            _ => p.nloop -= 1,
        }
    }

    /// An XGKICK's packet has ended: path 1 starts afresh.
    pub fn end_kick(&mut self) {
        self.paths[0] = Path::default();
    }

    fn advance(p: &mut Path) {
        p.index += 1;
        if p.index >= p.nreg {
            p.index = 0;
            p.nloop -= 1;
        }
    }

    /// A PACKED register write.
    fn packed(&mut self, desc: u8, q: u128, gs: &mut Gs) {
        let w = |k: u32| ((q >> (32 * k)) as u32) as u64;
        if matches!(desc, 4 | 5) && self.find == Some(((w(0) & 0xFFFF) as u32, (w(1) & 0xFFFF) as u32)) {
            self.found.push(self.path);
        }
        match desc {
            0x0 => gs.write_reg(reg::PRIM, w(0) & 0x7FF),
            0x1 => {
                let rgba = (w(0) & 0xFF) | (w(1) & 0xFF) << 8 | (w(2) & 0xFF) << 16 | (w(3) & 0xFF) << 24;
                gs.write_reg(reg::RGBAQ, rgba | u64::from(self.q) << 32);
            }
            0x2 => {
                self.q = w(2) as u32;
                gs.write_reg(reg::ST, w(0) | w(1) << 32);
            }
            0x3 => gs.write_reg(reg::UV, (w(0) & 0x3FFF) | (w(1) & 0x3FFF) << 16),
            0x4 => {
                let adc = w(3) & (1 << 15) != 0;
                let v = (w(0) & 0xFFFF)
                    | (w(1) & 0xFFFF) << 16
                    | ((w(2) >> 4) & 0xFF_FFFF) << 32
                    | ((w(3) >> 4) & 0xFF) << 56;
                gs.write_reg(if adc { reg::XYZF3 } else { reg::XYZF2 }, v);
            }
            0x5 => {
                let adc = w(3) & (1 << 15) != 0;
                let v = (w(0) & 0xFFFF) | (w(1) & 0xFFFF) << 16 | w(2) << 32;
                gs.write_reg(if adc { reg::XYZ3 } else { reg::XYZ2 }, v);
            }
            0xA => gs.write_reg(reg::FOG, ((w(3) >> 4) & 0xFF) << 56),
            0xE => {
                let r = (w(2) & 0xFF) as u8;
                if let Some(log) = self.context_log.as_mut()
                    && matches!(r, reg::FRAME_1 | reg::XYOFFSET_1)
                {
                    log.push((self.path, r, q as u64, gs.recorded()));
                }
                gs.write_reg(r, q as u64);
            }
            0xF => {}
            _ => gs.write_reg(desc, q as u64),
        }
    }

    /// XGKICK: path 1, from VU1's memory at `addr` (in quadwords), until
    /// the end of a packet marked as the last (EOP).
    /// Sends the packet at `addr` of VU1's memory: returns its quadwords.
    pub fn kick_path1(&mut self, vu: &Vu, addr: u32, gs: &mut Gs) -> u32 {
        let mut a = addr;
        for _ in 0..vu.data.len() / 16 * 4 {
            let w = vu.read_q(a);
            a = a.wrapping_add(1);
            let q = w
                .iter()
                .enumerate()
                .fold(0u128, |acc, (k, &x)| acc | u128::from(x) << (32 * k));
            self.write(0, q, gs);
            let p = &self.paths[0];
            if p.nloop == 0 && p.eop {
                break;
            }
        }
        if let Some(c) = self.capture.as_mut() {
            c.kick_end();
        }
        self.end_kick();
        a.wrapping_sub(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_sprite() {
        let mut gs = Gs::default();
        gs.write_reg(reg::FRAME_1, 1 << 16);
        gs.write_reg(reg::SCISSOR_1, (63 << 16) | (63 << 48));
        gs.write_reg(reg::PRMODECONT, 1);
        let mut gif = Gif::default();
        // NLOOP 1, EOP, PRE with PRIM = sprite, 3 registers: RGBAQ, XYZ2, XYZ2.
        let tag = 1 | (1 << 15) | (1 << 46) | (6u128 << 47) | (3u128 << 60) | ((0x551u128) << 64);
        gif.write(2, tag, &mut gs);
        gif.write(2, 0x40 << 96 | 0x30 << 64 | 0x20 << 32 | 0x10, &mut gs);
        gif.write(2, (16 << 32) | 16, &mut gs);
        gif.write(2, (48 << 32) | 48, &mut gs);
        assert!(gif.idle(2));
        assert_eq!(gs.mem.read(0, 0, 1, 1, 1), 0x4030_2010);
        assert_eq!(gs.mem.read(0, 0, 1, 3, 3), 0);
    }
}
