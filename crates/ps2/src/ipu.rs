//! The IPU: the EE's MPEG decoder, which games play their movies with. The
//! EE (or DMA channel 4) writes a bitstream into its input FIFO, issues
//! commands through IPU_CMD, and reads what they produce from its output
//! FIFO (or DMA channel 3):
//!
//! - reading the bitstream: BCLR (clear the input, set the bit pointer),
//!   FDEC (a fixed-length field), VDEC (a variable-length code: macroblock
//!   address increment, macroblock type, motion code, dual-prime vector);
//! - tables: SETIQ (a quantiser matrix), SETVQ (the colour table of PACK's
//!   4-bit output), SETTH (the thresholds of colour conversion's alpha);
//! - decoding: BDEC (one macroblock's blocks, inverse quantised and
//!   transformed, as 16-bit samples), IDEC (an intra picture's slice,
//!   macroblock after macroblock, straight to RGB);
//! - converting: CSC (YCbCr macroblocks to RGB), PACK (RGB32 to RGB16, or
//!   to 4-bit indices into the colour table).
//!
//! The rest of a movie (headers, motion compensation, which picture comes
//! when) is the games' code, on the EE.
//!
//! A command may need more of the bitstream than is there, as when a DMA
//! transfer ends in its middle: it then waits, busy, and goes on when more
//! comes, from the start of the macroblock it was decoding (or the start of
//! the command, for the others), as nothing it did is kept until a
//! macroblock is complete.

use std::collections::VecDeque;
use std::sync::OnceLock;

/// The input FIFO's size, in quadwords (what IPU_CTRL and IPU_BP count).
const FIFO: usize = 8;

// --- IPU_CTRL ---------------------------------------------------------------------

const CTRL_ECD: u32 = 1 << 14;
const CTRL_SCD: u32 = 1 << 15;
/// The fields the EE sets: IDP, AS, IVF, QST, MP1, PCT.
const CTRL_SETTABLE: u32 = 0x07F3_0000;
const CTRL_RST: u32 = 1 << 30;

// --- Macroblock types (VDEC, and what the decoder reads) ------------------------------

const MB_INTRA: i32 = 0x01;
const MB_QUANT: i32 = 0x10;

/// What stops a command: more input needed, or a bitstream error.
enum Halt {
    Stall,
    Error,
}

type Step<T> = Result<T, Halt>;

/// The bitstream: the input's quadwords, read from bit `pos` of the first,
/// most significant bit of each byte first; more pulled from DMA when it
/// runs out.
struct Bits<'a> {
    input: &'a mut VecDeque<u128>,
    pos: u64,
    pull: &'a mut dyn FnMut() -> Option<u128>,
}

impl Bits<'_> {
    fn ensure(&mut self, n: u32) -> Step<()> {
        while (self.input.len() as u64) * 128 < self.pos + u64::from(n) {
            match (self.pull)() {
                Some(q) => self.input.push_back(q),
                None => return Err(Halt::Stall),
            }
        }
        Ok(())
    }

    /// The next `n` bits (32 at most), not consumed.
    fn peek(&mut self, n: u32) -> Step<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure(n)?;
        let i = (self.pos / 128) as usize;
        let off = (self.pos % 128) as u32;
        let a = self.input[i].swap_bytes();
        let w = if off == 0 {
            a
        } else {
            let b = self.input.get(i + 1).map_or(0, |q| q.swap_bytes());
            (a << off) | (b >> (128 - off))
        };
        Ok((w >> (128 - n)) as u32)
    }

    fn skip(&mut self, n: u32) -> Step<()> {
        self.ensure(n)?;
        self.pos += u64::from(n);
        Ok(())
    }

    fn get(&mut self, n: u32) -> Step<u32> {
        let v = self.peek(n)?;
        self.pos += u64::from(n);
        Ok(v)
    }

    fn signed(&mut self, n: u32) -> Step<i32> {
        let v = self.get(n)? as i32;
        Ok(if v & (1 << (n - 1)) != 0 { v - (1 << n) } else { v })
    }
}

// --- Variable-length codes ----------------------------------------------------------

/// A code table, looked up by its longest code's length of bits: each
/// entry, the code's length (0: no code) and its value.
struct Vlc {
    bits: u32,
    table: Vec<(u8, i32)>,
}

impl Vlc {
    /// From the codes, written as the standard's bit strings (spaces
    /// allowed). Panics on a code that is the start of another.
    fn new(codes: &[(&str, i32)]) -> Self {
        let codes: Vec<(String, i32)> = codes.iter().map(|(c, v)| (c.replace(' ', ""), *v)).collect();
        let bits = codes.iter().map(|(c, _)| c.len() as u32).max().unwrap_or(1);
        let mut table = vec![(0u8, 0i32); 1 << bits];
        for (code, value) in &codes {
            let len = code.len() as u32;
            let prefix = u32::from_str_radix(code, 2).expect("a code of 0s and 1s");
            let start = (prefix << (bits - len)) as usize;
            for e in &mut table[start..start + (1 << (bits - len))] {
                assert_eq!(e.0, 0, "the code {code} overlaps another");
                *e = (len as u8, *value);
            }
        }
        Self { bits, table }
    }

    /// The next code's value (None: no code there).
    fn read(&self, b: &mut Bits) -> Step<Option<(i32, u32)>> {
        let (len, value) = self.table[b.peek(self.bits)? as usize];
        if len == 0 {
            return Ok(None);
        }
        b.skip(u32::from(len))?;
        Ok(Some((value, u32::from(len))))
    }
}

/// A DCT coefficient code's value: run and level, or these.
const EOB: i32 = -1;
const ESCAPE: i32 = -2;

const fn rl(run: i32, level: i32) -> i32 {
    run << 8 | level
}

struct Tables {
    mbai: Vlc,
    mbt: [Vlc; 4],
    cbp: Vlc,
    motion: Vlc,
    dmv: Vlc,
    dc_luma: Vlc,
    dc_chroma: Vlc,
    /// DCT coefficients, tables zero (B.14) and one (B.15), without their
    /// sign bit.
    dct: [Vlc; 2],
}

/// Escape and stuffing, as VDEC returns them for a macroblock address
/// increment.
const MBAI_ESCAPE: i32 = 0x23;
const MBAI_STUFFING: i32 = 0x22;

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        // B.1, with escape and (MPEG-1) stuffing.
        let mbai = Vlc::new(&[
            ("1", 1),
            ("011", 2),
            ("010", 3),
            ("0011", 4),
            ("0010", 5),
            ("0001 1", 6),
            ("0001 0", 7),
            ("0000 111", 8),
            ("0000 110", 9),
            ("0000 1011", 10),
            ("0000 1010", 11),
            ("0000 1001", 12),
            ("0000 1000", 13),
            ("0000 0111", 14),
            ("0000 0110", 15),
            ("0000 0101 11", 16),
            ("0000 0101 10", 17),
            ("0000 0101 01", 18),
            ("0000 0101 00", 19),
            ("0000 0100 11", 20),
            ("0000 0100 10", 21),
            ("0000 0100 011", 22),
            ("0000 0100 010", 23),
            ("0000 0100 001", 24),
            ("0000 0100 000", 25),
            ("0000 0011 111", 26),
            ("0000 0011 110", 27),
            ("0000 0011 101", 28),
            ("0000 0011 100", 29),
            ("0000 0011 011", 30),
            ("0000 0011 010", 31),
            ("0000 0011 001", 32),
            ("0000 0011 000", 33),
            ("0000 0001 000", MBAI_ESCAPE),
            ("0000 0001 111", MBAI_STUFFING),
        ]);
        // B.2 to B.4 (I, P, B pictures) and D pictures: quant 0x10,
        // forward 0x08, backward 0x04, pattern 0x02, intra 0x01.
        let mbt = [
            Vlc::new(&[("1", 0x01), ("01", 0x11)]),
            Vlc::new(&[
                ("1", 0x0A),
                ("01", 0x02),
                ("001", 0x08),
                ("0001 1", 0x01),
                ("0001 0", 0x1A),
                ("0000 1", 0x12),
                ("0000 01", 0x11),
            ]),
            Vlc::new(&[
                ("10", 0x0C),
                ("11", 0x0E),
                ("010", 0x04),
                ("011", 0x06),
                ("0010", 0x08),
                ("0011", 0x0A),
                ("0001 1", 0x01),
                ("0001 0", 0x1E),
                ("0000 11", 0x1A),
                ("0000 10", 0x16),
                ("0000 01", 0x11),
            ]),
            Vlc::new(&[("1", 0x01)]),
        ];
        // B.9.
        let cbp = Vlc::new(&[
            ("111", 60),
            ("1101", 4),
            ("1100", 8),
            ("1011", 16),
            ("1010", 32),
            ("1001 1", 12),
            ("1001 0", 48),
            ("1000 1", 20),
            ("1000 0", 40),
            ("0111 1", 28),
            ("0111 0", 44),
            ("0110 1", 52),
            ("0110 0", 56),
            ("0101 1", 1),
            ("0101 0", 61),
            ("0100 1", 2),
            ("0100 0", 62),
            ("0011 11", 24),
            ("0011 10", 36),
            ("0011 01", 3),
            ("0011 00", 63),
            ("0010 111", 5),
            ("0010 110", 9),
            ("0010 101", 17),
            ("0010 100", 33),
            ("0010 011", 6),
            ("0010 010", 10),
            ("0010 001", 18),
            ("0010 000", 34),
            ("0001 1111", 7),
            ("0001 1110", 11),
            ("0001 1101", 19),
            ("0001 1100", 35),
            ("0001 1011", 13),
            ("0001 1010", 49),
            ("0001 1001", 21),
            ("0001 1000", 41),
            ("0001 0111", 14),
            ("0001 0110", 50),
            ("0001 0101", 22),
            ("0001 0100", 42),
            ("0001 0011", 15),
            ("0001 0010", 51),
            ("0001 0001", 23),
            ("0001 0000", 43),
            ("0000 1111", 25),
            ("0000 1110", 37),
            ("0000 1101", 26),
            ("0000 1100", 38),
            ("0000 1011", 29),
            ("0000 1010", 45),
            ("0000 1001", 53),
            ("0000 1000", 57),
            ("0000 0111", 30),
            ("0000 0110", 46),
            ("0000 0101", 54),
            ("0000 0100", 58),
            ("0000 0011 1", 31),
            ("0000 0011 0", 47),
            ("0000 0010 1", 55),
            ("0000 0010 0", 59),
            ("0000 0001 1", 27),
            ("0000 0001 0", 39),
            ("0000 0000 1", 0),
        ]);
        // B.10, without the sign bit: the magnitude.
        let motion = Vlc::new(&[
            ("1", 0),
            ("01", 1),
            ("001", 2),
            ("0001", 3),
            ("0000 11", 4),
            ("0000 101", 5),
            ("0000 100", 6),
            ("0000 011", 7),
            ("0000 0101 1", 8),
            ("0000 0101 0", 9),
            ("0000 0100 1", 10),
            ("0000 0100 01", 11),
            ("0000 0100 00", 12),
            ("0000 0011 11", 13),
            ("0000 0011 10", 14),
            ("0000 0011 01", 15),
            ("0000 0011 00", 16),
        ]);
        // B.11.
        let dmv = Vlc::new(&[("0", 0), ("10", 1), ("11", -1)]);
        // B.12 and B.13: dct_dc_size.
        let dc_luma = Vlc::new(&[
            ("100", 0),
            ("00", 1),
            ("01", 2),
            ("101", 3),
            ("110", 4),
            ("1110", 5),
            ("1111 0", 6),
            ("1111 10", 7),
            ("1111 110", 8),
            ("1111 1110", 9),
            ("1111 1111 0", 10),
            ("1111 1111 1", 11),
        ]);
        let dc_chroma = Vlc::new(&[
            ("00", 0),
            ("01", 1),
            ("10", 2),
            ("110", 3),
            ("1110", 4),
            ("1111 0", 5),
            ("1111 10", 6),
            ("1111 110", 7),
            ("1111 1110", 8),
            ("1111 1111 0", 9),
            ("1111 1111 10", 10),
            ("1111 1111 11", 11),
        ]);
        // The codes of B.14 and B.15 longer than 12 bits (but the sign),
        // the same in both.
        let long: &[(&str, i32)] = &[
            ("0000 0000 1101 0", rl(0, 12)),
            ("0000 0000 1100 1", rl(0, 13)),
            ("0000 0000 1100 0", rl(0, 14)),
            ("0000 0000 1011 1", rl(0, 15)),
            ("0000 0000 1011 0", rl(1, 6)),
            ("0000 0000 1010 1", rl(1, 7)),
            ("0000 0000 1010 0", rl(2, 5)),
            ("0000 0000 1001 1", rl(3, 4)),
            ("0000 0000 1001 0", rl(5, 3)),
            ("0000 0000 1000 1", rl(9, 2)),
            ("0000 0000 1000 0", rl(10, 2)),
            ("0000 0000 1111 1", rl(22, 1)),
            ("0000 0000 1111 0", rl(23, 1)),
            ("0000 0000 1110 1", rl(24, 1)),
            ("0000 0000 1110 0", rl(25, 1)),
            ("0000 0000 1101 1", rl(26, 1)),
            ("0000 0000 0111 11", rl(0, 16)),
            ("0000 0000 0111 10", rl(0, 17)),
            ("0000 0000 0111 01", rl(0, 18)),
            ("0000 0000 0111 00", rl(0, 19)),
            ("0000 0000 0110 11", rl(0, 20)),
            ("0000 0000 0110 10", rl(0, 21)),
            ("0000 0000 0110 01", rl(0, 22)),
            ("0000 0000 0110 00", rl(0, 23)),
            ("0000 0000 0101 11", rl(0, 24)),
            ("0000 0000 0101 10", rl(0, 25)),
            ("0000 0000 0101 01", rl(0, 26)),
            ("0000 0000 0101 00", rl(0, 27)),
            ("0000 0000 0100 11", rl(0, 28)),
            ("0000 0000 0100 10", rl(0, 29)),
            ("0000 0000 0100 01", rl(0, 30)),
            ("0000 0000 0100 00", rl(0, 31)),
            ("0000 0000 0011 000", rl(0, 32)),
            ("0000 0000 0010 111", rl(0, 33)),
            ("0000 0000 0010 110", rl(0, 34)),
            ("0000 0000 0010 101", rl(0, 35)),
            ("0000 0000 0010 100", rl(0, 36)),
            ("0000 0000 0010 011", rl(0, 37)),
            ("0000 0000 0010 010", rl(0, 38)),
            ("0000 0000 0010 001", rl(0, 39)),
            ("0000 0000 0010 000", rl(0, 40)),
            ("0000 0000 0011 111", rl(1, 8)),
            ("0000 0000 0011 110", rl(1, 9)),
            ("0000 0000 0011 101", rl(1, 10)),
            ("0000 0000 0011 100", rl(1, 11)),
            ("0000 0000 0011 011", rl(1, 12)),
            ("0000 0000 0011 010", rl(1, 13)),
            ("0000 0000 0011 001", rl(1, 14)),
            ("0000 0000 0001 0011", rl(1, 15)),
            ("0000 0000 0001 0010", rl(1, 16)),
            ("0000 0000 0001 0001", rl(1, 17)),
            ("0000 0000 0001 0000", rl(1, 18)),
            ("0000 0000 0001 0100", rl(6, 3)),
            ("0000 0000 0001 1010", rl(11, 2)),
            ("0000 0000 0001 1001", rl(12, 2)),
            ("0000 0000 0001 1000", rl(13, 2)),
            ("0000 0000 0001 0111", rl(14, 2)),
            ("0000 0000 0001 0110", rl(15, 2)),
            ("0000 0000 0001 0101", rl(16, 2)),
            ("0000 0000 0001 1111", rl(27, 1)),
            ("0000 0000 0001 1110", rl(28, 1)),
            ("0000 0000 0001 1101", rl(29, 1)),
            ("0000 0000 0001 1100", rl(30, 1)),
            ("0000 0000 0001 1011", rl(31, 1)),
        ];
        // The 12-bit codes (but the sign) of both, with seven leading zeros.
        let twelve: &[(&str, i32)] = &[
            ("0000 0001 1100", rl(3, 3)),
            ("0000 0001 0010", rl(4, 3)),
            ("0000 0001 1110", rl(6, 2)),
            ("0000 0001 0101", rl(7, 2)),
            ("0000 0001 0001", rl(8, 2)),
            ("0000 0001 1111", rl(17, 1)),
            ("0000 0001 1010", rl(18, 1)),
            ("0000 0001 1001", rl(19, 1)),
            ("0000 0001 0111", rl(20, 1)),
            ("0000 0001 0110", rl(21, 1)),
        ];
        let b14: Vec<(&str, i32)> = [
            ("10", EOB),
            ("11", rl(0, 1)),
            ("011", rl(1, 1)),
            ("0100", rl(0, 2)),
            ("0101", rl(2, 1)),
            ("0010 1", rl(0, 3)),
            ("0011 1", rl(3, 1)),
            ("0011 0", rl(4, 1)),
            ("0001 10", rl(1, 2)),
            ("0001 11", rl(5, 1)),
            ("0001 01", rl(6, 1)),
            ("0001 00", rl(7, 1)),
            ("0000 110", rl(0, 4)),
            ("0000 100", rl(2, 2)),
            ("0000 111", rl(8, 1)),
            ("0000 101", rl(9, 1)),
            ("0000 01", ESCAPE),
            ("0010 0110", rl(0, 5)),
            ("0010 0001", rl(0, 6)),
            ("0010 0101", rl(1, 3)),
            ("0010 0100", rl(3, 2)),
            ("0010 0111", rl(10, 1)),
            ("0010 0011", rl(11, 1)),
            ("0010 0010", rl(12, 1)),
            ("0010 0000", rl(13, 1)),
            ("0000 0010 10", rl(0, 7)),
            ("0000 0011 00", rl(1, 4)),
            ("0000 0010 11", rl(2, 3)),
            ("0000 0011 11", rl(4, 2)),
            ("0000 0010 01", rl(5, 2)),
            ("0000 0011 10", rl(14, 1)),
            ("0000 0011 01", rl(15, 1)),
            ("0000 0010 00", rl(16, 1)),
            ("0000 0001 1101", rl(0, 8)),
            ("0000 0001 1000", rl(0, 9)),
            ("0000 0001 0011", rl(0, 10)),
            ("0000 0001 0000", rl(0, 11)),
            ("0000 0001 1011", rl(1, 5)),
            ("0000 0001 0100", rl(2, 4)),
        ]
        .into_iter()
        .chain(twelve.iter().copied())
        .chain(long.iter().copied())
        .collect();
        let b15: Vec<(&str, i32)> = [
            ("0110", EOB),
            ("10", rl(0, 1)),
            ("010", rl(1, 1)),
            ("110", rl(0, 2)),
            ("0010 1", rl(2, 1)),
            ("0111", rl(0, 3)),
            ("0011 1", rl(3, 1)),
            ("0001 10", rl(4, 1)),
            ("0011 0", rl(1, 2)),
            ("0001 11", rl(5, 1)),
            ("0000 110", rl(6, 1)),
            ("0000 100", rl(7, 1)),
            ("1110 0", rl(0, 4)),
            ("0000 111", rl(2, 2)),
            ("0000 101", rl(8, 1)),
            ("1111 000", rl(9, 1)),
            ("0000 01", ESCAPE),
            ("1110 1", rl(0, 5)),
            ("0001 01", rl(0, 6)),
            ("1111 001", rl(1, 3)),
            ("0010 0110", rl(3, 2)),
            ("1111 010", rl(10, 1)),
            ("0010 0001", rl(11, 1)),
            ("0010 0101", rl(12, 1)),
            ("0010 0100", rl(13, 1)),
            ("0001 00", rl(0, 7)),
            ("0010 0111", rl(1, 4)),
            ("1111 1100", rl(2, 3)),
            ("1111 1101", rl(4, 2)),
            ("0000 0010 0", rl(5, 2)),
            ("0000 0010 1", rl(14, 1)),
            ("0000 0011 1", rl(15, 1)),
            ("0000 0011 01", rl(16, 1)),
            ("1111 011", rl(0, 8)),
            ("1111 100", rl(0, 9)),
            ("0010 0011", rl(0, 10)),
            ("0010 0010", rl(0, 11)),
            ("0010 0000", rl(1, 5)),
            ("0000 0011 00", rl(2, 4)),
            ("1111 1010", rl(0, 12)),
            ("1111 1011", rl(0, 13)),
            ("1111 1110", rl(0, 14)),
            ("1111 1111", rl(0, 15)),
        ]
        .into_iter()
        .chain(twelve.iter().copied())
        // B.15's own codes for these runs and levels: not B.14's longer ones.
        .chain(long.iter().copied().filter(|&(_, v)| !matches!(v, 0x0C..=0x0F)))
        .collect();
        Tables {
            mbai,
            mbt,
            cbp,
            motion,
            dmv,
            dc_luma,
            dc_chroma,
            dct: [Vlc::new(&b14), Vlc::new(&b15)],
        }
    })
}

// --- Inverse quantisation and transform -------------------------------------------------

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
    47, 55, 62, 63,
];
const ALTERNATE: [usize; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19, 27, 34, 42, 50, 58,
    35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53, 61, 22, 30, 7, 15, 23, 31, 38, 46, 54,
    62, 39, 47, 55, 63,
];
/// quantiser_scale for each quantiser_scale_code, q_scale_type 1.
const NONLINEAR: [i32; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 104,
    112,
];
/// The default intra quantiser matrix, in natural order.
const DEFAULT_INTRA: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38, 22, 22, 26, 27, 29,
    34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58, 26, 27, 29, 34, 38, 46, 56, 69, 27, 29,
    35, 38, 46, 56, 69, 83,
];

/// The 8×8 inverse DCT (as the standard defines it, in double precision),
/// rounded, clamped to `lo..=hi`.
fn idct(f: &[i32; 64], lo: i32, hi: i32) -> [i32; 64] {
    static COS: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    let c = COS.get_or_init(|| {
        std::array::from_fn(|x| {
            std::array::from_fn(|u| {
                let cu = if u == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
                cu / 2.0 * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos()
            })
        })
    });
    // Rows, then columns.
    let mut tmp = [0f64; 64];
    for v in 0..8 {
        for x in 0..8 {
            tmp[v * 8 + x] = (0..8).map(|u| c[x][u] * f64::from(f[v * 8 + u])).sum();
        }
    }
    let mut out = [0i32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let s: f64 = (0..8).map(|v| c[y][v] * tmp[v * 8 + x]).sum();
            out[y * 8 + x] = (s.round() as i32).clamp(lo, hi);
        }
    }
    out
}

// --- Colour conversion -----------------------------------------------------------------

/// A 4×4 ordered dither, for RGB16.
const DITHER: [[i32; 4]; 4] = [[-4, 0, -3, 1], [2, -2, 3, -1], [-3, 1, -4, 0], [3, -1, 2, -2]];

/// RGB from YCbCr (ITU-R BT.601, studio range), in 7-bit fixed point.
fn rgb(y: u8, cb: u8, cr: u8) -> [i32; 3] {
    let y = (i32::from(y) - 16) * 149;
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    [
        (y + 204 * cr + 64) >> 7,
        (y - 50 * cb - 104 * cr + 64) >> 7,
        (y + 258 * cb + 64) >> 7,
    ]
    .map(|c| c.clamp(0, 255))
}

/// A macroblock's samples: luma 16×16, then Cb and Cr 8×8.
type Raw8 = [u8; 384];

// --- The IPU ----------------------------------------------------------------------------

/// A command in progress.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct Job {
    code: u32,
    opt: u32,
    /// Its first bits (FB) skipped, and its setup done.
    begun: bool,
    /// CSC, PACK: macroblocks left. IDEC: macroblocks done.
    count: u32,
    /// IDEC: the quantiser scale code in force.
    qsc: u32,
}

/// What decoding keeps between commands.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Decoder {
    /// IPU_CTRL: the EE's settings, and CBP, ECD, SCD.
    ctrl: u32,
    /// Intra DC predictors: Y, Cb, Cr.
    dc: [i32; 3],
    /// Quantiser matrices, intra and non-intra, in natural order.
    iq: [Vec<u8>; 2],
    /// PACK's colour table (RGB16).
    vq: [u16; 16],
    /// SETTH: alpha thresholds.
    th: [i32; 2],
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Ipu {
    dec: Decoder,
    /// IPU_CMD's result (FDEC, VDEC).
    data: u32,
    job: Option<Job>,
    /// The input: quadwords from the FIFO (and DMA, when a command pulls
    /// more), the first `pos` bits of which are used.
    input: VecDeque<u128>,
    pos: u64,
    /// How far a waiting command read before it ran out (it goes on from
    /// `pos`): what it read is out of the FIFO, as the console's decoder
    /// takes what it reads.
    reach: u64,
    /// The words of a quadword written into the input FIFO 32 bits at a
    /// time.
    partial: Vec<u32>,
    /// What the commands produced, for the EE or DMA channel 3.
    pub output: VecDeque<u128>,
    /// A command ended: the IPU's interrupt.
    #[serde(skip)]
    pub irq: bool,
    /// Commands issued, by code; and those that ended on an error, for
    /// diagnostics.
    #[serde(skip)]
    pub commands: [u64; 16],
    #[serde(skip)]
    pub errors: u64,
    /// Diagnostics: the first errors: the command (its code and
    /// options), the bit it started at, the bit it failed at, and the 32
    /// bits from there (0 when not there).
    #[serde(skip)]
    pub error_log: Vec<(u32, u32, u64, u64, u32)>,
    /// Diagnostics: the first error's IPU_CTRL and input (the quadwords
    /// from the one its command started in).
    #[serde(skip)]
    pub error_input: Option<(u32, Vec<u128>)>,
    /// Diagnostics, when set: the input stream and the commands run on it.
    #[serde(skip)]
    pub trace: Option<Box<Trace>>,
}

/// The IPU's input, as it left the FIFO (decoded, cleared by BCLR, or
/// dropped by a reset), and the commands, by bit of that stream: to
/// compare with another decoder's parse of the same stream.
#[derive(Default, Debug, Clone)]
pub struct Trace {
    /// The quadwords, in order, as bytes (128 MiB at most).
    pub stream: Vec<u8>,
    /// Quadwords gone from the FIFO's front so far.
    removed: u64,
    /// The running command's first bit (it may stall and go on).
    start: Option<u64>,
    /// Commands done.
    pub commands: Vec<TraceCommand>,
    /// The frame now, for the commands (set by the bus).
    pub frame: u64,
}

/// A command the IPU ran, in its trace.
#[derive(Debug, Clone)]
pub struct TraceCommand {
    pub code: u32,
    pub opt: u32,
    /// Its first and next bit, of the stream.
    pub start: u64,
    pub end: u64,
    /// Whether it failed (ECD).
    pub err: bool,
    /// Its result (VDEC, FDEC).
    pub data: u32,
    /// The frame it ended in.
    pub frame: u64,
}

impl Trace {
    fn remove(&mut self, q: u128) {
        if self.stream.len() < 128 << 20 {
            self.stream.extend_from_slice(&q.to_le_bytes());
        }
        self.removed += 1;
    }
}

impl Default for Ipu {
    fn default() -> Self {
        Self {
            dec: Decoder {
                ctrl: 0,
                dc: [128; 3],
                iq: [DEFAULT_INTRA.to_vec(), vec![16; 64]],
                vq: [0; 16],
                th: [0; 2],
            },
            data: 0,
            job: None,
            input: VecDeque::new(),
            pos: 0,
            reach: 0,
            partial: Vec::new(),
            output: VecDeque::new(),
            irq: false,
            commands: [0; 16],
            errors: 0,
            error_log: Vec::new(),
            error_input: None,
            trace: None,
        }
    }
}

impl Ipu {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Quadwords of input not read yet (a partly read one included).
    fn left(&self) -> usize {
        self.input
            .len()
            .saturating_sub((self.pos.max(self.reach) / 128) as usize)
    }

    /// The input FIFO's count, and the decoder's own (up to 2 quadwords).
    pub fn counts(&self) -> (u32, u32) {
        let left = self.left();
        let fp = left.min(2);
        ((left - fp).min(FIFO) as u32, fp as u32)
    }

    /// Whether the input FIFO has room, for DMA channel 4.
    pub fn wants_input(&self) -> bool {
        self.left() < FIFO + 2
    }

    pub fn read_ctrl(&self) -> u32 {
        let (ifc, _) = self.counts();
        let ofc = self.output.len().min(FIFO) as u32;
        (self.dec.ctrl & !0xFF & !(1 << 31)) | ifc | ofc << 4 | u32::from(self.busy()) << 31
    }

    pub fn write_ctrl(&mut self, v: u32) {
        if v & CTRL_RST != 0 {
            let dec = Decoder {
                ctrl: v & CTRL_SETTABLE,
                ..self.dec.clone()
            };
            *self = Self {
                dec,
                commands: self.commands,
                errors: self.errors,
                error_log: std::mem::take(&mut self.error_log),
                error_input: self.error_input.take(),
                trace: self.trace.take().map(|mut t| {
                    // What the FIFO held is dropped.
                    for q in self.input.drain(..) {
                        t.remove(q);
                    }
                    t
                }),
                ..Self::default()
            };
            return;
        }
        self.dec.ctrl = (self.dec.ctrl & !CTRL_SETTABLE) | (v & CTRL_SETTABLE);
    }

    /// IPU_CMD: the result, and whether a command runs.
    pub fn read_cmd(&self) -> (u32, bool) {
        (self.data, self.busy())
    }

    /// IPU_BP: the bit pointer, the FIFO's count, the decoder's.
    pub fn read_bp(&self) -> u32 {
        let (ifc, fp) = self.counts();
        (self.pos % 128) as u32 | ifc << 8 | fp << 16
    }

    /// IPU_TOP: the next 32 bits, and whether they are not all there.
    pub fn read_top(&mut self, pull: &mut dyn FnMut() -> Option<u128>) -> (u32, bool) {
        let mut b = Bits {
            input: &mut self.input,
            pos: self.pos,
            pull,
        };
        match b.peek(32) {
            Ok(v) => (v, false),
            Err(_) => {
                // What there is, left-aligned.
                let have = ((b.input.len() as u64) * 128).saturating_sub(b.pos).min(32) as u32;
                let v = if have == 0 {
                    0
                } else {
                    b.peek(have).unwrap_or(0) << (32 - have)
                };
                (v, true)
            }
        }
    }

    pub fn write_cmd(&mut self, v: u32) {
        if self.busy() {
            return;
        }
        let code = v >> 28;
        self.commands[code as usize] += 1;
        if matches!(code, 1..=4) {
            self.dec.ctrl &= !(CTRL_ECD | CTRL_SCD);
        }
        self.job = Some(Job {
            code,
            opt: v & 0x0FFF_FFFF,
            begun: false,
            count: 0,
            qsc: 0,
        });
    }

    /// The input FIFO: a quadword, or a word of one.
    pub fn write_fifo(&mut self, q: u128) {
        self.input.push_back(q);
    }

    pub fn write_fifo_word(&mut self, index: u32, w: u32) {
        if index == 0 {
            self.partial.clear();
        }
        self.partial.push(w);
        if self.partial.len() == 4 {
            let q = self
                .partial
                .iter()
                .enumerate()
                .fold(0u128, |q, (k, &w)| q | u128::from(w) << (32 * k));
            self.partial.clear();
            self.input.push_back(q);
        }
    }

    pub fn read_fifo(&mut self) -> u128 {
        self.output.pop_front().unwrap_or(0)
    }

    /// Runs the command, as far as the input goes (`pull` gives more, from
    /// DMA channel 4).
    pub fn run(&mut self, pull: &mut dyn FnMut() -> Option<u128>) {
        let Some(mut job) = self.job else {
            return;
        };
        // BCLR drops what the FIFO holds: kept in the trace first.
        if job.code == 0
            && let Some(t) = self.trace.as_mut()
        {
            for &q in &self.input {
                t.remove(q);
            }
        }
        let trace_base = self.trace.as_ref().map(|t| t.removed * 128);
        let mut b = Bits {
            input: &mut self.input,
            pos: self.pos,
            pull,
        };
        let mut reach = 0;
        let started = b.pos;
        if let (Some(t), Some(base)) = (self.trace.as_mut(), trace_base)
            && t.start.is_none()
        {
            t.start = Some(base + started);
        }
        let done = execute(
            &mut self.dec,
            &mut b,
            &mut job,
            &mut self.output,
            &mut self.data,
            &mut reach,
        );
        if done && self.dec.ctrl & CTRL_ECD != 0 && self.error_log.len() < 32 {
            let failed = b.pos;
            let next = b.peek(32).unwrap_or(0);
            self.error_log.push((job.code, job.opt, started, failed, next));
            if self.error_input.is_none() && job.code == 2 && job.opt & (1 << 27) == 0 {
                let words = b.input.iter().copied().take(12).collect();
                self.error_input = Some((self.dec.ctrl, words));
            }
        }
        let ended = b.pos;
        self.pos = b.pos;
        self.reach = reach;
        if done && let (Some(t), Some(base)) = (self.trace.as_mut(), trace_base) {
            // BCLR: its bit pointer, in the stream after what it dropped.
            let end = if job.code == 0 {
                t.removed * 128 + ended
            } else {
                base + ended
            };
            let start = t.start.take().unwrap_or(end);
            let frame = t.frame;
            t.commands.push(TraceCommand {
                code: job.code,
                opt: job.opt,
                start,
                end,
                err: self.dec.ctrl & CTRL_ECD != 0,
                data: self.data,
                frame,
            });
        }
        // What is behind the bit pointer is gone.
        while self.pos >= 128 && !self.input.is_empty() {
            if let Some(q) = self.input.pop_front()
                && let Some(t) = self.trace.as_mut()
            {
                t.remove(q);
            }
            self.pos -= 128;
            self.reach = self.reach.saturating_sub(128);
        }
        if done {
            self.job = None;
            self.irq = true;
            if self.dec.ctrl & CTRL_ECD != 0 {
                self.errors += 1;
            }
        } else {
            self.job = Some(job);
        }
    }
}

/// Runs a command: whether it is done (else it waits for input, from where
/// it stands: what it did up to there is kept; `reach`: how far it read).
fn execute(
    dec: &mut Decoder,
    b: &mut Bits,
    job: &mut Job,
    out: &mut VecDeque<u128>,
    data: &mut u32,
    reach: &mut u64,
) -> bool {
    let fb = job.opt & 0x3F;
    loop {
        // Where a stall goes back to.
        let (pos, dc, saved) = (b.pos, dec.dc, *job);
        let mut produced: Vec<u8> = Vec::new();
        let step = match job.code {
            // BCLR.
            0x0 => {
                b.input.clear();
                b.pos = u64::from(job.opt & 0x7F);
                return true;
            }
            0x1 => idec(dec, b, job, &mut produced),
            0x2 => bdec(dec, b, job, &mut produced).map(|()| true),
            0x3 => vdec(dec, b, fb, (job.opt >> 26) & 3).map(|v| {
                *data = v;
                true
            }),
            0x4 => (|| {
                b.skip(fb)?;
                *data = b.peek(32)?;
                Ok(true)
            })(),
            0x5 => setiq(dec, b, fb, job.opt >> 27 & 1).map(|()| true),
            0x6 => (|| {
                for e in &mut dec.vq {
                    *e = b.get(16)? as u16;
                }
                Ok(true)
            })(),
            0x7 | 0x8 => convert(dec, b, job, &mut produced),
            0x9 => {
                dec.th = [(job.opt & 0x1FF) as i32, ((job.opt >> 16) & 0x1FF) as i32];
                Ok(true)
            }
            _ => Ok(true),
        };
        match step {
            Ok(done) => {
                for chunk in produced.chunks(16) {
                    let mut q = [0u8; 16];
                    q[..chunk.len()].copy_from_slice(chunk);
                    out.push_back(u128::from_le_bytes(q));
                }
                if done {
                    return true;
                }
            }
            Err(Halt::Stall) => {
                *reach = b.pos;
                b.pos = pos;
                dec.dc = dc;
                *job = saved;
                return false;
            }
            Err(Halt::Error) => {
                dec.ctrl |= CTRL_ECD;
                return true;
            }
        }
    }
}

impl Decoder {
    fn mpeg1(&self) -> bool {
        self.ctrl & (1 << 23) != 0
    }
    fn idp(&self) -> u32 {
        (self.ctrl >> 16) & 3
    }
    fn pct(&self) -> u32 {
        (self.ctrl >> 24) & 7
    }
    fn scan(&self) -> &'static [usize; 64] {
        if self.ctrl & (1 << 20) != 0 {
            &ALTERNATE
        } else {
            &ZIGZAG
        }
    }
    fn reset_dc(&mut self) {
        self.dc = [1 << (7 + self.idp()); 3];
    }
    /// quantiser_scale from its code (MPEG-1's doubled, as MPEG-2's linear
    /// one: the formulas are MPEG-2's).
    fn qscale(&self, code: u32) -> i32 {
        let code = (code & 31) as usize;
        if !self.mpeg1() && self.ctrl & (1 << 22) != 0 {
            NONLINEAR[code]
        } else {
            2 * code as i32
        }
    }

    /// A block's coefficients, inverse quantised: intra (`comp`: 0 luma, 1
    /// Cb, 2 Cr) or not.
    fn block(&mut self, b: &mut Bits, intra: Option<usize>, qs: i32) -> Step<[i32; 64]> {
        let t = tables();
        let mpeg1 = self.mpeg1();
        let scan = self.scan();
        let w = &self.iq[usize::from(intra.is_none())];
        let mut f = [0i32; 64];
        let mut i: i32 = -1;
        if let Some(comp) = intra {
            let size = if comp == 0 { &t.dc_luma } else { &t.dc_chroma };
            let (size, _) = size.read(b)?.ok_or(Halt::Error)?;
            let diff = if size == 0 {
                0
            } else {
                let v = b.get(size as u32)? as i32;
                if v < 1 << (size - 1) { v - (1 << size) + 1 } else { v }
            };
            self.dc[comp] += diff;
            let mult = if mpeg1 { 8 } else { 8 >> self.idp() };
            f[0] = self.dc[comp] * mult;
            i = 0;
        }
        let table = &t.dct[usize::from(intra.is_some() && self.ctrl & (1 << 21) != 0)];
        let mut first = intra.is_none();
        loop {
            let (run, level) = if first && b.peek(1)? == 1 {
                // A non-intra block's first coefficient: "1s" is 0, ±1.
                b.skip(1)?;
                let s = b.get(1)?;
                (0, if s == 1 { -1 } else { 1 })
            } else {
                let (v, _) = table.read(b)?.ok_or(Halt::Error)?;
                match v {
                    EOB => break,
                    ESCAPE => {
                        let run = b.get(6)? as i32;
                        let level = if mpeg1 {
                            match b.signed(8)? {
                                0 => b.get(8)? as i32,
                                -128 => b.get(8)? as i32 - 256,
                                l => l,
                            }
                        } else {
                            b.signed(12)?
                        };
                        (run, level)
                    }
                    v => {
                        let s = b.get(1)?;
                        let level = v & 0xFF;
                        (v >> 8, if s == 1 { -level } else { level })
                    }
                }
            };
            first = false;
            i += run + 1;
            if i > 63 {
                return Err(Halt::Error);
            }
            let n = scan[i as usize];
            let m = level * qs * i32::from(w[n]);
            let mut v = if intra.is_some() {
                // (2·QF·W·qs) / 32, as quantiser_scale is doubled here.
                m / 16
            } else {
                ((2 * level + level.signum()) * qs * i32::from(w[n])) / 32
            };
            if mpeg1 && v & 1 == 0 && v != 0 {
                v -= v.signum();
            }
            f[n] = v.clamp(-2048, 2047);
        }
        if !mpeg1 {
            // Mismatch control.
            if f.iter().sum::<i32>() & 1 == 0 {
                f[63] ^= 1;
            }
        }
        Ok(f)
    }

    /// A macroblock's six blocks: luma in a 16×16 (frame or field DCT),
    /// chroma 8×8; `cbp`'s bits (from 32, the first block) say which are
    /// coded.
    fn macroblock(&mut self, b: &mut Bits, intra: bool, cbp: u32, qs: i32, field: bool) -> Step<[i32; 384]> {
        let mut mb = [0i32; 384];
        let (lo, hi) = if intra { (0, 255) } else { (-256, 255) };
        for k in 0..6 {
            if cbp & (32 >> k) == 0 {
                continue;
            }
            let comp = match k {
                0..=3 => 0,
                4 => 1,
                _ => 2,
            };
            let f = self.block(b, intra.then_some(comp), qs)?;
            let s = idct(&f, lo, hi);
            for r in 0..8 {
                for c in 0..8 {
                    let at = match k {
                        0..=3 => {
                            let x = (k & 1) * 8 + c;
                            let y = if field { (k >> 1) + 2 * r } else { (k >> 1) * 8 + r };
                            y * 16 + x
                        }
                        4 => 256 + r * 8 + c,
                        _ => 320 + r * 8 + c,
                    };
                    mb[at] = s[r * 8 + c];
                }
            }
        }
        Ok(mb)
    }

    /// RGB32 (with alpha by the thresholds) or RGB16 (dithered or not) from
    /// a macroblock's samples.
    fn csc(&self, raw: &Raw8, rgb16: bool, dither: bool, out: &mut Vec<u8>) {
        for y in 0..16 {
            for x in 0..16 {
                let c = (y / 2) * 8 + x / 2;
                let [r, g, bl] = rgb(raw[y * 16 + x], raw[256 + c], raw[320 + c]);
                let below = |t: i32| r < t && g < t && bl < t;
                let (rgbv, a) = if below(self.th[0]) {
                    ([0, 0, 0], 0)
                } else if below(self.th[1]) {
                    ([r, g, bl], 0x40)
                } else {
                    ([r, g, bl], 0x80)
                };
                if rgb16 {
                    let d = if dither { DITHER[y & 3][x & 3] } else { 0 };
                    let p = pack16(rgbv, a, d);
                    out.extend_from_slice(&p.to_le_bytes());
                } else {
                    out.extend_from_slice(&[rgbv[0] as u8, rgbv[1] as u8, rgbv[2] as u8, a as u8]);
                }
            }
        }
    }
}

fn pack16([r, g, b]: [i32; 3], a: u32, d: i32) -> u16 {
    let c = |v: i32| ((v + d).clamp(0, 255) >> 3) as u16;
    c(r) | c(g) << 5 | c(b) << 10 | u16::from(a >= 0x80) << 15
}

fn setiq(dec: &mut Decoder, b: &mut Bits, fb: u32, nonintra: u32) -> Step<()> {
    b.skip(fb)?;
    let mut m = vec![0u8; 64];
    for &n in &ZIGZAG {
        m[n] = b.get(8)? as u8;
    }
    dec.iq[nonintra as usize] = m;
    Ok(())
}

/// VDEC: a variable-length code's value, and its length above (as the IPU
/// gives them; escape and stuffing alone).
fn vdec(dec: &mut Decoder, b: &mut Bits, fb: u32, tbl: u32) -> Step<u32> {
    b.skip(fb)?;
    let t = tables();
    let found = match tbl {
        0 => match t.mbai.read(b)? {
            Some((v @ (MBAI_ESCAPE | MBAI_STUFFING), _)) => return Ok(v as u32),
            other => other,
        },
        1 => {
            let pct = dec.pct().clamp(1, 4) as usize - 1;
            t.mbt[pct].read(b)?
        }
        2 => match t.motion.read(b)? {
            Some((0, len)) => Some((0, len)),
            Some((v, len)) => {
                let s = b.get(1)?;
                Some((if s == 1 { -v } else { v }, len + 1))
            }
            None => None,
        },
        _ => t.dmv.read(b)?,
    };
    let (v, len) = found.ok_or(Halt::Error)?;
    Ok((v as u32 & 0xFFFF) | len << 16)
}

/// BDEC: a macroblock, as 16-bit samples (luma 16×16, Cb, Cr).
fn bdec(dec: &mut Decoder, b: &mut Bits, job: &Job, out: &mut Vec<u8>) -> Step<()> {
    let opt = job.opt;
    b.skip(opt & 0x3F)?;
    if opt & (1 << 26) != 0 {
        dec.reset_dc();
    }
    let intra = opt & (1 << 27) != 0;
    let cbp = if intra {
        0x3F
    } else {
        let (v, _) = tables().cbp.read(b)?.ok_or(Halt::Error)?;
        v as u32
    };
    dec.ctrl = (dec.ctrl & !(0x3F << 8)) | cbp << 8;
    let qs = dec.qscale(opt >> 16);
    let mb = dec.macroblock(b, intra, cbp, qs, opt & (1 << 25) != 0)?;
    for v in mb {
        out.extend_from_slice(&(v as i16).to_le_bytes());
    }
    Ok(())
}

/// IDEC, one macroblock of an intra slice (after the first, its address
/// increment first): whether the slice is done.
fn idec(dec: &mut Decoder, b: &mut Bits, job: &mut Job, out: &mut Vec<u8>) -> Step<bool> {
    let opt = job.opt;
    if !job.begun {
        b.skip(opt & 0x3F)?;
        dec.reset_dc();
        job.qsc = (opt >> 16) & 31;
        job.begun = true;
    }
    let t = tables();
    if job.count > 0 {
        // A start code ends the slice.
        if b.peek(23)? == 0 {
            dec.ctrl |= CTRL_SCD;
            return Ok(true);
        }
        loop {
            match t.mbai.read(b)? {
                Some((MBAI_ESCAPE | MBAI_STUFFING, _)) => continue,
                Some(_) => break,
                None => return Err(Halt::Error),
            }
        }
    }
    let (mbt, _) = t.mbt[0].read(b)?.ok_or(Halt::Error)?;
    if mbt & MB_QUANT != 0 {
        job.qsc = b.get(5)?;
    }
    let field = opt & (1 << 24) != 0 && b.get(1)? == 1;
    debug_assert!(mbt & MB_INTRA != 0);
    let qs = dec.qscale(job.qsc);
    let mb = dec.macroblock(b, true, 0x3F, qs, field)?;
    let raw: Raw8 = std::array::from_fn(|k| mb[k] as u8);
    dec.csc(&raw, opt & (1 << 27) != 0, opt & (1 << 26) != 0, out);
    job.count += 1;
    Ok(false)
}

/// CSC (RAW8 to RGB) and PACK (RGB32 to RGB16 or 4-bit indices), a
/// macroblock at a time: whether all are done.
fn convert(dec: &mut Decoder, b: &mut Bits, job: &mut Job, out: &mut Vec<u8>) -> Step<bool> {
    let opt = job.opt;
    if !job.begun {
        job.count = opt & 0x7FF;
        job.begun = true;
    }
    if job.count == 0 {
        return Ok(true);
    }
    let (dither, rgb16) = (opt & (1 << 26) != 0, opt & (1 << 27) != 0);
    if job.code == 0x7 {
        let mut raw: Raw8 = [0; 384];
        for v in &mut raw {
            *v = b.get(8)? as u8;
        }
        dec.csc(&raw, rgb16, dither, out);
    } else {
        let mut px = [0u32; 256];
        for p in &mut px {
            *p = b.get(32)?.swap_bytes();
        }
        let mut nibbles = Vec::with_capacity(256);
        for (k, &p) in px.iter().enumerate() {
            let d = if dither { DITHER[(k / 16) & 3][k & 3] } else { 0 };
            let rgbv = [(p & 0xFF) as i32, ((p >> 8) & 0xFF) as i32, ((p >> 16) & 0xFF) as i32];
            let c16 = pack16(rgbv, p >> 24, d);
            if rgb16 {
                out.extend_from_slice(&c16.to_le_bytes());
            } else {
                // The nearest colour of the table.
                let parts = |c: u16| [c & 31, (c >> 5) & 31, (c >> 10) & 31].map(i32::from);
                let want = parts(c16);
                let best = (0..16)
                    .min_by_key(|&i| {
                        let have = parts(dec.vq[i]);
                        (0..3).map(|j| (have[j] - want[j]).pow(2)).sum::<i32>()
                    })
                    .unwrap_or(0) as u8;
                nibbles.push(best);
            }
        }
        for pair in nibbles.chunks(2) {
            out.push(pair[0] | pair.get(1).copied().unwrap_or(0) << 4);
        }
    }
    job.count -= 1;
    Ok(job.count == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code of a list of tables' (TEXEL_VLC: lines of "dct T BITS
    /// RUN LEVEL", "dct T BITS esc|eob 0" or "cbp 0 BITS VALUE 0", from
    /// another copy of the standard's tables) decodes as the list says.
    #[test]
    #[ignore = "needs TEXEL_VLC, a list of codes"]
    fn codes_match_another_copy_of_the_tables() {
        let path = std::env::var("TEXEL_VLC").expect("TEXEL_VLC");
        let mut wrong = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            let bits = f[2];
            let mut bytes = [0u8; 16];
            for (k, c) in bits.chars().enumerate() {
                if c == '1' {
                    bytes[k / 8] |= 0x80 >> (k % 8);
                }
            }
            let mut input = VecDeque::from([u128::from_le_bytes(bytes), 0]);
            let mut pull = || None;
            let mut b = Bits {
                input: &mut input,
                pos: 0,
                pull: &mut pull,
            };
            let t = tables();
            let vlc = if f[0] == "dct" {
                &t.dct[f[1].parse::<usize>().unwrap()]
            } else {
                &t.cbp
            };
            let want = match (f[0], f[3]) {
                ("dct", "esc") => ESCAPE,
                ("dct", "eob") => EOB,
                ("dct", run) => rl(run.parse().unwrap(), f[4].parse().unwrap()),
                (_, v) => v.parse().unwrap(),
            };
            let got = vlc.read(&mut b).ok().flatten();
            if got != Some((want, bits.len() as u32)) {
                wrong.push(format!("{line}: got {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{} codes differ:\n{}", wrong.len(), wrong.join("\n"));
    }

    /// The code tables are prefix-free (`Vlc::new` checks), and complete
    /// where the standard's are.
    #[test]
    fn tables_build() {
        let t = tables();
        assert_eq!(t.dct[0].bits, 16);
        assert_eq!(t.dct[1].bits, 16);
        // Every 6-bit motion code prefix decodes (B.10 is complete but
        // for its longest codes' unused ones).
        assert!(t.motion.table.iter().filter(|e| e.0 != 0).count() > 1000);
    }

    fn run(ipu: &mut Ipu) {
        ipu.run(&mut || None);
    }

    /// FDEC reads 32 bits after FB, without consuming them; BCLR sets the
    /// bit pointer.
    #[test]
    fn fdec_and_bclr() {
        let mut ipu = Ipu::default();
        let bytes: [u8; 16] = std::array::from_fn(|k| (0x10 + k) as u8);
        ipu.write_fifo(u128::from_le_bytes(bytes));
        ipu.write_cmd(0x4000_0008);
        run(&mut ipu);
        assert!(!ipu.busy());
        assert_eq!(ipu.read_cmd().0, 0x1112_1314);
        assert_eq!(ipu.read_bp() & 0x7F, 8);
        // Past the end (bit 134): waits for more.
        ipu.write_cmd(0x4000_0000 | 63);
        run(&mut ipu);
        ipu.write_cmd(0x4000_0000 | 63);
        run(&mut ipu);
        assert!(ipu.busy());
        assert_eq!(ipu.read_bp() & 0x7F, 71, "back where the command began");
        ipu.write_fifo(0xAA);
        run(&mut ipu);
        assert!(!ipu.busy());
        assert_eq!(ipu.read_cmd().0, 0x8000_0000);
        ipu.write_cmd(0x0000_0010);
        run(&mut ipu);
        assert_eq!(ipu.read_bp(), 16);
    }

    /// A DC-only intra block: flat; gray through CSC.
    #[test]
    fn flat_block_and_gray() {
        let mut f = [0i32; 64];
        f[0] = 128 * 8;
        assert!(idct(&f, 0, 255).iter().all(|&v| v == 128));
        assert_eq!(rgb(235, 128, 128), [255, 255, 255]);
        assert_eq!(rgb(16, 128, 128), [0, 0, 0]);
        let [r, g, b] = rgb(126, 128, 128);
        assert!(r == g && g == b && (127..=129).contains(&r));
    }

    /// An intra BDEC of a coded macroblock: DC sizes and EOBs only.
    #[test]
    fn bdec_dc_only() {
        let mut ipu = Ipu::default();
        // Four luma blocks: dc size 0 ("100"), EOB ("10"); two chroma: dc
        // size 0 ("00"), EOB ("10").
        let bits = "10010".repeat(4) + &"0010".repeat(2);
        let mut v = bits.clone();
        while !v.len().is_multiple_of(128) {
            v.push('0');
        }
        let q = u128::from_str_radix(&v, 2).unwrap().swap_bytes();
        ipu.write_fifo(q);
        ipu.write_cmd(0x2000_0000 | 1 << 27 | 1 << 26 | 1 << 16);
        run(&mut ipu);
        assert!(!ipu.busy(), "done");
        assert_eq!(ipu.dec.ctrl & CTRL_ECD, 0);
        assert_eq!(ipu.output.len(), 48);
        let first = ipu.output[0].to_le_bytes();
        assert_eq!(i16::from_le_bytes([first[0], first[1]]), 128);
        assert_eq!(ipu.read_bp() as usize & 0x7F, bits.len());
    }
}
