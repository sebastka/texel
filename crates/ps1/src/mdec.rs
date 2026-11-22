//! Motion Decoder: run-length decoding, inverse DCT and YUV to RGB for the
//! macroblocks of FMVs and compressed pictures.

use std::collections::VecDeque;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
    47, 55, 62, 63,
];

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Command {
    None,
    Decode,
    SetQuant { color: bool },
    SetScale,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Mdec {
    command: Command,
    /// Words still expected for the current command.
    remaining: u32,
    input: Vec<u32>,
    output: VecDeque<u32>,
    depth: u32,
    signed: bool,
    set_bit15: bool,
    #[serde(with = "serde_big_array::BigArray")]
    quant_luma: [u8; 64],
    #[serde(with = "serde_big_array::BigArray")]
    quant_chroma: [u8; 64],
    #[serde(with = "serde_big_array::BigArray")]
    scale: [i16; 64],
    dma_in: bool,
    dma_out: bool,
}

impl Default for Mdec {
    fn default() -> Self {
        Self::new()
    }
}

impl Mdec {
    pub fn new() -> Self {
        Self {
            command: Command::None,
            remaining: 0,
            input: Vec::new(),
            output: VecDeque::new(),
            depth: 0,
            signed: false,
            set_bit15: false,
            quant_luma: [1; 64],
            quant_chroma: [1; 64],
            scale: [0; 64],
            dma_in: false,
            dma_out: false,
        }
    }

    pub fn status(&self) -> u32 {
        let mut s = 0;
        s |= u32::from(self.output.is_empty()) << 31;
        s |= u32::from(self.remaining > 0) << 29;
        s |= u32::from(self.dma_in && self.remaining > 0) << 28;
        s |= u32::from(self.dma_out && !self.output.is_empty()) << 27;
        s |= self.depth << 25;
        s |= u32::from(self.signed) << 24;
        s |= u32::from(self.set_bit15) << 23;
        s |= self.remaining.wrapping_sub(1) & 0xffff;
        s
    }

    pub fn write_control(&mut self, v: u32) {
        if v & 0x8000_0000 != 0 {
            self.command = Command::None;
            self.remaining = 0;
            self.input.clear();
            self.output.clear();
        }
        self.dma_in = v & 0x4000_0000 != 0;
        self.dma_out = v & 0x2000_0000 != 0;
    }

    pub fn output_ready(&self) -> bool {
        !self.output.is_empty()
    }

    pub fn read_data(&mut self) -> u32 {
        self.output.pop_front().unwrap_or(0)
    }

    pub fn write_command(&mut self, v: u32) {
        if self.remaining > 0 {
            self.input.push(v);
            self.remaining -= 1;
            if self.remaining == 0 {
                self.run();
            }
            return;
        }
        self.input.clear();
        self.depth = (v >> 27) & 3;
        self.signed = v & (1 << 26) != 0;
        self.set_bit15 = v & (1 << 25) != 0;
        (self.command, self.remaining) = match v >> 29 {
            1 => (Command::Decode, v & 0xffff),
            2 => (
                Command::SetQuant { color: v & 1 != 0 },
                if v & 1 != 0 { 32 } else { 16 },
            ),
            3 => (Command::SetScale, 32),
            _ => (Command::None, 0),
        };
    }

    fn run(&mut self) {
        let bytes: Vec<u8> = self.input.iter().flat_map(|w| w.to_le_bytes()).collect();
        match self.command {
            Command::SetQuant { color } => {
                self.quant_luma.copy_from_slice(&bytes[..64]);
                if color {
                    self.quant_chroma.copy_from_slice(&bytes[64..128]);
                }
            }
            Command::SetScale => {
                for (i, s) in self.scale.iter_mut().enumerate() {
                    *s = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                }
            }
            Command::Decode => self.decode(&bytes),
            Command::None => {}
        }
        self.command = Command::None;
    }

    fn decode(&mut self, bytes: &[u8]) {
        let halfwords: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let mut pos = 0;
        let color = self.depth >= 2;
        loop {
            if color {
                let mut blocks = [[0i32; 64]; 6];
                for (b, block) in blocks.iter_mut().enumerate() {
                    let qt = if b < 2 { self.quant_chroma } else { self.quant_luma };
                    if !self.decode_block(&halfwords, &mut pos, block, &qt) {
                        return;
                    }
                }
                self.emit_color(&blocks);
            } else {
                let mut block = [0i32; 64];
                let qt = self.quant_luma;
                if !self.decode_block(&halfwords, &mut pos, &mut block, &qt) {
                    return;
                }
                self.emit_mono(&block);
            }
        }
    }

    /// Run-length decodes and transforms one 8x8 block. Returns `false`
    /// when the input is exhausted.
    fn decode_block(&self, input: &[u16], pos: &mut usize, block: &mut [i32; 64], qt: &[u8; 64]) -> bool {
        let next = |pos: &mut usize| -> Option<u16> {
            let v = input.get(*pos).copied();
            *pos += 1;
            v
        };
        let mut n = loop {
            match next(pos) {
                Some(0xfe00) => continue,
                Some(v) => break v,
                None => return false,
            }
        };
        *block = [0; 64];
        let q_scale = i32::from((n >> 10) & 0x3f);
        let signed10 = |v: u16| i32::from(((v & 0x3ff) << 6) as i16 >> 6);
        let mut k = 0usize;
        let mut val = signed10(n) * i32::from(qt[0]);
        loop {
            if q_scale == 0 {
                val = signed10(n) * 2;
            }
            val = val.clamp(-0x400, 0x3ff);
            if q_scale > 0 {
                block[ZIGZAG[k]] = val;
            } else {
                block[k] = val;
            }
            n = match next(pos) {
                Some(v) => v,
                None => break,
            };
            k += usize::from((n >> 10) & 0x3f) + 1;
            if k > 63 {
                break;
            }
            val = (signed10(n) * i32::from(qt[k]) * q_scale + 4) / 8;
        }
        self.idct(block);
        true
    }

    fn idct(&self, block: &mut [i32; 64]) {
        let mut tmp = [0i64; 64];
        for x in 0..8 {
            for y in 0..8 {
                let sum: i64 = (0..8)
                    .map(|z| i64::from(block[y + z * 8]) * i64::from(self.scale[x + z * 8]))
                    .sum();
                tmp[x + y * 8] = sum;
            }
        }
        for x in 0..8 {
            for y in 0..8 {
                let sum: i64 = (0..8)
                    .map(|z| ((tmp[y + z * 8] + 0x4000) >> 15) * i64::from(self.scale[x + z * 8]))
                    .sum();
                block[x + y * 8] = ((sum + 0x1_0000) >> 17) as i32;
            }
        }
    }

    fn emit_color(&mut self, blocks: &[[i32; 64]; 6]) {
        let (cr, cb) = (&blocks[0], &blocks[1]);
        let mut pixels = [[0u8; 3]; 256];
        for (yb, yblock) in blocks[2..].iter().enumerate() {
            let (ox, oy) = ((yb & 1) * 8, (yb >> 1) * 8);
            for y in 0..8 {
                for x in 0..8 {
                    let (px, py) = (ox + x, oy + y);
                    let c = (px / 2) + (py / 2) * 8;
                    let (r, b) = (f64::from(cr[c]), f64::from(cb[c]));
                    let l = f64::from(yblock[x + y * 8]);
                    let conv = |v: f64| -> u8 {
                        let v = (v.round() as i32).clamp(-128, 127);
                        if self.signed { v as u8 } else { (v + 128) as u8 }
                    };
                    pixels[px + py * 16] = [
                        conv(l + 1.402 * r),
                        conv(l - 0.3437 * b - 0.7143 * r),
                        conv(l + 1.772 * b),
                    ];
                }
            }
        }
        if self.depth == 2 {
            let bytes: Vec<u8> = pixels.iter().flatten().copied().collect();
            for w in bytes.as_chunks::<4>().0 {
                self.output.push_back(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
            }
        } else {
            let bit15 = u16::from(self.set_bit15) << 15;
            let to15 = |p: &[u8; 3]| -> u16 {
                // Signed output is converted back to unsigned for 15-bit.
                let f = |c: u8| u16::from(if self.signed { c ^ 0x80 } else { c } >> 3);
                f(p[0]) | (f(p[1]) << 5) | (f(p[2]) << 10) | bit15
            };
            for pair in pixels.as_chunks::<2>().0 {
                self.output
                    .push_back(u32::from(to15(&pair[0])) | (u32::from(to15(&pair[1])) << 16));
            }
        }
    }

    fn emit_mono(&mut self, block: &[i32; 64]) {
        let px: Vec<u8> = block
            .iter()
            .map(|&v| {
                let v = v.clamp(-128, 127);
                if self.signed { v as u8 } else { (v + 128) as u8 }
            })
            .collect();
        if self.depth == 1 {
            for w in px.as_chunks::<4>().0 {
                self.output.push_back(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
            }
        } else {
            for w in px.as_chunks::<8>().0 {
                let mut v = 0u32;
                for (i, p) in w.iter().enumerate() {
                    v |= u32::from(p >> 4) << (i * 4);
                }
                self.output.push_back(v);
            }
        }
    }
}
