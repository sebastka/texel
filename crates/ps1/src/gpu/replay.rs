//! Scaled drawing in software, from what the GPU records (`Gpu::set_recording`):
//! the same drawing as the GPU's own scaled pass, elsewhere (in a browser,
//! another worker: another core). Its VRAMs: the scaled one it draws, and
//! the native one textures are read from, as the recorder sends it.
//!
//! Precise triangles (`Command::Precise`) are drawn as the GPU renderer
//! draws them (`Raster::exact_triangle`).

use super::{
    Command, Display, Picture, Prim, Raster, Spans, Transfer, VRAM_HEIGHT, VRAM_WIDTH, Vertex, display_15, display_rgba,
};

pub struct Replay {
    scale: usize,
    hires: Box<[u16]>,
    native: Box<[u16]>,
}

impl Default for Replay {
    fn default() -> Self {
        Self::new()
    }
}

impl Replay {
    pub fn new() -> Self {
        Self {
            scale: 1,
            hires: vec![0; VRAM_WIDTH * VRAM_HEIGHT].into_boxed_slice(),
            native: vec![0; VRAM_WIDTH * VRAM_HEIGHT].into_boxed_slice(),
        }
    }

    pub fn scale(&self) -> usize {
        self.scale
    }

    /// Runs recorded commands, in order.
    pub fn apply(&mut self, commands: &[Command]) {
        for c in commands {
            match c {
                Command::Upload { x, y, w, h, data } => {
                    let (x, w) = (*x as usize, *w as usize);
                    for row in 0..*h as usize {
                        let at = (*y as usize + row) * VRAM_WIDTH + x;
                        self.native[at..at + w].copy_from_slice(&data[row * w..row * w + w]);
                    }
                }
                Command::Rescale { scale } => {
                    let s = (*scale as usize).clamp(1, super::MAX_SCALE);
                    self.scale = s;
                    self.hires = vec![0; VRAM_WIDTH * VRAM_HEIGHT * s * s].into_boxed_slice();
                    for y in 0..VRAM_HEIGHT {
                        for x in 0..VRAM_WIDTH {
                            let v = self.native[y * VRAM_WIDTH + x];
                            self.block(x, y, v, false);
                        }
                    }
                }
                Command::Triangle { pixel, spans } => {
                    let mut r = self.raster(pixel.check_mask, pixel.set_mask, [0, 0, 0, 0]);
                    triangle(&mut r, *spans, &pixel.prim, pixel.textured, pixel.shaded);
                }
                Command::Rect {
                    pixel,
                    x0,
                    y0,
                    xs,
                    xe,
                    ys,
                    ye,
                    u,
                    v,
                    color,
                    flip,
                } => {
                    let vertex = Vertex {
                        u: *u,
                        v: *v,
                        r: color.0,
                        g: color.1,
                        b: color.2,
                        ..Vertex::default()
                    };
                    let mut r = self.raster(pixel.check_mask, pixel.set_mask, [0, 0, 0, 0]);
                    let bbox = (*xs, *xe, *ys, *ye);
                    if pixel.textured {
                        r.rect_rows::<true>(vertex, (*x0, *y0), bbox, *flip, &pixel.prim);
                    } else {
                        r.rect_rows::<false>(vertex, (*x0, *y0), bbox, *flip, &pixel.prim);
                    }
                }
                Command::Line { pixel, a, b, area } => {
                    let mut r = self.raster(pixel.check_mask, pixel.set_mask, *area);
                    r.line_scaled(*a, *b, pixel.shaded, &pixel.prim);
                }
                Command::Precise { pixel, vertices, bbox } => {
                    let mut r = self.raster(pixel.check_mask, pixel.set_mask, *bbox);
                    r.exact_triangle(vertices, *bbox, pixel.textured, pixel.shaded, &pixel.prim);
                }
                Command::Fill { x, y, w, h, color } => {
                    for row in 0..*h {
                        let ny = ((y + row) & 0x1ff) as usize;
                        for col in 0..*w {
                            self.block(((x + col) & 0x3ff) as usize, ny, *color, false);
                        }
                    }
                }
                Command::Copy {
                    sx,
                    sy,
                    dx,
                    dy,
                    w,
                    h,
                    check_mask,
                    set_mask,
                    skipped,
                } => self.copy((*sx, *sy, *dx, *dy, *w, *h), *check_mask, *set_mask, skipped),
                Command::Write {
                    x,
                    y,
                    w,
                    first,
                    check_mask,
                    pixels,
                    ..
                } => {
                    for (k, &p) in pixels.iter().enumerate() {
                        if p & 0x1_0000 != 0 {
                            continue;
                        }
                        let i = first + k as u32;
                        let nx = ((x + i % w) & 0x3ff) as usize;
                        let ny = ((y + i / w) & 0x1ff) as usize;
                        self.block(nx, ny, p as u16, *check_mask);
                    }
                }
            }
        }
    }

    /// The displayed picture, RGBA8888, into `out`: its size.
    pub fn render_display(&self, d: Display, out: &mut Vec<u8>) -> (usize, usize) {
        display_rgba(d, self.scale, &self.hires, &self.native, out)
    }

    /// The displayed picture as the scaled VRAM holds it (see
    /// `display_15`; `None`: `render_display`).
    pub fn picture<'a>(&'a self, d: Display, wrapped: &'a mut Vec<u16>) -> Option<Picture<'a>> {
        display_15(d, self.scale, &self.hires, wrapped)
    }

    fn raster(&mut self, check_mask: bool, set_mask: bool, area: [i32; 4]) -> Raster<'_> {
        let s = self.scale;
        Raster {
            target: &mut self.hires,
            tex: Some(&self.native),
            scale: s as i32,
            stride: VRAM_WIDTH * s,
            area: (area[0], area[1], area[2], area[3]),
            check_mask,
            set_mask,
            tex_window: 0,
            pixels: 0,
            bounds: None,
            rec: None,
        }
    }

    /// A native pixel's block (`masked`: keeping pixels with the mask bit).
    fn block(&mut self, x: usize, y: usize, v: u16, masked: bool) {
        let s = self.scale;
        let stride = VRAM_WIDTH * s;
        for dy in 0..s {
            let row = (y * s + dy) * stride + x * s;
            for p in &mut self.hires[row..row + s] {
                if masked && *p & 0x8000 != 0 {
                    continue;
                }
                *p = v;
            }
        }
    }

    /// A VRAM copy, in the software GPU's order, but the pixels the native
    /// mask kept.
    fn copy(
        &mut self,
        (sx, sy, dx, dy, w, h): (u32, u32, u32, u32, u32, u32),
        check_mask: bool,
        set_mask: bool,
        skipped: &[u32],
    ) {
        let s = self.scale;
        let stride = VRAM_WIDTH * s;
        let mut src = Transfer {
            x: sx,
            y: sy,
            w,
            h,
            i: 0,
        };
        let mut dst = Transfer {
            x: dx,
            y: dy,
            w,
            h,
            i: 0,
        };
        let mut skip = skipped.iter().peekable();
        while let (Some((fx, fy)), Some((tx, ty))) = (src.next(), dst.next()) {
            if skip.next_if(|&&k| k == dst.i - 1).is_some() {
                continue;
            }
            for by in 0..s {
                for bx in 0..s {
                    let from = (fy * s + by) * stride + fx * s + bx;
                    let to = (ty * s + by) * stride + tx * s + bx;
                    let v = self.hires[from];
                    if check_mask && self.hires[to] & 0x8000 != 0 {
                        continue;
                    }
                    self.hires[to] = v | (u16::from(set_mask) << 15);
                }
            }
        }
    }
}

fn triangle(r: &mut Raster, spans: Spans, prim: &Prim, textured: bool, shaded: bool) {
    match (textured, shaded) {
        (true, true) => r.triangle_spans::<true, true>(spans, prim),
        (true, false) => r.triangle_spans::<true, false>(spans, prim),
        (false, true) => r.triangle_spans::<false, true>(spans, prim),
        (false, false) => r.triangle_spans::<false, false>(spans, prim),
    }
}
