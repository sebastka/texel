//! The VIFs: the vector units' interfaces. A stream of 32-bit words: VIF
//! codes, each with its data. UNPACK writes vectors to VU memory, MPG
//! loads microprograms, MSCAL starts them, and VIF1 passes DIRECT data to
//! the GIF (path 2). VIF1 double-buffers VU1's memory (BASE, OFFSET,
//! TOPS).
//!
//! Microprograms run to their end when started.
//!
//! A VIF reaches its VU through a `VuSink`: VIF0 acts on VU0 at once
//! (`Direct`), VIF1 queues for VU1 (`vu1q`), which runs when what it owns
//! is needed. The VIF never reads VU memory: a masked UNPACK writes the
//! lanes it sets only. With VU1 elsewhere, VIF1's words go there too, to a
//! VIF1 of its own: this one keeps only its registers and counts, its
//! UNPACKs reading no values unless they change its registers (MODE 2).

use crate::gif::Gif;
use crate::gs::Gs;
use crate::vu::Vu;

/// What a VIF does to its VU, and the GIF (VIF1's DIRECT).
pub trait VuSink {
    /// Sets the lanes given (x first) of data quadword `qaddr` (wrapped).
    fn write_lanes(&mut self, qaddr: u32, lanes: [Option<u32>; 4]);
    /// Micro memory's word at byte `at` (wrapped).
    fn write_micro(&mut self, at: u32, word: u32);
    fn micro_len(&self) -> u32;
    /// Starts a program at `pc` (None: where the last one stopped), with
    /// ITOP, and TOP when given (VIF1).
    fn start(&mut self, pc: Option<u32>, itop: u16, top: Option<u16>);
    /// A DIRECT quadword, to the GIF's path 2.
    fn direct(&mut self, q: u128);
    /// Whether UNPACK's values are wanted (else the VIF only counts its
    /// vectors: another VIF writes them).
    fn values(&self) -> bool {
        true
    }
}

/// Acting on a VU at once.
pub struct Direct<'a> {
    pub vu: &'a mut Vu,
    pub gif: &'a mut Gif,
    pub gs: &'a mut Gs,
}

impl VuSink for Direct<'_> {
    fn write_lanes(&mut self, qaddr: u32, lanes: [Option<u32>; 4]) {
        for (k, v) in lanes.iter().enumerate() {
            if let Some(v) = v {
                self.vu.write_lane(qaddr, k, *v);
            }
        }
    }

    fn write_micro(&mut self, at: u32, word: u32) {
        let at = at as usize % self.vu.micro.len();
        self.vu.micro[at..at + 4].copy_from_slice(&word.to_le_bytes());
        self.vu.micro_gen += 1;
    }

    fn micro_len(&self) -> u32 {
        self.vu.micro.len() as u32
    }

    fn start(&mut self, pc: Option<u32>, itop: u16, top: Option<u16>) {
        self.vu.itop = itop;
        if let Some(top) = top {
            self.vu.top = top;
        }
        let pc = pc.unwrap_or(self.vu.pc);
        self.vu.run(pc, self.gif, self.gs);
    }

    fn direct(&mut self, q: u128) {
        self.gif.write(1, q, self.gs);
    }
}

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
    pub fn write(&mut self, words: &[u32], vu: &mut impl VuSink) {
        let mut i = 0;
        while i < words.len() {
            // UNPACK's data, vectors at a time, from the words themselves.
            if matches!(self.pending, Pending::Unpack(_)) && self.bytes.is_empty() {
                let n = self.unpack_words(&words[i..], vu);
                if n > 0 {
                    i += n;
                    continue;
                }
            }
            self.word(words[i], vu);
            i += 1;
        }
    }

    /// UNPACK, with no bits left over from earlier words: unpacks the
    /// vectors `words` hold whole, as `unpack_word` would word by word, and
    /// returns the words used (the rest of the last one kept in `bytes`
    /// for the next vector, or padding after the last vector).
    fn unpack_words(&mut self, words: &[u32], vu: &mut impl VuSink) -> usize {
        let (elements, bits) = self.format();
        let at = if !vu.values() && self.mode != 2 {
            // The vectors counted only.
            let size = if bits == 5 { 2 } else { (elements * bits / 8) as usize };
            self.unpack_skip(words.len() * 4, size)
        } else {
            // V4-5, rare (and the undefined 4-bit formats): word by word.
            if !matches!(bits, 8 | 16 | 32) {
                return 0;
            }
            let signed = self.code & (1 << 14) == 0;
            match bits {
                32 => self.unpack_run::<32>(words, elements, signed, vu),
                16 => self.unpack_run::<16>(words, elements, signed, vu),
                _ => self.unpack_run::<8>(words, elements, signed, vu),
            }
        };
        let byte = |b: usize| (words[b / 4] >> (8 * (b % 4))) as u8;
        let used = at.div_ceil(4);
        if self.vectors_left() == 0 {
            // The rest of the last word is padding.
            self.pending = Pending::None;
        } else {
            self.bytes.extend((at..used * 4).map(byte));
        }
        used
    }

    fn word(&mut self, w: u32, vu: &mut impl VuSink) {
        match self.pending {
            Pending::None => self.command(w, vu),
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
                let at = at % vu.micro_len();
                vu.write_micro(at, w);
                self.pending = if left > 1 {
                    Pending::Mpg(left - 1, at + 4)
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
                    vu.direct(q);
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

    fn command(&mut self, w: u32, vu: &mut impl VuSink) {
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
            0x14 | 0x15 => self.start(Some(imm), vu),
            0x17 => self.start(None, vu),
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
    fn start(&mut self, pc: Option<u32>, vu: &mut impl VuSink) {
        let mut top = None;
        if self.index == 1 {
            top = Some(self.tops as u16);
            self.dbf = !self.dbf;
            // A 10-bit register: VU1's memory is 1024 quadwords.
            self.tops = (self.base + if self.dbf { self.ofst } else { 0 }) & 0x3FF;
        }
        self.calls += 1;
        vu.start(pc, self.itops as u16, top);
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

    fn unpack_word(&mut self, w: u32, vu: &mut impl VuSink) {
        self.bytes.extend_from_slice(&w.to_le_bytes());
        let (elements, bits) = self.format();
        let vector_bytes = if bits == 5 { 2 } else { (elements * bits / 8) as usize };
        while self.vectors_left() > 0 && self.needs_data() && self.bytes.len() >= vector_bytes {
            // At most 16 bytes (V4-32).
            let mut data = [0u8; 16];
            data[..vector_bytes].copy_from_slice(&self.bytes[..vector_bytes]);
            self.bytes.drain(..vector_bytes);
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

    /// `unpack_words`' vectors, of `BITS`-bit elements: the bytes used.
    fn unpack_run<const BITS: u32>(
        &mut self,
        words: &[u32],
        elements: u32,
        signed: bool,
        vu: &mut impl VuSink,
    ) -> usize {
        let size = (elements * BITS / 8) as usize;
        let get = |at: usize, k: usize| -> u32 {
            match BITS {
                32 => words[at / 4 + k],
                16 => {
                    let b = at + 2 * k;
                    let v = (words[b / 4] >> (8 * (b % 4))) as u16;
                    if signed { v as i16 as i32 as u32 } else { u32::from(v) }
                }
                _ => {
                    let b = at + k;
                    let v = (words[b / 4] >> (8 * (b % 4))) as u8;
                    if signed { v as i8 as i32 as u32 } else { u32::from(v) }
                }
            }
        };
        let total = words.len() * 4;
        let mut at = 0;
        while self.vectors_left() > 0 {
            if !self.needs_data() {
                self.write_vector([None; 4], vu);
                continue;
            }
            if at + size > total {
                break;
            }
            let values: [Option<u32>; 4] = match elements {
                // A scalar fills the four lanes; V2 and V3 leave the rest.
                1 => [Some(get(at, 0)); 4],
                2 => [Some(get(at, 0)), Some(get(at, 1)), None, None],
                3 => [Some(get(at, 0)), Some(get(at, 1)), Some(get(at, 2)), None],
                _ => [Some(get(at, 0)), Some(get(at, 1)), Some(get(at, 2)), Some(get(at, 3))],
            };
            self.write_vector(values, vu);
            at += size;
        }
        at
    }

    /// `unpack_words`' vectors, counted, not read: of `total` bytes, those
    /// they use (`size` a vector).
    fn unpack_skip(&mut self, total: usize, size: usize) -> usize {
        let mut at = 0;
        while let Pending::Unpack(n) = self.pending
            && n > 0
        {
            if self.needs_data() {
                if at + size > total {
                    break;
                }
                at += size;
            }
            self.written += 1;
            self.pending = Pending::Unpack(n - 1);
        }
        at
    }

    /// Filling writes: vectors with no input, from the row and column
    /// registers through the mask.
    fn unpack_fill(&mut self, vu: &mut impl VuSink) {
        while self.vectors_left() > 0 && !self.needs_data() {
            self.write_vector([None; 4], vu);
        }
        if self.vectors_left() == 0 {
            self.pending = Pending::None;
        }
    }

    /// Writes a vector (masked, with the row added or accumulated) at the
    /// next address.
    fn write_vector(&mut self, data: [Option<u32>; 4], vu: &mut impl VuSink) {
        let wl = self.wl.max(1);
        let cycle = self.written % wl;
        let dest = if self.wl <= self.cl {
            self.addr + (self.written / wl) * self.cl + cycle
        } else {
            self.addr + self.written
        };
        let masked = (self.code >> 24) & 0x10 != 0;
        if !masked && self.mode == 0 {
            // The lanes given, as they are.
            vu.write_lanes(dest, data);
            self.written += 1;
            if let Pending::Unpack(n) = self.pending {
                self.pending = Pending::Unpack(n - 1);
            }
            return;
        }
        let row = cycle.min(3);
        let mut out = [None; 4];
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
            out[k] = value;
        }
        vu.write_lanes(dest, out);
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
        vif.write(
            words,
            &mut Direct {
                vu,
                gif: &mut gif,
                gs: &mut gs,
            },
        );
    }

    /// UNPACKs fed in chunks (vectors at a time, `unpack_words`) do what
    /// they do word by word (`word`): random formats, cycles, modes, masks.
    #[test]
    fn unpacking_in_bulk_is_unpacking_word_by_word() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % u64::from(n)) as u32
        };
        for round in 0..200 {
            let (mut slow, mut fast) = (Vif::new(1), Vif::new(1));
            let (mut slow_vu, mut fast_vu) = (Vu::new(1), Vu::new(1));
            let (mut gif, mut gs) = (Gif::default(), Gs::default());
            // The stream, made against the word-by-word VIF.
            let mut stream = Vec::new();
            let mut push = |w: u32, vif: &mut Vif, vu: &mut Vu, stream: &mut Vec<u32>| {
                vif.word(
                    w,
                    &mut Direct {
                        vu,
                        gif: &mut gif,
                        gs: &mut gs,
                    },
                );
                stream.push(w);
            };
            for _ in 0..30 {
                let cmd = match rand(6) {
                    0 => 0x0100_0000 | (rand(4) + 1) << 8 | (rand(4) + 1),
                    1 => 0x0500_0000 | rand(3),
                    2 => 0x2000_0000,
                    3 => 0x3000_0000 | rand(2) << 24,
                    _ => {
                        let unpack = 0x60 | rand(32);
                        // With or without USN (bit 14) and FLG (bit 15).
                        (unpack << 24) | (rand(20) + 1) << 16 | rand(0x1_0000)
                    }
                };
                push(cmd, &mut slow, &mut slow_vu, &mut stream);
                // Its data, random, until it takes commands again.
                while slow.pending != Pending::None {
                    push(rand(u32::MAX), &mut slow, &mut slow_vu, &mut stream);
                }
            }
            // The same stream in random chunks.
            let mut at = 0;
            while at < stream.len() {
                let n = (rand(12) as usize + 1).min(stream.len() - at);
                feed(&mut fast, &mut fast_vu, &stream[at..at + n]);
                at += n;
            }
            if let Some(q) =
                (0..slow_vu.data.len() / 16).find(|&q| slow_vu.read_q(q as u32) != fast_vu.read_q(q as u32))
            {
                panic!(
                    "round {round}: quadword {q} differs: {:08x?} word by word, {:08x?} in bulk; stream {:08x?}",
                    slow_vu.read_q(q as u32),
                    fast_vu.read_q(q as u32),
                    &stream[..stream.len().min(80)]
                );
            }
            assert_eq!(
                postcard::to_allocvec(&slow).unwrap(),
                postcard::to_allocvec(&fast).unwrap(),
                "round {round}: VIF state differs"
            );
        }
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
