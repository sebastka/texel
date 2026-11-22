//! The GS's local memory: 4 MiB, addressed in pages of 8 KiB and blocks of
//! 256 bytes, with each pixel format laid out its own way inside them
//! ("swizzled"). Games rely on the layouts: they upload in one format and
//! read in another, so every format is stored as the GS stores it.
//!
//! Base pointers are in blocks (64 words) and buffer widths in units of 64
//! pixels, as in the GS registers.

pub const MEM_SIZE: usize = 4 * 1024 * 1024;
const WORDS: u32 = (MEM_SIZE / 4) as u32;

/// Pixel storage formats.
pub mod psm {
    pub const CT32: u32 = 0x00;
    pub const CT24: u32 = 0x01;
    pub const CT16: u32 = 0x02;
    pub const CT16S: u32 = 0x0A;
    pub const T8: u32 = 0x13;
    pub const T4: u32 = 0x14;
    pub const T8H: u32 = 0x1B;
    pub const T4HL: u32 = 0x24;
    pub const T4HH: u32 = 0x2C;
    pub const Z32: u32 = 0x30;
    pub const Z24: u32 = 0x31;
    pub const Z16: u32 = 0x32;
    pub const Z16S: u32 = 0x3A;
}

// Blocks within a page, by block row and column.
const BLOCK32: [[u32; 8]; 4] = [
    [0, 1, 4, 5, 16, 17, 20, 21],
    [2, 3, 6, 7, 18, 19, 22, 23],
    [8, 9, 12, 13, 24, 25, 28, 29],
    [10, 11, 14, 15, 26, 27, 30, 31],
];
const BLOCK32Z: [[u32; 8]; 4] = [
    [24, 25, 28, 29, 8, 9, 12, 13],
    [26, 27, 30, 31, 10, 11, 14, 15],
    [16, 17, 20, 21, 0, 1, 4, 5],
    [18, 19, 22, 23, 2, 3, 6, 7],
];
const BLOCK16: [[u32; 4]; 8] = [
    [0, 2, 8, 10],
    [1, 3, 9, 11],
    [4, 6, 12, 14],
    [5, 7, 13, 15],
    [16, 18, 24, 26],
    [17, 19, 25, 27],
    [20, 22, 28, 30],
    [21, 23, 29, 31],
];
const BLOCK16S: [[u32; 4]; 8] = [
    [0, 2, 16, 18],
    [1, 3, 17, 19],
    [8, 10, 24, 26],
    [9, 11, 25, 27],
    [4, 6, 20, 22],
    [5, 7, 21, 23],
    [12, 14, 28, 30],
    [13, 15, 29, 31],
];
const BLOCK16Z: [[u32; 4]; 8] = [
    [24, 26, 16, 18],
    [25, 27, 17, 19],
    [28, 30, 20, 22],
    [29, 31, 21, 23],
    [8, 10, 0, 2],
    [9, 11, 1, 3],
    [12, 14, 4, 6],
    [13, 15, 5, 7],
];
const BLOCK16SZ: [[u32; 4]; 8] = [
    [24, 26, 8, 10],
    [25, 27, 9, 11],
    [16, 18, 0, 2],
    [17, 19, 1, 3],
    [28, 30, 12, 14],
    [29, 31, 13, 15],
    [20, 22, 4, 6],
    [21, 23, 5, 7],
];

/// Words within a block of 8×8 32-bit pixels.
const COLUMN32: [[u32; 8]; 8] = [
    [0, 1, 4, 5, 8, 9, 12, 13],
    [2, 3, 6, 7, 10, 11, 14, 15],
    [16, 17, 20, 21, 24, 25, 28, 29],
    [18, 19, 22, 23, 26, 27, 30, 31],
    [32, 33, 36, 37, 40, 41, 44, 45],
    [34, 35, 38, 39, 42, 43, 46, 47],
    [48, 49, 52, 53, 56, 57, 60, 61],
    [50, 51, 54, 55, 58, 59, 62, 63],
];

/// Halfwords within a block of 16×8 16-bit pixels.
const COLUMN16: [[u32; 16]; 8] = [
    [0, 2, 8, 10, 16, 18, 24, 26, 1, 3, 9, 11, 17, 19, 25, 27],
    [4, 6, 12, 14, 20, 22, 28, 30, 5, 7, 13, 15, 21, 23, 29, 31],
    [32, 34, 40, 42, 48, 50, 56, 58, 33, 35, 41, 43, 49, 51, 57, 59],
    [36, 38, 44, 46, 52, 54, 60, 62, 37, 39, 45, 47, 53, 55, 61, 63],
    [64, 66, 72, 74, 80, 82, 88, 90, 65, 67, 73, 75, 81, 83, 89, 91],
    [68, 70, 76, 78, 84, 86, 92, 94, 69, 71, 77, 79, 85, 87, 93, 95],
    [
        96, 98, 104, 106, 112, 114, 120, 122, 97, 99, 105, 107, 113, 115, 121, 123,
    ],
    [
        100, 102, 108, 110, 116, 118, 124, 126, 101, 103, 109, 111, 117, 119, 125, 127,
    ],
];

/// Bytes within a block of 16×16 8-bit pixels.
const COLUMN8: [[u32; 16]; 16] = [
    [0, 4, 16, 20, 32, 36, 48, 52, 2, 6, 18, 22, 34, 38, 50, 54],
    [8, 12, 24, 28, 40, 44, 56, 60, 10, 14, 26, 30, 42, 46, 58, 62],
    [33, 37, 49, 53, 1, 5, 17, 21, 35, 39, 51, 55, 3, 7, 19, 23],
    [41, 45, 57, 61, 9, 13, 25, 29, 43, 47, 59, 63, 11, 15, 27, 31],
    [96, 100, 112, 116, 64, 68, 80, 84, 98, 102, 114, 118, 66, 70, 82, 86],
    [104, 108, 120, 124, 72, 76, 88, 92, 106, 110, 122, 126, 74, 78, 90, 94],
    [65, 69, 81, 85, 97, 101, 113, 117, 67, 71, 83, 87, 99, 103, 115, 119],
    [73, 77, 89, 93, 105, 109, 121, 125, 75, 79, 91, 95, 107, 111, 123, 127],
    [
        128, 132, 144, 148, 160, 164, 176, 180, 130, 134, 146, 150, 162, 166, 178, 182,
    ],
    [
        136, 140, 152, 156, 168, 172, 184, 188, 138, 142, 154, 158, 170, 174, 186, 190,
    ],
    [
        161, 165, 177, 181, 129, 133, 145, 149, 163, 167, 179, 183, 131, 135, 147, 151,
    ],
    [
        169, 173, 185, 189, 137, 141, 153, 157, 171, 175, 187, 191, 139, 143, 155, 159,
    ],
    [
        224, 228, 240, 244, 192, 196, 208, 212, 226, 230, 242, 246, 194, 198, 210, 214,
    ],
    [
        232, 236, 248, 252, 200, 204, 216, 220, 234, 238, 250, 254, 202, 206, 218, 222,
    ],
    [
        193, 197, 209, 213, 225, 229, 241, 245, 195, 199, 211, 215, 227, 231, 243, 247,
    ],
    [
        201, 205, 217, 221, 233, 237, 249, 253, 203, 207, 219, 223, 235, 239, 251, 255,
    ],
];

/// Nibbles within a block of 32×16 4-bit pixels (the first 8 rows; the
/// next 8 are the same plus 256).
const COLUMN4: [[u32; 32]; 8] = [
    [
        0, 8, 32, 40, 64, 72, 96, 104, 2, 10, 34, 42, 66, 74, 98, 106, 4, 12, 36, 44, 68, 76, 100, 108, 6, 14, 38, 46,
        70, 78, 102, 110,
    ],
    [
        16, 24, 48, 56, 80, 88, 112, 120, 18, 26, 50, 58, 82, 90, 114, 122, 20, 28, 52, 60, 84, 92, 116, 124, 22, 30,
        54, 62, 86, 94, 118, 126,
    ],
    [
        65, 73, 97, 105, 1, 9, 33, 41, 67, 75, 99, 107, 3, 11, 35, 43, 69, 77, 101, 109, 5, 13, 37, 45, 71, 79, 103,
        111, 7, 15, 39, 47,
    ],
    [
        81, 89, 113, 121, 17, 25, 49, 57, 83, 91, 115, 123, 19, 27, 51, 59, 85, 93, 117, 125, 21, 29, 53, 61, 87, 95,
        119, 127, 23, 31, 55, 63,
    ],
    [
        192, 200, 224, 232, 128, 136, 160, 168, 194, 202, 226, 234, 130, 138, 162, 170, 196, 204, 228, 236, 132, 140,
        164, 172, 198, 206, 230, 238, 134, 142, 166, 174,
    ],
    [
        208, 216, 240, 248, 144, 152, 176, 184, 210, 218, 242, 250, 146, 154, 178, 186, 212, 220, 244, 252, 148, 156,
        180, 188, 214, 222, 246, 254, 150, 158, 182, 190,
    ],
    [
        129, 137, 161, 169, 193, 201, 225, 233, 131, 139, 163, 171, 195, 203, 227, 235, 133, 141, 165, 173, 197, 205,
        229, 237, 135, 143, 167, 175, 199, 207, 231, 239,
    ],
    [
        145, 153, 177, 185, 209, 217, 241, 249, 147, 155, 179, 187, 211, 219, 243, 251, 149, 157, 181, 189, 213, 221,
        245, 253, 151, 159, 183, 191, 215, 223, 247, 255,
    ],
];

/// Word address of a 32-bit pixel (CT32, CT24, Z32, Z24, and the 8H/4H
/// formats, which live in their upper bits).
pub fn addr32(bp: u32, bw: u32, x: u32, y: u32, z: bool) -> u32 {
    let page = (x / 64) + (y / 32) * bw.max(1);
    let table = if z { &BLOCK32Z } else { &BLOCK32 };
    let block = table[((y % 32) / 8) as usize][((x % 64) / 8) as usize];
    let column = COLUMN32[(y % 8) as usize][(x % 8) as usize];
    (bp * 64 + page * 2048 + block * 64 + column) % WORDS
}

/// Halfword address of a 16-bit pixel (CT16, CT16S, Z16, Z16S).
pub fn addr16(bp: u32, bw: u32, x: u32, y: u32, format: u32) -> u32 {
    let page = (x / 64) + (y / 64) * bw.max(1);
    let table = match format {
        psm::CT16S => &BLOCK16S,
        psm::Z16 => &BLOCK16Z,
        psm::Z16S => &BLOCK16SZ,
        _ => &BLOCK16,
    };
    let block = table[((y % 64) / 8) as usize][((x % 64) / 16) as usize];
    let column = COLUMN16[(y % 8) as usize][(x % 16) as usize];
    (bp * 128 + page * 4096 + block * 128 + column) % (WORDS * 2)
}

/// Byte address of an 8-bit pixel.
pub fn addr8(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x / 128) + (y / 64) * (bw / 2).max(1);
    let block = BLOCK32[((y % 64) / 16) as usize][((x % 128) / 16) as usize];
    let column = COLUMN8[(y % 16) as usize][(x % 16) as usize];
    (bp * 256 + page * 8192 + block * 256 + column) % (WORDS * 4)
}

/// Nibble address of a 4-bit pixel.
pub fn addr4(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x / 128) + (y / 128) * (bw / 2).max(1);
    let block = BLOCK16[((y % 128) / 16) as usize][((x % 128) / 32) as usize];
    let row = (y % 16) as usize;
    let column = COLUMN4[row % 8][(x % 32) as usize] + if row >= 8 { 256 } else { 0 };
    (bp * 512 + page * 16384 + block * 512 + column) % (WORDS * 8)
}

/// The local memory.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub words: Box<[u32]>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            words: vec![0; WORDS as usize].into_boxed_slice(),
        }
    }
}

impl Memory {
    /// Reads a pixel of any format, as stored (not expanded).
    pub fn read(&self, format: u32, bp: u32, bw: u32, x: u32, y: u32) -> u32 {
        match format {
            psm::CT32 | psm::Z32 => self.words[addr32(bp, bw, x, y, format == psm::Z32) as usize],
            psm::CT24 | psm::Z24 => self.words[addr32(bp, bw, x, y, format == psm::Z24) as usize] & 0xFF_FFFF,
            psm::CT16 | psm::CT16S | psm::Z16 | psm::Z16S => {
                let a = addr16(bp, bw, x, y, format);
                (self.words[(a / 2) as usize] >> ((a & 1) * 16)) & 0xFFFF
            }
            psm::T8 => {
                let a = addr8(bp, bw, x, y);
                (self.words[(a / 4) as usize] >> ((a & 3) * 8)) & 0xFF
            }
            psm::T4 => {
                let a = addr4(bp, bw, x, y);
                (self.words[(a / 8) as usize] >> ((a & 7) * 4)) & 0xF
            }
            psm::T8H => self.words[addr32(bp, bw, x, y, false) as usize] >> 24,
            psm::T4HL => (self.words[addr32(bp, bw, x, y, false) as usize] >> 24) & 0xF,
            psm::T4HH => self.words[addr32(bp, bw, x, y, false) as usize] >> 28,
            _ => 0,
        }
    }

    /// Writes a pixel of any format (24-bit formats keep the upper byte).
    pub fn write(&mut self, format: u32, bp: u32, bw: u32, x: u32, y: u32, v: u32) {
        match format {
            psm::CT32 | psm::Z32 => self.words[addr32(bp, bw, x, y, format == psm::Z32) as usize] = v,
            psm::CT24 | psm::Z24 => {
                let w = &mut self.words[addr32(bp, bw, x, y, format == psm::Z24) as usize];
                *w = (*w & 0xFF00_0000) | (v & 0xFF_FFFF);
            }
            psm::CT16 | psm::CT16S | psm::Z16 | psm::Z16S => {
                let a = addr16(bp, bw, x, y, format);
                let w = &mut self.words[(a / 2) as usize];
                let shift = (a & 1) * 16;
                *w = (*w & !(0xFFFF << shift)) | ((v & 0xFFFF) << shift);
            }
            psm::T8 => {
                let a = addr8(bp, bw, x, y);
                let w = &mut self.words[(a / 4) as usize];
                let shift = (a & 3) * 8;
                *w = (*w & !(0xFF << shift)) | ((v & 0xFF) << shift);
            }
            psm::T4 => {
                let a = addr4(bp, bw, x, y);
                let w = &mut self.words[(a / 8) as usize];
                let shift = (a & 7) * 4;
                *w = (*w & !(0xF << shift)) | ((v & 0xF) << shift);
            }
            psm::T8H => {
                let w = &mut self.words[addr32(bp, bw, x, y, false) as usize];
                *w = (*w & 0x00FF_FFFF) | ((v & 0xFF) << 24);
            }
            psm::T4HL => {
                let w = &mut self.words[addr32(bp, bw, x, y, false) as usize];
                *w = (*w & !0x0F00_0000) | ((v & 0xF) << 24);
            }
            psm::T4HH => {
                let w = &mut self.words[addr32(bp, bw, x, y, false) as usize];
                *w = (*w & !0xF000_0000) | ((v & 0xF) << 28);
            }
            _ => {}
        }
    }
}

/// Bits per pixel of a format.
pub fn bits(format: u32) -> u32 {
    match format {
        psm::CT32 | psm::Z32 | psm::CT24 | psm::Z24 => 32,
        psm::CT16 | psm::CT16S | psm::Z16 | psm::Z16S => 16,
        psm::T8 | psm::T8H => 8,
        psm::T4 | psm::T4HL | psm::T4HH => 4,
        _ => 32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_bijective_on_a_page() {
        // Each format maps a page's pixels onto distinct units.
        let mut seen = std::collections::HashSet::new();
        for y in 0..32 {
            for x in 0..64 {
                assert!(seen.insert(addr32(0, 1, x, y, false)));
            }
        }
        assert_eq!(seen.len(), 2048);
        seen.clear();
        for y in 0..64 {
            for x in 0..64 {
                assert!(seen.insert(addr16(0, 1, x, y, psm::CT16)));
            }
        }
        assert_eq!(seen.len(), 4096);
        seen.clear();
        for y in 0..64 {
            for x in 0..128 {
                assert!(seen.insert(addr8(0, 2, x, y)));
            }
        }
        assert_eq!(seen.len(), 8192);
        seen.clear();
        for y in 0..128 {
            for x in 0..128 {
                assert!(seen.insert(addr4(0, 2, x, y)));
            }
        }
        assert_eq!(seen.len(), 16384);
    }

    /// The layouts as bit operations, as the GPU renderer's shader computes
    /// them: the same as the tables, for every position.
    #[test]
    fn layout_formulas_match_the_tables() {
        let block32 = |x: u32, y: u32| {
            let (bx, by) = ((x >> 3) & 7, (y >> 3) & 3);
            (bx & 1) | ((by & 1) << 1) | ((bx & 2) << 1) | ((by & 2) << 2) | ((bx & 4) << 2)
        };
        let column32 = |x: u32, y: u32| (x & 1) | ((y & 1) << 1) | ((x & 6) << 1) | ((y & 6) << 3);
        let block16 = |x: u32, y: u32| {
            let (bx, by) = ((x >> 4) & 3, (y >> 3) & 7);
            (by & 1) | ((bx & 1) << 1) | ((by & 2) << 1) | ((bx & 2) << 2) | ((by & 4) << 2)
        };
        let block16s = |x: u32, y: u32| {
            let (bx, by) = ((x >> 4) & 3, (y >> 3) & 7);
            (by & 1) | ((bx & 1) << 1) | (by & 4) | ((by & 2) << 2) | ((bx & 2) << 3)
        };
        let column16 =
            |x: u32, y: u32| ((x & 1) << 1) | ((x & 6) << 2) | ((x & 8) >> 3) | ((y & 1) << 2) | ((y & 6) << 4);
        let column8 = |x: u32, y: u32| {
            let flip = ((x >> 2) & 1) ^ ((y >> 1) & 1) ^ ((y >> 2) & 1);
            ((x & 1) << 2)
                | ((x & 2) << 3)
                | (flip << 5)
                | ((x & 8) >> 2)
                | ((y >> 1) & 1)
                | ((y & 1) << 3)
                | ((y & 12) << 4)
        };
        let column4 = |x: u32, y: u32| {
            let flip = ((x >> 2) & 1) ^ ((y >> 1) & 1) ^ ((y >> 2) & 1);
            ((x & 1) << 3)
                | ((x & 2) << 4)
                | (flip << 6)
                | ((x & 24) >> 2)
                | ((y >> 1) & 1)
                | ((y & 1) << 4)
                | ((y & 12) << 5)
        };
        for y in 0..32 {
            for x in 0..64 {
                assert_eq!(
                    block32(x, y),
                    BLOCK32[(y / 8) as usize][(x / 8) as usize],
                    "block32 {x},{y}"
                );
                assert_eq!(block32(x, y) ^ 24, BLOCK32Z[(y / 8) as usize][(x / 8) as usize]);
            }
        }
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(column32(x, y), COLUMN32[y as usize][x as usize], "column32 {x},{y}");
            }
            for x in 0..16 {
                assert_eq!(column16(x, y), COLUMN16[y as usize][x as usize], "column16 {x},{y}");
            }
        }
        for y in 0..64 {
            for x in 0..64 {
                let (r, c) = ((y / 8) as usize, (x / 16) as usize);
                assert_eq!(block16(x, y), BLOCK16[r][c], "block16 {x},{y}");
                assert_eq!(block16(x, y) ^ 24, BLOCK16Z[r][c]);
                assert_eq!(block16s(x, y), BLOCK16S[r][c], "block16s {x},{y}");
                assert_eq!(block16s(x, y) ^ 24, BLOCK16SZ[r][c]);
            }
        }
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(column8(x, y), COLUMN8[y as usize][x as usize], "column8 {x},{y}");
            }
            for x in 0..32 {
                let expected = COLUMN4[(y % 8) as usize][x as usize] + if y >= 8 { 256 } else { 0 };
                assert_eq!(column4(x, y), expected, "column4 {x},{y}");
            }
        }
    }

    #[test]
    fn formats_share_memory() {
        let mut m = Memory::default();
        m.write(psm::CT32, 0, 1, 1, 0, 0x1122_3344);
        assert_eq!(m.read(psm::CT32, 0, 1, 1, 0), 0x1122_3344);
        // A 32-bit pixel is word 1 of block 0: 16-bit pixels 2 and 3 of it.
        m.write(psm::T8H, 0, 1, 1, 0, 0xAB);
        assert_eq!(m.read(psm::CT24, 0, 1, 1, 0), 0x22_3344);
        assert_eq!(m.read(psm::T8H, 0, 1, 1, 0), 0xAB);
        m.write(psm::T4, 32, 2, 5, 3, 0x9);
        assert_eq!(m.read(psm::T4, 32, 2, 5, 3), 0x9);
    }
}
