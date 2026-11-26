//! The Graphics Synthesizer: 4 MiB of local memory, the general registers
//! that the GIF writes (drawing environment, vertices, transfers), a
//! software rasterizer (draw.rs), and the display circuits that read the
//! frame out. The privileged registers (display setup, CSR) are in
//! privileged.rs.

mod draw;
pub mod mem;
mod privileged;
mod record;

pub use privileged::{CSR_VSINT, Privileged, REG_NAMES};
pub use record::{Command, DrawRegs, Rendering};

use mem::{Memory, bits, psm};
use privileged::{CSR_FINISH, CSR_SIGNAL};

/// General register addresses.
pub mod reg {
    pub const PRIM: u8 = 0x00;
    pub const RGBAQ: u8 = 0x01;
    pub const ST: u8 = 0x02;
    pub const UV: u8 = 0x03;
    pub const XYZF2: u8 = 0x04;
    pub const XYZ2: u8 = 0x05;
    pub const TEX0_1: u8 = 0x06;
    pub const TEX0_2: u8 = 0x07;
    pub const CLAMP_1: u8 = 0x08;
    pub const FOG: u8 = 0x0A;
    pub const XYZF3: u8 = 0x0C;
    pub const XYZ3: u8 = 0x0D;
    pub const TEX1_1: u8 = 0x14;
    pub const TEX2_1: u8 = 0x16;
    pub const TEX2_2: u8 = 0x17;
    pub const XYOFFSET_1: u8 = 0x18;
    pub const PRMODECONT: u8 = 0x1A;
    pub const PRMODE: u8 = 0x1B;
    pub const TEXCLUT: u8 = 0x1C;
    pub const TEXA: u8 = 0x3B;
    pub const FOGCOL: u8 = 0x3D;
    pub const SCISSOR_1: u8 = 0x40;
    pub const ALPHA_1: u8 = 0x42;
    pub const DTHE: u8 = 0x45;
    pub const COLCLAMP: u8 = 0x46;
    pub const TEST_1: u8 = 0x47;
    pub const PABE: u8 = 0x49;
    pub const FBA_1: u8 = 0x4A;
    pub const FRAME_1: u8 = 0x4C;
    pub const ZBUF_1: u8 = 0x4E;
    pub const BITBLTBUF: u8 = 0x50;
    pub const TRXPOS: u8 = 0x51;
    pub const TRXREG: u8 = 0x52;
    pub const TRXDIR: u8 = 0x53;
    pub const HWREG: u8 = 0x54;
    pub const SIGNAL: u8 = 0x60;
    pub const FINISH: u8 = 0x61;
    pub const LABEL: u8 = 0x62;
}

/// Bits `lo..lo+n` of `v`.
#[inline]
pub(crate) fn field(v: u64, lo: u32, n: u32) -> u32 {
    ((v >> lo) & ((1u64 << n) - 1)) as u32
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vertex {
    /// Window coordinates, 12.4 fixed point (before XYOFFSET).
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub fog: u8,
    pub rgba: [u8; 4],
    pub s: f32,
    pub t: f32,
    pub q: f32,
    /// Texel coordinates, 12.4 fixed point.
    pub u: u32,
    pub v: u32,
}

/// A host to local memory transfer under way.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Transfer {
    active: bool,
    x: u32,
    y: u32,
    /// Bytes waiting for a whole pixel (24-bit transfers).
    pending: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Gs {
    pub privileged: Privileged,
    #[serde(skip)]
    pub mem: Memory,
    /// The general registers, by address.
    #[serde(with = "serde_big_array::BigArray")]
    pub regs: [u64; 0x63],
    vertices: Vec<Vertex>,
    /// Q from the last ST (packed mode) or RGBAQ.
    q: f32,
    transfer: Transfer,
    /// The CLUT buffer: 256 entries, as stored (32 or 16 bits).
    /// Kept beside the rest in save states (see `Ps2::save_state`).
    #[serde(skip, default = "no_clut")]
    clut: [u32; 256],
    cbp0: u32,
    cbp1: u32,
    /// An event the IMR does not mask happened: the GS interrupt.
    pub irq: bool,
    /// Primitives drawn, for diagnostics.
    pub drawn: u64,
    /// Who renders; what was recorded for another renderer.
    #[serde(skip)]
    pub rendering: Rendering,
    #[serde(skip)]
    commands: Vec<Command>,
    /// Diagnostics, when set: the transfers started (direction, then the
    /// destination's base, width, format, position and size, the source's
    /// base, width and format, the 64-bit words of data the transfer
    /// received and how many were not zero, and the EE address its first
    /// data came from, by GIF DMA), the first million.
    #[serde(skip)]
    pub transfer_log: Option<Vec<[u32; 14]>>,
    /// Diagnostics: the EE address of the GIF DMA's current quadword.
    #[serde(skip)]
    pub dma_source: u32,
}

fn no_clut() -> [u32; 256] {
    [0; 256]
}

impl Default for Gs {
    fn default() -> Self {
        Self {
            privileged: Privileged::default(),
            mem: Memory::default(),
            regs: [0; 0x63],
            vertices: Vec::with_capacity(4),
            q: 1.0,
            transfer: Transfer::default(),
            clut: [0; 256],
            cbp0: u32::MAX,
            cbp1: u32::MAX,
            irq: false,
            drawn: 0,
            rendering: Rendering::Software,
            commands: Vec::new(),
            transfer_log: None,
            dma_source: 0,
        }
    }
}

impl Gs {
    /// The CLUT draws use now (for save states).
    pub fn clut(&self) -> &[u32; 256] {
        &self.clut
    }

    /// Sets the CLUT draws use (restoring a save state).
    pub fn set_clut(&mut self, clut: &[u32]) {
        let n = clut.len().min(256);
        self.clut[..n].copy_from_slice(&clut[..n]);
    }

    // --- Privileged registers (0x12000000) -----------------------------------

    pub fn vblank(&mut self) {
        self.privileged.vblank();
        // VSINT, unless masked by IMR bit 11.
        if self.privileged.imr & (1 << 11) == 0 {
            self.irq = true;
        }
    }

    pub fn read64(&self, offset: u32) -> u64 {
        self.privileged.read64(offset)
    }
    pub fn write64(&mut self, offset: u32, value: u64) {
        self.privileged.write64(offset, value);
    }
    pub fn read32(&self, offset: u32) -> u32 {
        self.privileged.read32(offset)
    }
    pub fn write32(&mut self, offset: u32, value: u32) {
        self.privileged.write32(offset, value);
    }

    // --- General registers (through the GIF) --------------------------------

    pub fn write_reg(&mut self, addr: u8, data: u64) {
        let a = addr as usize;
        match addr {
            reg::PRIM => {
                self.regs[a] = data;
                self.vertices.clear();
            }
            reg::RGBAQ => {
                self.regs[a] = data;
                self.q = f32::from_bits((data >> 32) as u32);
            }
            reg::XYZ2 | reg::XYZF2 | reg::XYZ3 | reg::XYZF3 => {
                self.regs[a] = data;
                let with_fog = addr == reg::XYZF2 || addr == reg::XYZF3;
                let kick = addr == reg::XYZ2 || addr == reg::XYZF2;
                self.vertex(data, with_fog, kick);
            }
            reg::TEX0_1 | reg::TEX0_2 => {
                self.regs[a] = data;
                self.load_clut(data);
            }
            reg::TEX2_1 | reg::TEX2_2 => {
                // TEX2 changes TEX0's format and CLUT fields only.
                let tex0 = (a - reg::TEX2_1 as usize) + reg::TEX0_1 as usize;
                let keep = !((0x3Fu64 << 20) | (0x7FF_FFFFu64 << 37));
                self.regs[tex0] = (self.regs[tex0] & keep) | (data & !keep);
                self.load_clut(self.regs[tex0]);
            }
            reg::TRXDIR => {
                self.regs[a] = data;
                self.start_transfer(field(data, 0, 2));
            }
            reg::HWREG => self.hwreg(data),
            reg::SIGNAL => {
                // ID and mask into SIGLBLID's low half.
                let p = &mut self.privileged;
                let mask = u64::from(field(data, 32, 32));
                p.siglblid = (p.siglblid & !mask) | (u64::from(field(data, 0, 32)) & mask);
                p.csr |= CSR_SIGNAL;
                if p.imr & (1 << 8) == 0 {
                    self.irq = true;
                }
            }
            reg::FINISH => {
                self.privileged.csr |= CSR_FINISH;
                if self.privileged.imr & (1 << 9) == 0 {
                    self.irq = true;
                }
            }
            reg::LABEL => {
                let p = &mut self.privileged;
                let mask = u64::from(field(data, 32, 32));
                p.siglblid = (p.siglblid & !(mask << 32)) | ((u64::from(field(data, 0, 32)) & mask) << 32);
            }
            _ => {
                if a < self.regs.len() {
                    self.regs[a] = data;
                }
            }
        }
    }

    /// A vertex: queued, and the primitive drawn when complete (`kick`).
    fn vertex(&mut self, data: u64, with_fog: bool, kick: bool) {
        let rgbaq = self.regs[reg::RGBAQ as usize];
        let st = self.regs[reg::ST as usize];
        let uv = self.regs[reg::UV as usize];
        let v = Vertex {
            x: field(data, 0, 16),
            y: field(data, 16, 16),
            z: if with_fog {
                field(data, 32, 24)
            } else {
                field(data, 32, 32)
            },
            fog: if with_fog {
                field(data, 56, 8) as u8
            } else {
                field(self.regs[reg::FOG as usize], 56, 8) as u8
            },
            rgba: [
                field(rgbaq, 0, 8) as u8,
                field(rgbaq, 8, 8) as u8,
                field(rgbaq, 16, 8) as u8,
                field(rgbaq, 24, 8) as u8,
            ],
            s: f32::from_bits(field(st, 0, 32)),
            t: f32::from_bits(field(st, 32, 32)),
            q: self.q,
            u: field(uv, 0, 14),
            v: field(uv, 16, 14),
        };
        self.vertices.push(v);
        let prim = self.regs[reg::PRIM as usize];
        let kind = field(prim, 0, 3);
        let needed = match kind {
            0 => 1,
            1 | 2 | 6 => 2,
            3..=5 => 3,
            _ => {
                self.vertices.clear();
                return;
            }
        };
        if self.vertices.len() < needed {
            return;
        }
        if kick {
            let verts: Vec<Vertex> = self.vertices[self.vertices.len() - needed..].to_vec();
            if self.rendering.software() {
                self.draw(kind, &verts);
            }
            if self.rendering.records() {
                let regs = self.draw_regs();
                self.commands.push(Command::Draw {
                    kind,
                    regs,
                    vertices: verts,
                });
            }
            self.drawn += 1;
        }
        // What stays queued for the next primitive.
        match kind {
            2 => {
                let last = self.vertices[self.vertices.len() - 1];
                self.vertices.clear();
                self.vertices.push(last);
            }
            4 => {
                self.vertices.remove(0);
            }
            5 => {
                self.vertices.remove(1);
            }
            _ => self.vertices.clear(),
        }
    }

    // --- The CLUT buffer -------------------------------------------------------

    /// Loads the CLUT buffer as TEX0's CLD field asks.
    fn load_clut(&mut self, tex0: u64) {
        let cbp = field(tex0, 37, 14);
        let cld = field(tex0, 61, 3);
        let load = match cld {
            1 => true,
            2 => {
                self.cbp0 = cbp;
                true
            }
            3 => {
                self.cbp1 = cbp;
                true
            }
            4 => {
                let changed = self.cbp0 != cbp;
                self.cbp0 = cbp;
                changed
            }
            5 => {
                let changed = self.cbp1 != cbp;
                self.cbp1 = cbp;
                changed
            }
            _ => false,
        };
        if !load {
            return;
        }
        if self.rendering.records() {
            self.commands.push(Command::Clut { tex0 });
        }
        if !self.rendering.software() {
            return;
        }
        let format = field(tex0, 20, 6);
        let cpsm = field(tex0, 51, 4);
        let csa = field(tex0, 56, 5) as usize;
        let read = |m: &Memory, x: u32, y: u32| {
            if cpsm == psm::CT32 || cpsm == psm::CT24 {
                m.read(psm::CT32, cbp, 1, x, y)
            } else {
                m.read(cpsm, cbp, 1, x, y)
            }
        };
        if bits(format) == 8 {
            // 16×16 entries; bits 3 and 4 of the index are swapped.
            for i in 0..256u32 {
                let j = (i & 0xE7) | ((i & 0x08) << 1) | ((i & 0x10) >> 1);
                self.clut[i as usize] = read(&self.mem, j & 15, j >> 4);
            }
        } else {
            // 8×2 entries, at the CSA offset.
            for i in 0..16u32 {
                self.clut[(csa * 16 + i as usize) & 255] = read(&self.mem, i & 7, i >> 3);
            }
        }
    }

    // --- Transfers -------------------------------------------------------------

    fn start_transfer(&mut self, dir: u32) {
        if self.transfer_log.as_ref().is_some_and(|l| l.len() < 1_000_000) {
            let (dbp, dbw, dpsm, dx, dy, w, h) = self.transfer_target();
            let b = self.regs[reg::BITBLTBUF as usize];
            let entry = [
                dir,
                dbp,
                dbw,
                dpsm,
                dx,
                dy,
                w,
                h,
                field(b, 0, 14),
                field(b, 16, 6),
                field(b, 24, 6),
                0,
                0,
                0,
            ];
            if let Some(l) = self.transfer_log.as_mut() {
                l.push(entry);
            }
        }
        self.transfer = Transfer::default();
        match dir {
            0 => self.transfer.active = true,
            2 => self.local_to_local(),
            _ => {}
        }
    }

    /// The destination of host to local transfers: base, width, format,
    /// position and size.
    fn transfer_target(&self) -> (u32, u32, u32, u32, u32, u32, u32) {
        let b = self.regs[reg::BITBLTBUF as usize];
        let p = self.regs[reg::TRXPOS as usize];
        let r = self.regs[reg::TRXREG as usize];
        (
            field(b, 32, 14),
            field(b, 48, 6),
            field(b, 56, 6),
            field(p, 32, 11),
            field(p, 48, 11),
            field(r, 0, 12),
            field(r, 32, 12),
        )
    }

    /// 64 bits of a host to local transfer.
    fn hwreg(&mut self, data: u64) {
        if !self.transfer.active {
            return;
        }
        let source = self.dma_source;
        if let Some(last) = self.transfer_log.as_mut().and_then(|l| l.last_mut()) {
            if last[11] == 0 {
                last[13] = source;
            }
            last[11] += 1;
            last[12] += u32::from(data != 0);
        }
        let (dbp, dbw, dpsm, dx, dy, w, h) = self.transfer_target();
        if w == 0 || h == 0 {
            self.transfer.active = false;
            return;
        }
        let bpp = bits(dpsm);
        let put = |gs: &mut Gs, value: u32| {
            let t = &mut gs.transfer;
            if !t.active {
                return;
            }
            let index = t.y * w + t.x;
            let (x, y) = ((dx + t.x) % 2048, (dy + t.y) % 2048);
            t.x += 1;
            if t.x >= w {
                t.x = 0;
                t.y += 1;
                if t.y >= h {
                    t.active = false;
                }
            }
            if gs.rendering.records() {
                gs.record_pixel((dbp, dbw, dpsm, dx, dy, w), index, value);
            }
            if gs.rendering.software() {
                gs.mem.write(dpsm, dbp, dbw, x, y, value);
            }
        };
        match bpp {
            32 if dpsm == psm::CT24 || dpsm == psm::Z24 => {
                // Packed: 3 bytes per pixel.
                let mut bytes = std::mem::take(&mut self.transfer.pending);
                bytes.extend_from_slice(&data.to_le_bytes());
                let whole = bytes.len() / 3 * 3;
                for c in bytes[..whole].as_chunks::<3>().0 {
                    put(self, u32::from(c[0]) | u32::from(c[1]) << 8 | u32::from(c[2]) << 16);
                }
                self.transfer.pending = bytes[whole..].to_vec();
            }
            32 => {
                put(self, data as u32);
                put(self, (data >> 32) as u32);
            }
            16 => (0..4).for_each(|k| put(self, ((data >> (16 * k)) & 0xFFFF) as u32)),
            8 => (0..8).for_each(|k| put(self, ((data >> (8 * k)) & 0xFF) as u32)),
            _ => (0..16).for_each(|k| put(self, ((data >> (4 * k)) & 0xF) as u32)),
        }
    }

    fn local_to_local(&mut self) {
        let b = self.regs[reg::BITBLTBUF as usize];
        let p = self.regs[reg::TRXPOS as usize];
        let (sbp, sbw, spsm) = (field(b, 0, 14), field(b, 16, 6), field(b, 24, 6));
        let (sx, sy) = (field(p, 0, 11), field(p, 16, 11));
        let (dbp, dbw, dpsm, dx, dy, w, h) = self.transfer_target();
        if self.rendering.records() {
            self.commands.push(Command::Copy {
                sbp,
                sbw,
                spsm,
                sx,
                sy,
                dbp,
                dbw,
                dpsm,
                dx,
                dy,
                w,
                h,
            });
        }
        if !self.rendering.software() {
            return;
        }
        for y in 0..h {
            for x in 0..w {
                let v = self.mem.read(spsm, sbp, sbw, (sx + x) % 2048, (sy + y) % 2048);
                self.mem.write(dpsm, dbp, dbw, (dx + x) % 2048, (dy + y) % 2048, v);
            }
        }
    }

    // --- Display ----------------------------------------------------------------

    /// The displayed picture, as RGBA8888: its size, or (0, 0) when the
    /// display is off. One read circuit (the second, when both are on).
    pub fn display(&self, out: &mut Vec<u8>) -> (usize, usize) {
        let p = &self.privileged;
        let pmode = p.regs[0];
        let circuit = if pmode & 2 != 0 {
            1
        } else if pmode & 1 != 0 {
            0
        } else {
            return (0, 0);
        };
        let dispfb = p.regs[7 + 2 * circuit];
        let display = p.regs[8 + 2 * circuit];
        let fbp = field(dispfb, 0, 9) * 32;
        let fbw = field(dispfb, 9, 6);
        let format = field(dispfb, 15, 5);
        let (dbx, dby) = (field(dispfb, 32, 11), field(dispfb, 43, 11));
        let magh = field(display, 23, 4) + 1;
        let magv = field(display, 27, 2) + 1;
        let w = ((field(display, 32, 12) + 1) / magh).clamp(1, 1024) as usize;
        let mut h = ((field(display, 44, 11) + 1) / magv).clamp(1, 1024) as usize;
        // Interlaced, field mode: one line of the buffer per two.
        let smode2 = p.regs[2];
        if smode2 & 1 != 0 && smode2 & 2 != 0 {
            h /= 2;
        }
        out.clear();
        out.reserve(w * h * 4);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let c = self.mem.read(format, fbp, fbw, dbx + x, dby + y);
                let rgba = match format {
                    psm::CT16 | psm::CT16S => expand16(c),
                    _ => c,
                };
                out.extend_from_slice(&[rgba as u8, (rgba >> 8) as u8, (rgba >> 16) as u8, 255]);
            }
        }
        (w, h)
    }
}

/// A 16-bit 5551 color as 8888 (alpha 0 or 0x80).
pub(crate) fn expand16(c: u32) -> u32 {
    let r = (c & 0x1F) << 3;
    let g = ((c >> 5) & 0x1F) << 3;
    let b = ((c >> 10) & 0x1F) << 3;
    let a = if c & 0x8000 != 0 { 0x80 } else { 0 };
    r | (g << 8) | (b << 16) | (a << 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_to_local_transfer() {
        let mut gs = Gs::default();
        // 4×1 pixels of CT32 at (2, 3) of a buffer at block 0, 64 wide.
        gs.write_reg(reg::BITBLTBUF, (1 << 48) | (u64::from(psm::CT32) << 56));
        gs.write_reg(reg::TRXPOS, (2 << 32) | (3 << 48));
        gs.write_reg(reg::TRXREG, 4 | (1 << 32));
        gs.write_reg(reg::TRXDIR, 0);
        gs.write_reg(reg::HWREG, 0x2222_2222_1111_1111);
        gs.write_reg(reg::HWREG, 0x4444_4444_3333_3333);
        assert_eq!(gs.mem.read(psm::CT32, 0, 1, 2, 3), 0x1111_1111);
        assert_eq!(gs.mem.read(psm::CT32, 0, 1, 5, 3), 0x4444_4444);
        assert!(!gs.transfer.active);
    }

    /// Two quads at the same Z overlapping, the depth test GEQUAL: the
    /// second draws over the first everywhere, its Z (flat) staying exactly
    /// the vertices' (Liberty City Stories' glyphs lost their overlap).
    #[test]
    fn flat_z_passes_gequal_over_a_neighbour() {
        let mut gs = Gs::default();
        gs.write_reg(reg::FRAME_1, 1 << 16);
        gs.write_reg(reg::SCISSOR_1, (63 << 16) | (63 << 48));
        // Z24 at block 64; ZTE, GEQUAL.
        gs.write_reg(reg::ZBUF_1, 2 | (1 << 24));
        gs.write_reg(reg::TEST_1, (1 << 16) | (2 << 17));
        gs.write_reg(reg::PRMODECONT, 1);
        gs.write_reg(reg::PRIM, 3);
        let xyz = |x: f64, y: f64| (x * 16.0) as u64 | ((y * 16.0) as u64) << 16 | 0xFF_FFFE << 32;
        for (x0, color) in [(4.5, 0x8000_00FFu64), (16.5, 0x8000_FF00)] {
            let (x1, y0, y1) = (x0 + 15.0, 2.5, 28.5);
            gs.write_reg(reg::RGBAQ, color);
            for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0), (x1, y1)] {
                gs.write_reg(reg::XYZ2, xyz(x, y));
            }
        }
        for y in 3..28 {
            for x in 17..20 {
                assert_eq!(gs.mem.read(psm::CT32, 0, 1, x, y) & 0xFF_FFFF, 0xFF00, "pixel {x},{y}");
            }
        }
    }

    /// TEX1's MMAG = linear: a 2×1 texture (black, white) stretched over 4
    /// pixels blends the two between their centers; nearest, it does not.
    #[test]
    fn bilinear_filtering_when_magnifying() {
        for (tex1, third) in [(1 | 1 << 5, 0x7F_7F7F), (1, 0xFF_FFFF)] {
            let mut gs = Gs::default();
            gs.write_reg(reg::FRAME_1, 1 << 16);
            gs.write_reg(reg::SCISSOR_1, (63 << 16) | (63 << 48));
            gs.write_reg(reg::TEST_1, 0);
            gs.write_reg(reg::PRMODECONT, 1);
            // The texture at block 32, 64 wide: black then white.
            gs.write_reg(reg::BITBLTBUF, (32 << 32) | (1 << 48));
            gs.write_reg(reg::TRXPOS, 0);
            gs.write_reg(reg::TRXREG, 2 | (1 << 32));
            gs.write_reg(reg::TRXDIR, 0);
            gs.write_reg(reg::HWREG, 0x80FF_FFFF_8000_0000);
            // 2×1 (TW 1, TH 0), CT32, TCC, decal; TEX1: LCM (LOD = K = 0).
            gs.write_reg(reg::TEX0_1, 32 | (1 << 14) | (1 << 26) | (1 << 34) | (1 << 35));
            gs.write_reg(reg::TEX1_1, tex1);
            gs.write_reg(reg::PRIM, 6 | (1 << 4) | (1 << 8));
            gs.write_reg(reg::UV, 0);
            gs.write_reg(reg::XYZ2, 0);
            gs.write_reg(reg::UV, (2 * 16) | (16 << 16));
            gs.write_reg(reg::XYZ2, (4 * 16) | (16 << 16));
            let row: Vec<u32> = (0..4).map(|x| gs.mem.read(psm::CT32, 0, 1, x, 0) & 0xFF_FFFF).collect();
            assert_eq!(row[1], 0, "TEX1 {tex1:#x}: {row:x?}");
            assert_eq!(row[2], third, "TEX1 {tex1:#x}: {row:x?}");
        }
    }
}
