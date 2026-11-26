//! The GS's rasterizer, in software: points, lines, triangles and sprites,
//! with Gouraud shading, texture mapping (STQ or UV, nearest sampling,
//! palettes, the four wrap modes), fog, the alpha, depth and destination
//! alpha tests, alpha blending and the frame buffer write masks.
//!
//! Pixels are sampled at their top-left corner, with a top-left fill rule.

use super::mem::{bits, psm};
use super::{Gs, Vertex, expand16, field, reg};

/// What a texture lookup needs.
struct Texture {
    tbp: u32,
    tbw: u32,
    psm: u32,
    w: i32,
    h: i32,
    tcc: bool,
    tfx: u32,
    cpsm: u32,
    csa: u32,
    wms: u32,
    wmt: u32,
    minu: i32,
    maxu: i32,
    minv: i32,
    maxv: i32,
    ta0: u32,
    ta1: u32,
    aem: bool,
    /// TEX1: the level of detail from K alone (LCM), or from Q, L and K;
    /// bilinear filtering when magnifying (MMAG) and when minifying (MMIN
    /// 1, 4 or 5: the base level only, mipmaps not drawn).
    lcm: bool,
    l: u32,
    k: f64,
    mag_linear: bool,
    min_linear: bool,
}

/// The drawing environment of a primitive.
struct Env {
    iip: bool,
    fge: bool,
    abe: bool,
    fst: bool,
    tex: Option<Texture>,
    ofx: i32,
    ofy: i32,
    sx0: i32,
    sx1: i32,
    sy0: i32,
    sy1: i32,
    fbp: u32,
    fbw: u32,
    fpsm: u32,
    fbmsk: u32,
    zbp: u32,
    zpsm: u32,
    zmsk: bool,
    ate: bool,
    atst: u32,
    aref: u32,
    afail: u32,
    date: bool,
    datm: bool,
    zte: bool,
    ztst: u32,
    alpha: [u32; 4],
    fix: u32,
    fba: bool,
    clamp: bool,
    fogcol: [u32; 3],
}

impl Gs {
    fn env(&self) -> Env {
        let r = &self.regs;
        let prim = r[reg::PRIM as usize];
        let attr = if r[reg::PRMODECONT as usize] & 1 != 0 {
            prim
        } else {
            r[reg::PRMODE as usize]
        };
        let c = field(attr, 9, 1) as usize;
        let tme = field(attr, 4, 1) != 0;
        let frame = r[reg::FRAME_1 as usize + c];
        let zbuf = r[reg::ZBUF_1 as usize + c];
        let test = r[reg::TEST_1 as usize + c];
        let alpha = r[reg::ALPHA_1 as usize + c];
        let scissor = r[reg::SCISSOR_1 as usize + c];
        let offset = r[reg::XYOFFSET_1 as usize + c];
        let texa = r[reg::TEXA as usize];
        let fogcol = r[reg::FOGCOL as usize];
        let tex = tme.then(|| {
            let t = r[reg::TEX0_1 as usize + c];
            let t1 = r[reg::TEX1_1 as usize + c];
            let clamp = r[reg::CLAMP_1 as usize + c];
            // K: signed, 4 fractional bits.
            let k = f64::from(((field(t1, 32, 12) << 20) as i32) >> 20) / 16.0;
            Texture {
                tbp: field(t, 0, 14),
                tbw: field(t, 14, 6),
                psm: field(t, 20, 6),
                w: 1 << field(t, 26, 4).min(10),
                h: 1 << field(t, 30, 4).min(10),
                tcc: field(t, 34, 1) != 0,
                tfx: field(t, 35, 2),
                cpsm: field(t, 51, 4),
                csa: field(t, 56, 5),
                wms: field(clamp, 0, 2),
                wmt: field(clamp, 2, 2),
                minu: field(clamp, 4, 10) as i32,
                maxu: field(clamp, 14, 10) as i32,
                minv: field(clamp, 24, 10) as i32,
                maxv: field(clamp, 34, 10) as i32,
                ta0: field(texa, 0, 8),
                ta1: field(texa, 32, 8),
                aem: field(texa, 15, 1) != 0,
                lcm: field(t1, 0, 1) != 0,
                l: field(t1, 19, 2),
                k,
                mag_linear: field(t1, 5, 1) != 0,
                min_linear: matches!(field(t1, 6, 3), 1 | 4 | 5),
            }
        });
        Env {
            iip: field(attr, 3, 1) != 0,
            fge: field(attr, 5, 1) != 0,
            abe: field(attr, 6, 1) != 0,
            fst: field(attr, 8, 1) != 0,
            tex,
            ofx: field(offset, 0, 16) as i32,
            ofy: field(offset, 32, 16) as i32,
            sx0: field(scissor, 0, 11) as i32,
            sx1: field(scissor, 16, 11) as i32,
            sy0: field(scissor, 32, 11) as i32,
            sy1: field(scissor, 48, 11) as i32,
            fbp: field(frame, 0, 9) * 32,
            fbw: field(frame, 16, 6),
            fpsm: field(frame, 24, 6),
            fbmsk: field(frame, 32, 32),
            zbp: field(zbuf, 0, 9) * 32,
            zpsm: field(zbuf, 24, 4) | 0x30,
            zmsk: field(zbuf, 32, 1) != 0,
            ate: field(test, 0, 1) != 0,
            atst: field(test, 1, 3),
            aref: field(test, 4, 8),
            afail: field(test, 12, 2),
            date: field(test, 14, 1) != 0,
            datm: field(test, 15, 1) != 0,
            zte: field(test, 16, 1) != 0,
            ztst: field(test, 17, 2),
            alpha: [
                field(alpha, 0, 2),
                field(alpha, 2, 2),
                field(alpha, 4, 2),
                field(alpha, 6, 2),
            ],
            fix: field(alpha, 32, 8),
            fba: field(r[reg::FBA_1 as usize + c], 0, 1) != 0,
            clamp: field(r[reg::COLCLAMP as usize], 0, 1) != 0,
            fogcol: [field(fogcol, 0, 8), field(fogcol, 8, 8), field(fogcol, 16, 8)],
        }
    }

    pub(super) fn draw(&mut self, kind: u32, v: &[Vertex]) {
        let env = self.env();
        match kind {
            0 => self.draw_point(&env, &v[0]),
            1 | 2 => self.draw_line(&env, &v[0], &v[1]),
            3..=5 => self.draw_triangle(&env, v),
            _ => self.draw_sprite(&env, &v[0], &v[1]),
        }
    }

    // --- Primitives ---------------------------------------------------------------

    fn draw_point(&mut self, e: &Env, v: &Vertex) {
        let x = (v.x as i32 - e.ofx + 8) >> 4;
        let y = (v.y as i32 - e.ofy + 8) >> 4;
        let tc = self.texcoord(
            e,
            f64::from(v.s),
            f64::from(v.t),
            f64::from(v.q),
            f64::from(v.u),
            f64::from(v.v),
        );
        self.fragment(e, x, y, v.z, v.rgba.map(u32::from), tc, u32::from(v.fog));
    }

    fn draw_line(&mut self, e: &Env, a: &Vertex, b: &Vertex) {
        let (x0, y0) = (
            f64::from(a.x as i32 - e.ofx) / 16.0,
            f64::from(a.y as i32 - e.ofy) / 16.0,
        );
        let (x1, y1) = (
            f64::from(b.x as i32 - e.ofx) / 16.0,
            f64::from(b.y as i32 - e.ofy) / 16.0,
        );
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).round() as i32;
        for i in 0..steps.max(1) {
            let k = if steps == 0 {
                0.0
            } else {
                f64::from(i) / f64::from(steps)
            };
            let lerp = |p: f64, q: f64| p + (q - p) * k;
            // Positions exactly, in 1/16 pixels: floats put some on the
            // wrong side of a pixel boundary.
            let (x, y) = line_pixel(
                (a.x as i32 - e.ofx, a.y as i32 - e.ofy),
                (b.x as i32 - e.ofx, b.y as i32 - e.ofy),
                i,
                steps,
            );
            let z = lerp(f64::from(a.z), f64::from(b.z)) as u32;
            let color = if e.iip {
                std::array::from_fn(|c| lerp(f64::from(a.rgba[c]), f64::from(b.rgba[c])) as u32)
            } else {
                b.rgba.map(u32::from)
            };
            let tc = self.texcoord(
                e,
                lerp(f64::from(a.s), f64::from(b.s)),
                lerp(f64::from(a.t), f64::from(b.t)),
                lerp(f64::from(a.q), f64::from(b.q)),
                lerp(f64::from(a.u), f64::from(b.u)),
                lerp(f64::from(a.v), f64::from(b.v)),
            );
            let fog = lerp(f64::from(a.fog), f64::from(b.fog)) as u32;
            self.fragment(e, x, y, z, color, tc, fog);
        }
    }

    fn draw_sprite(&mut self, e: &Env, a: &Vertex, b: &Vertex) {
        let (ax, ay) = (a.x as i32 - e.ofx, a.y as i32 - e.ofy);
        let (bx, by) = (b.x as i32 - e.ofx, b.y as i32 - e.ofy);
        let (x0, x1) = (ax.min(bx), ax.max(bx));
        let (y0, y1) = (ay.min(by), ay.max(by));
        // Pixels whose corner is in [x0, x1) × [y0, y1), within the scissor.
        let px0 = ((x0 + 15) >> 4).max(e.sx0).max(0);
        let px1 = ((x1 + 15) >> 4).min(e.sx1 + 1);
        let py0 = ((y0 + 15) >> 4).max(e.sy0).max(0);
        let py1 = ((y1 + 15) >> 4).min(e.sy1 + 1);
        if px0 >= px1 || py0 >= py1 {
            return;
        }
        let color = b.rgba.map(u32::from);
        // Texel coordinates along each axis, from a to b: linear in x and y.
        let (mut u0, mut du, mut v0, mut dv) = (0.0, 0.0, 0.0, 0.0);
        let q = if e.fst || b.q == 0.0 { 1.0 } else { f64::from(b.q) };
        if let Some(tex) = &e.tex {
            let (ua, ub, va, vb) = if e.fst {
                (
                    f64::from(a.u) / 16.0,
                    f64::from(b.u) / 16.0,
                    f64::from(a.v) / 16.0,
                    f64::from(b.v) / 16.0,
                )
            } else {
                let q = if b.q == 0.0 { 1.0 } else { f64::from(b.q) };
                let (w, h) = (f64::from(tex.w), f64::from(tex.h));
                (
                    f64::from(a.s) / q * w,
                    f64::from(b.s) / q * w,
                    f64::from(a.t) / q * h,
                    f64::from(b.t) / q * h,
                )
            };
            let span = |p0: i32, p1: i32, c0: f64, c1: f64, start: i32| -> (f64, f64) {
                if p0 == p1 {
                    (c0, 0.0)
                } else {
                    let d = (c1 - c0) * 16.0 / f64::from(p1 - p0);
                    (c0 + (c1 - c0) * f64::from(start * 16 - p0) / f64::from(p1 - p0), d)
                }
            };
            (u0, du) = span(ax, bx, ua, ub, px0);
            (v0, dv) = span(ay, by, va, vb, py0);
        }
        let fog = u32::from(b.fog);
        for y in py0..py1 {
            let v = v0 + dv * f64::from(y - py0);
            let mut u = u0;
            for x in px0..px1 {
                self.fragment(e, x, y, b.z, color, (u, v, q), fog);
                u += du;
            }
        }
    }

    fn draw_triangle(&mut self, e: &Env, v: &[Vertex]) {
        let p: [(i64, i64); 3] =
            std::array::from_fn(|i| (i64::from(v[i].x as i32 - e.ofx), i64::from(v[i].y as i32 - e.ofy)));
        let edge = |a: (i64, i64), b: (i64, i64), c: (i64, i64)| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
        let area = edge(p[0], p[1], p[2]);
        if area == 0 {
            return;
        }
        let sign = area.signum();
        let minx = p.iter().map(|q| q.0).min().unwrap_or(0);
        let maxx = p.iter().map(|q| q.0).max().unwrap_or(0);
        let miny = p.iter().map(|q| q.1).min().unwrap_or(0);
        let maxy = p.iter().map(|q| q.1).max().unwrap_or(0);
        let px0 = (((minx + 15) >> 4) as i32).max(e.sx0).max(0);
        let px1 = (((maxx + 15) >> 4) as i32).min(e.sx1 + 1);
        let py0 = (((miny + 15) >> 4) as i32).max(e.sy0).max(0);
        let py1 = (((maxy + 15) >> 4) as i32).min(e.sy1 + 1);
        if px0 >= px1 || py0 >= py1 {
            return;
        }
        // Edge functions: w_k = edge(p_i, p_j, sample), positive inside,
        // stepped per pixel (16 units of 1/16 pixel) and per row.
        let edges = [(1, 2), (2, 0), (0, 1)];
        let mut w_row = [0i64; 3];
        let mut step_x = [0i64; 3];
        let mut step_y = [0i64; 3];
        let mut bias = [0i64; 3];
        let start = (i64::from(px0) * 16, i64::from(py0) * 16);
        for (k, &(i, j)) in edges.iter().enumerate() {
            let (dx, dy) = ((p[j].0 - p[i].0) * sign, (p[j].1 - p[i].1) * sign);
            w_row[k] = edge(p[i], p[j], start) * sign;
            step_x[k] = -dy * 16;
            step_y[k] = dx * 16;
            // The top-left rule: pixels exactly on an edge belong to it only
            // if it is a top or a left edge.
            bias[k] = if (dy == 0 && dx < 0) || dy > 0 { 0 } else { -1 };
        }
        // Attributes are planes: their value at the first pixel, and their
        // steps per pixel and per row, from the barycentric weights'.
        let inv = 1.0 / (area * sign) as f64;
        let attr = |f: &dyn Fn(&Vertex) -> f64| -> (f64, f64, f64) {
            let a = [f(&v[0]), f(&v[1]), f(&v[2])];
            // Weight k is w_k / area, for the vertex opposite edge k.
            let at = |w: [f64; 3]| a[0] * w[0] + a[1] * w[1] + a[2] * w[2];
            (
                at(w_row.map(|w| w as f64 * inv)),
                at(step_x.map(|w| w as f64 * inv)),
                at(step_y.map(|w| w as f64 * inv)),
            )
        };
        // z, r, g, b, a, s, t, q, u, v, fog.
        let planes: [(f64, f64, f64); 11] = [
            attr(&|q| f64::from(q.z)),
            attr(&|q| f64::from(q.rgba[0])),
            attr(&|q| f64::from(q.rgba[1])),
            attr(&|q| f64::from(q.rgba[2])),
            attr(&|q| f64::from(q.rgba[3])),
            attr(&|q| f64::from(q.s)),
            attr(&|q| f64::from(q.t)),
            attr(&|q| f64::from(q.q)),
            attr(&|q| f64::from(q.u)),
            attr(&|q| f64::from(q.v)),
            attr(&|q| f64::from(q.fog)),
        ];
        let flat = v[2].rgba.map(u32::from);
        // Z stays within the vertices' range: interpolated in floats and
        // truncated, a flat 0xFFFFFE came out 0xFFFFFD in places, failing
        // a GEQUAL test against a neighbour's (Liberty City Stories' text,
        // its glyphs' quads overlapping).
        let zmin = v.iter().take(3).map(|q| f64::from(q.z)).fold(f64::MAX, f64::min);
        let zmax = v.iter().take(3).map(|q| f64::from(q.z)).fold(0.0, f64::max);
        let mut row_vals: [f64; 11] = planes.map(|p| p.0);
        for y in py0..py1 {
            let mut w = w_row;
            let mut vals = row_vals;
            for x in px0..px1 {
                if w[0] + bias[0] >= 0 && w[1] + bias[1] >= 0 && w[2] + bias[2] >= 0 {
                    let z = vals[0].clamp(zmin, zmax) as u32;
                    let color = if e.iip {
                        [1, 2, 3, 4].map(|k| vals[k].clamp(0.0, 255.0) as u32)
                    } else {
                        flat
                    };
                    let tc = self.texcoord(e, vals[5], vals[6], vals[7], vals[8], vals[9]);
                    self.fragment(e, x, y, z, color, tc, vals[10] as u32);
                }
                for k in 0..3 {
                    w[k] += step_x[k];
                }
                for (val, plane) in vals.iter_mut().zip(&planes) {
                    *val += plane.1;
                }
            }
            for k in 0..3 {
                w_row[k] += step_y[k];
            }
            for (val, plane) in row_vals.iter_mut().zip(&planes) {
                *val += plane.2;
            }
        }
    }

    // --- Textures ------------------------------------------------------------------

    /// Texel coordinates, from STQ or UV (12.4), and Q (1 with UV), for the
    /// level of detail.
    fn texcoord(&self, e: &Env, s: f64, t: f64, q: f64, u: f64, v: f64) -> (f64, f64, f64) {
        match &e.tex {
            None => (0.0, 0.0, 1.0),
            Some(_) if e.fst => (u / 16.0, v / 16.0, 1.0),
            Some(tex) => {
                let q = if q == 0.0 { 1.0 } else { q };
                (s / q * f64::from(tex.w), t / q * f64::from(tex.h), q)
            }
        }
    }

    /// Whether a texel at that Q is filtered (bilinear): the level of
    /// detail decides between the magnifying and the minifying filter.
    fn linear(tex: &Texture, q: f64) -> bool {
        let lod = if tex.lcm {
            tex.k
        } else {
            (1.0 / q.abs()).log2() * f64::from(1 << tex.l) + tex.k
        };
        if lod <= 0.0 { tex.mag_linear } else { tex.min_linear }
    }

    /// The texel at (u, v), filtered or not, as RGBA8.
    fn sample(&self, tex: &Texture, (u, v, q): (f64, f64, f64)) -> [u32; 4] {
        if !Self::linear(tex, q) {
            return self.texel(tex, u.floor() as i32, v.floor() as i32);
        }
        // Bilinear: the four texels around (u - 0.5, v - 0.5), weighted
        // with 4 fractional bits.
        let fu = ((u - 0.5) * 16.0).floor() as i32;
        let fv = ((v - 0.5) * 16.0).floor() as i32;
        let (u0, v0, a, b) = (fu >> 4, fv >> 4, (fu & 15) as u32, (fv & 15) as u32);
        let t00 = self.texel(tex, u0, v0);
        let t10 = self.texel(tex, u0 + 1, v0);
        let t01 = self.texel(tex, u0, v0 + 1);
        let t11 = self.texel(tex, u0 + 1, v0 + 1);
        std::array::from_fn(|c| {
            let top = t00[c] * (16 - a) + t10[c] * a;
            let bottom = t01[c] * (16 - a) + t11[c] * a;
            (top * (16 - b) + bottom * b) >> 8
        })
    }

    fn wrap(mode: u32, c: i32, size: i32, min: i32, max: i32) -> i32 {
        match mode {
            // Texture sizes are powers of two.
            0 => c & (size - 1),
            1 => c.clamp(0, size - 1),
            2 => c.clamp(min, max),
            _ => (c & min) | max,
        }
    }

    /// A texel as RGBA8 (alpha 0x80 is 1.0).
    fn texel(&self, tex: &Texture, u: i32, v: i32) -> [u32; 4] {
        let u = Self::wrap(tex.wms, u, tex.w, tex.minu, tex.maxu);
        let v = Self::wrap(tex.wmt, v, tex.h, tex.minv, tex.maxv);
        let raw = self.mem.read(tex.psm, tex.tbp, tex.tbw, u as u32, v as u32);
        let c = match tex.psm {
            psm::CT32 => raw,
            psm::CT24 => {
                let a = if tex.aem && raw & 0xFF_FFFF == 0 { 0 } else { tex.ta0 };
                (raw & 0xFF_FFFF) | (a << 24)
            }
            psm::CT16 | psm::CT16S => self.expand16_texa(tex, raw),
            _ => {
                let index = if bits(tex.psm) == 4 {
                    (tex.csa * 16 + raw) & 255
                } else {
                    raw & 255
                };
                let entry = self.clut[index as usize];
                if tex.cpsm == psm::CT32 || tex.cpsm == psm::CT24 {
                    entry
                } else {
                    self.expand16_texa(tex, entry)
                }
            }
        };
        [c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF, c >> 24]
    }

    fn expand16_texa(&self, tex: &Texture, raw: u32) -> u32 {
        let rgb = expand16(raw) & 0xFF_FFFF;
        let a = if raw & 0x8000 != 0 {
            tex.ta1
        } else if tex.aem && raw & 0x7FFF == 0 {
            0
        } else {
            tex.ta0
        };
        rgb | (a << 24)
    }

    // --- The pixel pipeline -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn fragment(&mut self, e: &Env, x: i32, y: i32, z: u32, color: [u32; 4], tc: (f64, f64, f64), fog: u32) {
        if x < e.sx0 || x > e.sx1 || y < e.sy0 || y > e.sy1 || x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);
        let [mut r, mut g, mut b, mut a] = color;
        // Texture function.
        if let Some(tex) = &e.tex {
            let [tr, tg, tb, ta] = self.sample(tex, tc);
            let modulate = |t: u32, f: u32| ((t * f) >> 7).min(255);
            (r, g, b, a) = match tex.tfx {
                0 => (
                    modulate(tr, r),
                    modulate(tg, g),
                    modulate(tb, b),
                    if tex.tcc { modulate(ta, a) } else { a },
                ),
                1 => (tr, tg, tb, if tex.tcc { ta } else { a }),
                2 => (
                    (modulate(tr, r) + a).min(255),
                    (modulate(tg, g) + a).min(255),
                    (modulate(tb, b) + a).min(255),
                    if tex.tcc { (ta + a).min(255) } else { a },
                ),
                _ => (
                    (modulate(tr, r) + a).min(255),
                    (modulate(tg, g) + a).min(255),
                    (modulate(tb, b) + a).min(255),
                    if tex.tcc { ta } else { a },
                ),
            };
        }
        if e.fge {
            let f = fog.min(255);
            let mix = |c: u32, k: u32| (f * c + (255 - f) * k) >> 8;
            (r, g, b) = (mix(r, e.fogcol[0]), mix(g, e.fogcol[1]), mix(b, e.fogcol[2]));
        }
        // Alpha test: what to write when it fails.
        let (mut write_fb, mut write_z, mut write_alpha) = (true, !e.zmsk, true);
        if e.ate {
            let pass = match e.atst {
                0 => false,
                1 => true,
                2 => a < e.aref,
                3 => a <= e.aref,
                4 => a == e.aref,
                5 => a >= e.aref,
                6 => a > e.aref,
                _ => a != e.aref,
            };
            if !pass {
                match e.afail {
                    0 => return,
                    1 => write_z = false,
                    2 => write_fb = false,
                    _ => {
                        write_z = false;
                        write_alpha = false;
                    }
                }
            }
        }
        // Depth test.
        let zmax = match e.zpsm {
            psm::Z24 => 0xFF_FFFF,
            psm::Z16 | psm::Z16S => 0xFFFF,
            _ => u32::MAX,
        };
        let z = z.min(zmax);
        if e.zte {
            let pass = match e.ztst {
                0 => false,
                1 => true,
                _ => {
                    let old = self.mem.read(e.zpsm, e.zbp, e.fbw, x, y);
                    if e.ztst == 2 { z >= old } else { z > old }
                }
            };
            if !pass {
                return;
            }
        }
        // The frame buffer's pixel, as RGBA8, when anything needs it.
        let (old_raw, old) = if e.abe || e.date || e.fbmsk != 0 || !write_alpha {
            let raw = self.mem.read(e.fpsm, e.fbp, e.fbw, x, y);
            let rgba = match e.fpsm {
                psm::CT16 | psm::CT16S => expand16(raw),
                psm::CT24 => raw | 0x8000_0000,
                _ => raw,
            };
            (raw, rgba)
        } else {
            (0, 0)
        };
        // Destination alpha test.
        if e.date {
            let bit = match e.fpsm {
                psm::CT16 | psm::CT16S => old_raw & 0x8000 != 0,
                _ => old_raw & 0x8000_0000 != 0,
            };
            if bit != e.datm {
                return;
            }
        }
        if e.abe {
            let src = [r, g, b];
            let dst = [old & 0xFF, (old >> 8) & 0xFF, (old >> 16) & 0xFF];
            let ad = old >> 24;
            let pick = |sel: u32, k: usize| -> i32 {
                match sel {
                    0 => src[k] as i32,
                    1 => dst[k] as i32,
                    _ => 0,
                }
            };
            let cf = match e.alpha[2] {
                0 => a,
                1 => ad,
                _ => e.fix,
            } as i32;
            let out: [i32; 3] = std::array::from_fn(|k| {
                (((pick(e.alpha[0], k) - pick(e.alpha[1], k)) * cf) >> 7) + pick(e.alpha[3], k)
            });
            let fix = |c: i32| {
                if e.clamp {
                    c.clamp(0, 255) as u32
                } else {
                    (c as u32) & 0xFF
                }
            };
            (r, g, b) = (fix(out[0]), fix(out[1]), fix(out[2]));
        }
        if e.fba {
            a |= 0x80;
        }
        if write_fb {
            let mut new = r | (g << 8) | (b << 16) | (a.min(255) << 24);
            let mut mask = e.fbmsk;
            if !write_alpha {
                mask |= 0xFF00_0000;
            }
            if mask != 0 {
                new = (old & mask) | (new & !mask);
            }
            let raw = match e.fpsm {
                psm::CT16 | psm::CT16S => {
                    ((new & 0xFF) >> 3)
                        | (((new >> 8) & 0xFF) >> 3) << 5
                        | (((new >> 16) & 0xFF) >> 3) << 10
                        | ((new >> 31) << 15)
                }
                _ => new,
            };
            self.mem.write(e.fpsm, e.fbp, e.fbw, x, y, raw);
        }
        if write_z && e.zte {
            self.mem.write(e.zpsm, e.zbp, e.fbw, x, y, z);
        }
    }
}

/// Pixel `i` of a line from `a` to `b` (in 1/16 pixels) drawn in `steps`
/// pixels: floor(a + (b - a) * i / steps), exactly.
fn line_pixel(a: (i32, i32), b: (i32, i32), i: i32, steps: i32) -> (i32, i32) {
    let steps = i64::from(steps.max(1));
    let at = |p: i32, q: i32| (i64::from(p) * steps + i64::from(q - p) * i64::from(i)).div_euclid(16 * steps) as i32;
    (at(a.0, b.0), at(a.1, b.1))
}

#[cfg(test)]
mod tests {
    use super::super::{Gs, reg};

    /// A 64×64 CT32 frame at page 0, no depth, scissor over it all.
    fn gs() -> Gs {
        let mut gs = Gs::default();
        gs.write_reg(reg::FRAME_1, 1 << 16);
        gs.write_reg(reg::SCISSOR_1, (63 << 16) | (63 << 48));
        gs.write_reg(reg::TEST_1, 0);
        gs.write_reg(reg::PRMODECONT, 1);
        gs
    }

    fn xyz(x: u32, y: u32) -> u64 {
        u64::from(x * 16) | (u64::from(y * 16) << 16)
    }

    #[test]
    fn flat_sprite_and_triangle() {
        let mut gs = gs();
        gs.write_reg(reg::PRIM, 6);
        gs.write_reg(reg::RGBAQ, 0x8011_2233);
        gs.write_reg(reg::XYZ2, xyz(2, 2));
        gs.write_reg(reg::XYZ2, xyz(6, 4));
        assert_eq!(gs.mem.read(0, 0, 1, 2, 2), 0x8011_2233);
        assert_eq!(gs.mem.read(0, 0, 1, 5, 3), 0x8011_2233);
        assert_eq!(gs.mem.read(0, 0, 1, 6, 3), 0);
        assert_eq!(gs.mem.read(0, 0, 1, 5, 4), 0);
        // A triangle covering (10..20) × (10..20)'s lower-left half.
        gs.write_reg(reg::PRIM, 3);
        gs.write_reg(reg::RGBAQ, 0x80FF_0000);
        gs.write_reg(reg::XYZ2, xyz(10, 10));
        gs.write_reg(reg::XYZ2, xyz(10, 20));
        gs.write_reg(reg::XYZ2, xyz(20, 20));
        assert_eq!(gs.mem.read(0, 0, 1, 11, 18), 0x80FF_0000);
        assert_eq!(gs.mem.read(0, 0, 1, 18, 11), 0);
    }

    #[test]
    fn alpha_blending() {
        let mut gs = gs();
        gs.write_reg(reg::PRIM, 6);
        gs.write_reg(reg::RGBAQ, 0x8000_00C8);
        gs.write_reg(reg::XYZ2, xyz(0, 0));
        gs.write_reg(reg::XYZ2, xyz(1, 1));
        // (Cs - Cd) * As + Cd with As = 0x40 (half): halfway.
        gs.write_reg(reg::ALPHA_1, 0b01_00_01_00);
        gs.write_reg(reg::PRIM, 6 | (1 << 6));
        gs.write_reg(reg::RGBAQ, 0x4000_0000);
        gs.write_reg(reg::XYZ2, xyz(0, 0));
        gs.write_reg(reg::XYZ2, xyz(1, 1));
        assert_eq!(gs.mem.read(0, 0, 1, 0, 0) & 0xFF, 100);
    }
}
