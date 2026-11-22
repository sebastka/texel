//! The VIFs: the vector units' interfaces. A stream of 32-bit words: VIF
//! codes, each with its data. UNPACK writes vectors to VU memory, MPG
//! loads microprograms, MSCAL starts them, and VIF1 passes DIRECT data to
//! the GIF (path 2). VIF1 double-buffers VU1's memory (BASE, OFFSET,
//! TOPS).
//!
//! Microprograms run to their end when started.

use crate::gif::Gif;
use crate::gs::Gs;
use crate::vu::Vu;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Pending {
    None,
    /// STMASK, STROW, STCOL: words left.
    Mask,
    Row(u32),
    Col(u32),
    /// MPG: words left, and where the next one goes (in bytes).
    Mpg(u32, u32),
    /// DIRECT: words left (sent four at a time).
    Direct(u32),
    /// UNPACK: vectors left.
    Unpack(u32),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Vif {
    pub index: usize,
    pub cl: u32,
    pub wl: u32,
    pub mode: u32,
    pub mask: u32,
    pub row: [u32; 4],
    pub col: [u32; 4],
    pub itops: u32,
    pub base: u32,
    pub ofst: u32,
    pub tops: u32,
    pub dbf: bool,
    pub mark: u32,
    code: u32,
    pending: Pending,
    /// The words of a quadword for DIRECT, and of a vector for UNPACK.
    buffer: Vec<u32>,
    /// Bits left over between UNPACK input words.
    bytes: Vec<u8>,
    /// UNPACK: destination (quadwords), vectors written in this unpack.
    addr: u32,
    written: u32,
    /// An interrupt was asked for (a code with its IRQ bit).
    pub irq: bool,
    /// Microprograms started, for diagnostics.
    pub calls: u64,
}

impl Vif {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            cl: 1,
            wl: 1,
            mode: 0,
            mask: 0,
            row: [0; 4],
            col: [0; 4],
            itops: 0,
            base: 0,
            ofst: 0,
            tops: 0,
            dbf: false,
            mark: 0,
            code: 0,
            pending: Pending::None,
            buffer: Vec::with_capacity(4),
            bytes: Vec::with_capacity(16),
            addr: 0,
            written: 0,
            irq: false,
            calls: 0,
        }
    }

    /// Feeds words to the VIF.
    pub fn write(&mut self, words: &[u32], vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
        for &w in words {
            self.word(w, vu, gif, gs);
        }
    }

    fn word(&mut self, w: u32, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
        match self.pending {
            Pending::None => self.command(w, vu, gif, gs),
            Pending::Mask => {
                self.mask = w;
                self.pending = Pending::None;
            }
            Pending::Row(left) => {
                self.row[(4 - left) as usize] = w;
                self.pending = if left > 1 {
                    Pending::Row(left - 1)
                } else {
                    Pending::None
                };
            }
            Pending::Col(left) => {
                self.col[(4 - left) as usize] = w;
                self.pending = if left > 1 {
                    Pending::Col(left - 1)
                } else {
                    Pending::None
                };
            }
            Pending::Mpg(left, at) => {
                let at = at as usize % vu.micro.len();
                vu.micro[at..at + 4].copy_from_slice(&w.to_le_bytes());
                self.pending = if left > 1 {
                    Pending::Mpg(left - 1, at as u32 + 4)
                } else {
                    Pending::None
                };
            }
            Pending::Direct(left) => {
                self.buffer.push(w);
                if self.buffer.len() == 4 {
                    let q = self
                        .buffer
                        .iter()
                        .enumerate()
                        .fold(0u128, |a, (k, &x)| a | u128::from(x) << (32 * k));
                    self.buffer.clear();
                    gif.write(1, q, gs);
                }
                self.pending = if left > 1 {
                    Pending::Direct(left - 1)
                } else {
                    Pending::None
                };
            }
            Pending::Unpack(_) => self.unpack_word(w, vu),
        }
    }

    fn command(&mut self, w: u32, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
        self.code = w;
        let cmd = (w >> 24) & 0x7F;
        let num = (w >> 16) & 0xFF;
        let imm = w & 0xFFFF;
        if w & (1 << 31) != 0 {
            self.irq = true;
        }
        match cmd {
            0x00 => {}
            0x01 => {
                self.cl = imm & 0xFF;
                self.wl = imm >> 8;
            }
            0x02 => {
                self.ofst = imm & 0x3FF;
                self.dbf = false;
                self.tops = self.base;
            }
            0x03 => self.base = imm & 0x3FF,
            0x04 => self.itops = imm & 0x3FF,
            0x05 => self.mode = imm & 3,
            0x07 => self.mark = imm,
            // MSCAL, MSCALF; MSCNT.
            0x14 | 0x15 => self.start(imm, vu, gif, gs),
            0x17 => self.start(vu.pc, vu, gif, gs),
            0x20 => self.pending = Pending::Mask,
            0x30 => self.pending = Pending::Row(4),
            0x31 => self.pending = Pending::Col(4),
            0x4A => {
                let n = if num == 0 { 256 } else { num };
                self.pending = Pending::Mpg(n * 2, imm * 8);
            }
            0x50 | 0x51 => {
                let n = if imm == 0 { 65536 } else { imm };
                self.buffer.clear();
                self.pending = Pending::Direct(n * 4);
            }
            0x60..=0x7F => {
                let n = if num == 0 { 256 } else { num };
                let flg = imm & 0x8000 != 0 && self.index == 1;
                self.addr = ((imm & 0x3FF) + if flg { self.tops } else { 0 }) & 0x3FF;
                self.written = 0;
                self.bytes.clear();
                self.pending = Pending::Unpack(n);
                // Vectors that need no input (filling writes) go out now.
                self.unpack_fill(vu);
            }
            // NOP, MSKPATH3, FLUSHE, FLUSH, FLUSHA: nothing to wait for.
            _ => {}
        }
    }

    /// MSCAL/MSCNT: TOP and ITOP for the program, double buffering on VIF1,
    /// then the program runs.
    fn start(&mut self, pc: u32, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) {
        vu.itop = self.itops as u16;
        if self.index == 1 {
            vu.top = self.tops as u16;
            self.dbf = !self.dbf;
            // A 10-bit register: VU1's memory is 1024 quadwords.
            self.tops = (self.base + if self.dbf { self.ofst } else { 0 }) & 0x3FF;
        }
        self.calls += 1;
        vu.run(pc, gif, gs);
    }

    // --- UNPACK ------------------------------------------------------------------

    /// The unpack's format: elements per vector and bits per element
    /// (V4-5: 4 elements in 16 bits).
    fn format(&self) -> (u32, u32) {
        let cmd = (self.code >> 24) & 0x7F;
        let vl = cmd & 3;
        let vn = (cmd >> 2) & 3;
        if vn == 3 && vl == 3 { (4, 5) } else { (vn + 1, 32 >> vl) }
    }

    fn vectors_left(&self) -> u32 {
        match self.pending {
            Pending::Unpack(n) => n,
            _ => 0,
        }
    }

    /// Whether the next vector takes input: in filling mode (WL > CL),
    /// only the first CL of each WL do.
    fn needs_data(&self) -> bool {
        self.wl <= self.cl || self.written % self.wl.max(1) < self.cl
    }

    fn unpack_word(&mut self, w: u32, vu: &mut Vu) {
        self.bytes.extend_from_slice(&w.to_le_bytes());
        let (elements, bits) = self.format();
        let vector_bytes = if bits == 5 { 2 } else { (elements * bits / 8) as usize };
        while self.vectors_left() > 0 && self.needs_data() && self.bytes.len() >= vector_bytes {
            let data: Vec<u8> = self.bytes.drain(..vector_bytes).collect();
            let signed = self.code & (1 << 14) == 0;
            let get = |k: usize| -> u32 {
                match bits {
                    32 => u32::from_le_bytes(data[4 * k..4 * k + 4].try_into().unwrap_or_default()),
                    16 => {
                        let v = u16::from_le_bytes([data[2 * k], data[2 * k + 1]]);
                        if signed { v as i16 as i32 as u32 } else { u32::from(v) }
                    }
                    8 => {
                        if signed {
                            data[k] as i8 as i32 as u32
                        } else {
                            u32::from(data[k])
                        }
                    }
                    _ => {
                        let v = u32::from(u16::from_le_bytes([data[0], data[1]]));
                        [
                            (v & 0x1F) << 3,
                            ((v >> 5) & 0x1F) << 3,
                            ((v >> 10) & 0x1F) << 3,
                            (v >> 15) << 7,
                        ][k]
                    }
                }
            };
            let values: [Option<u32>; 4] = match elements {
                // A scalar fills the four lanes; V2 and V3 leave the rest.
                1 => [Some(get(0)); 4],
                2 => [Some(get(0)), Some(get(1)), None, None],
                3 => [Some(get(0)), Some(get(1)), Some(get(2)), None],
                _ => [Some(get(0)), Some(get(1)), Some(get(2)), Some(get(3))],
            };
            self.write_vector(values, vu);
            // Filling vectors after this one, if any.
            self.unpack_fill(vu);
        }
        if self.vectors_left() == 0 {
            // The rest of the last word is padding.
            self.bytes.clear();
            self.pending = Pending::None;
        }
    }

    /// Filling writes: vectors with no input, from the row and column
    /// registers through the mask.
    fn unpack_fill(&mut self, vu: &mut Vu) {
        while self.vectors_left() > 0 && !self.needs_data() {
            self.write_vector([None; 4], vu);
        }
        if self.vectors_left() == 0 {
            self.pending = Pending::None;
        }
    }

    /// Writes a vector (masked, with the row added or accumulated) at the
    /// next address.
    fn write_vector(&mut self, data: [Option<u32>; 4], vu: &mut Vu) {
        let wl = self.wl.max(1);
        let cycle = self.written % wl;
        let dest = if self.wl <= self.cl {
            self.addr + (self.written / wl) * self.cl + cycle
        } else {
            self.addr + self.written
        };
        let masked = (self.code >> 24) & 0x10 != 0;
        let row = cycle.min(3);
        let mut out = vu.read_q(dest);
        for k in 0..4 {
            let m = if masked {
                (self.mask >> (row * 8 + 2 * k as u32)) & 3
            } else {
                0
            };
            let value = match (m, data[k]) {
                (0, Some(v)) => Some(match self.mode {
                    1 => v.wrapping_add(self.row[k]),
                    2 => {
                        self.row[k] = self.row[k].wrapping_add(v);
                        self.row[k]
                    }
                    _ => v,
                }),
                (0, None) => None,
                (1, _) => Some(self.row[k]),
                (2, _) => Some(self.col[row as usize]),
                _ => None,
            };
            if let Some(v) = value {
                out[k] = v;
            }
        }
        vu.write_q(dest, out, 0xF);
        self.written += 1;
        if let Pending::Unpack(n) = self.pending {
            self.pending = Pending::Unpack(n - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(vif: &mut Vif, vu: &mut Vu, words: &[u32]) {
        let mut gif = Gif::default();
        let mut gs = Gs::default();
        vif.write(words, vu, &mut gif, &mut gs);
    }

    #[test]
    fn unpack_v4_32_and_s_16() {
        let mut vif = Vif::new(1);
        let mut vu = Vu::new(1);
        // STCYCL 1,1; UNPACK V4-32, 2 vectors at 10.
        feed(&mut vif, &mut vu, &[0x0100_0101, 0x6C02_000A, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(vu.read_q(10), [1, 2, 3, 4]);
        assert_eq!(vu.read_q(11), [5, 6, 7, 8]);
        // UNPACK S-16 (signed), 3 vectors at 20: 3 halfwords, one padding.
        feed(&mut vif, &mut vu, &[0x6103_0014, 0xFFFF_0001, 0x0000_0002]);
        assert_eq!(vu.read_q(20), [1; 4]);
        assert_eq!(vu.read_q(21), [u32::MAX; 4]);
        assert_eq!(vu.read_q(22), [2; 4]);
        // The VIF takes commands again.
        feed(&mut vif, &mut vu, &[0x0500_0001]);
        assert_eq!(vif.mode, 1);
    }

    #[test]
    fn double_buffers_wrap_at_the_end_of_memory() {
        let mut vif = Vif::new(1);
        let mut vu = Vu::new(1);
        // A program that ends at once: NOP with the E bit, then NOP.
        vu.micro[..8].copy_from_slice(&0x4000_02FF_8000_033Cu64.to_le_bytes());
        vu.micro[8..16].copy_from_slice(&0x0000_02FF_8000_033Cu64.to_le_bytes());
        // BASE 0x200, OFFSET 0x200: the buffers are 0x200 and 0x400, which
        // TOP, ten bits, wraps to 0.
        feed(&mut vif, &mut vu, &[0x0300_0200, 0x0200_0200, 0x1400_0000]);
        assert_eq!(vu.top, 0x200);
        feed(&mut vif, &mut vu, &[0x1400_0000]);
        assert_eq!(vu.top, 0);
    }

    #[test]
    fn skipping_and_masked_filling() {
        let mut vif = Vif::new(1);
        let mut vu = Vu::new(1);
        // CL 2, WL 1: every other quadword.
        feed(&mut vif, &mut vu, &[0x0100_0102, 0x6C02_0000, 1, 1, 1, 1, 2, 2, 2, 2]);
        assert_eq!(vu.read_q(0), [1; 4]);
        assert_eq!(vu.read_q(2), [2; 4]);
        // CL 1, WL 2 with the mask taking the second vector from the row.
        feed(
            &mut vif,
            &mut vu,
            &[
                0x3000_0000,
                9,
                9,
                9,
                9,
                0x2000_0000,
                0x0000_5500,
                0x0100_0201,
                0x7C02_0010,
                3,
                3,
                3,
                3,
            ],
        );
        assert_eq!(vu.read_q(16), [3; 4]);
        assert_eq!(vu.read_q(17), [9; 4]);
    }
}
