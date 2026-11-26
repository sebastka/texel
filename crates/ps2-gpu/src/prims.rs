//! Primitive setup, on the CPU: a recorded draw becomes a record of
//! `PRIM_WORDS` words for the shader (see gs.wgsl): its covered area, edge
//! functions (triangles) or end points (lines), its attributes as planes,
//! and the registers its pixels need. The same setup as the software GS's
//! draw.rs, so that both draw the same pixels.

use texel_ps2::gs::{DrawRegs, Vertex};

pub const PRIM_WORDS: usize = 64;

const KIND_RECT: u32 = 0;
const KIND_TRIANGLE: u32 = 1;
const KIND_LINE: u32 = 2;

fn field(v: u64, lo: u32, n: u32) -> u32 {
    ((v >> lo) & ((1u64 << n) - 1)) as u32
}

/// A primitive ready for the GPU, and what batching needs to know of it.
pub struct Prim {
    pub words: [u32; PRIM_WORDS],
    /// Covered area: x0, y0, x1, y1 (end excluded).
    pub bbox: [i32; 4],
    /// Memory it writes (frame and depth buffers) and reads (its texture).
    pub writes: [Option<Region>; 2],
    pub reads: Option<Region>,
}

/// A range of GS memory words, and the layout a primitive addresses it
/// through (base, width, format): two primitives with the same layout put
/// a pixel at the same address, so the same GPU thread handles both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub start: u32,
    pub end: u32,
    pub layout: u32,
}

impl Region {
    pub fn overlaps(&self, o: &Region) -> bool {
        self.start < o.end && o.start < self.end
    }
}

/// The eleven attributes of a vertex: z, r, g, b, a, s, t, q, u, v, fog.
fn attrs(v: &Vertex) -> [f64; 11] {
    [
        f64::from(v.z),
        f64::from(v.rgba[0]),
        f64::from(v.rgba[1]),
        f64::from(v.rgba[2]),
        f64::from(v.rgba[3]),
        f64::from(v.s),
        f64::from(v.t),
        f64::from(v.q),
        f64::from(v.u),
        f64::from(v.v),
        f64::from(v.fog),
    ]
}

fn color(v: &Vertex) -> u32 {
    u32::from_le_bytes(v.rgba)
}

struct Env {
    ofx: i32,
    ofy: i32,
    /// The scissor, end excluded, within the 2048×2048 space.
    clip: [i32; 4],
}

fn env(r: &DrawRegs) -> Env {
    let s = r.scissor;
    Env {
        ofx: field(r.xyoffset, 0, 16) as i32,
        ofy: field(r.xyoffset, 32, 16) as i32,
        clip: [
            field(s, 0, 11) as i32,
            field(s, 32, 11) as i32,
            (field(s, 16, 11) as i32 + 1).min(2048),
            (field(s, 48, 11) as i32 + 1).min(2048),
        ],
    }
}

/// The record's registers, and the memory the primitive touches.
fn registers(
    words: &mut [u32; PRIM_WORDS],
    r: &DrawRegs,
    prim: u64,
    clut: u32,
    y1: i32,
) -> ([Option<Region>; 2], Option<Region>) {
    words[48] = prim as u32;
    words[49] = r.frame as u32;
    words[50] = (r.frame >> 32) as u32;
    // ZMSK (bit 32) moves to bit 31, which ZBUF does not use.
    words[51] = (r.zbuf as u32 & 0x0F00_01FF) | (field(r.zbuf, 32, 1) << 31);
    words[52] = r.test as u32;
    words[53] = r.alpha as u32;
    words[54] = field(r.alpha, 32, 8);
    words[55] = r.tex0 as u32;
    words[56] = (r.tex0 >> 32) as u32;
    words[57] = r.clamp as u32;
    words[58] = (r.clamp >> 32) as u32;
    words[59] = r.texa as u32;
    words[60] = field(r.texa, 32, 8);
    words[61] = r.fogcol as u32;
    // Flags: FBA, COLCLAMP; TEX1's LCM, L, MMAG, MMIN and K, for filtering.
    words[62] = field(r.fba, 0, 1)
        | (field(r.colclamp, 0, 1) << 1)
        | (field(r.tex1, 0, 1) << 8)
        | (field(r.tex1, 19, 2) << 9)
        | (field(r.tex1, 5, 1) << 11)
        | (field(r.tex1, 6, 3) << 12)
        | (field(r.tex1, 32, 12) << 16);
    words[63] = clut;
    // Frame and depth buffers: whole pages, down to the last row drawn.
    let rows = (y1.max(1) as u32).div_ceil(32);
    let fbp = field(r.frame, 0, 9) * 2048;
    let fbw = field(r.frame, 16, 6).max(1);
    let zbp = field(r.zbuf, 0, 9) * 2048;
    let frame = (field(r.frame, 32, 32) != u32::MAX).then_some(Region {
        start: fbp,
        end: fbp + fbw * rows * 2048,
        layout: field(r.frame, 0, 30),
    });
    // The depth buffer counts when tested (read) or written.
    let zte = field(r.test, 16, 1) != 0;
    let depth = zte.then_some(Region {
        start: zbp,
        end: zbp + fbw * rows * 2048,
        layout: (field(r.zbuf, 0, 9) | field(r.zbuf, 24, 4) << 24) | fbw << 16 | 1 << 31,
    });
    let writes = [frame, depth];
    let reads = (field(prim, 4, 1) != 0).then(|| {
        let tbp = field(r.tex0, 0, 14) * 64;
        let bits = match field(r.tex0, 20, 6) {
            0x02 | 0x0A => 16,
            0x13 | 0x1B => 8,
            0x14 | 0x24 | 0x2C => 4,
            _ => 32,
        };
        let w = 1u32 << field(r.tex0, 26, 4).min(10);
        let h = 1u32 << field(r.tex0, 30, 4).min(10);
        // Whole pages too: the layouts spread a texture over its pages.
        let words = (w * h * bits / 32).div_ceil(2048) * 2048;
        Region {
            start: tbp,
            end: tbp + words.max(2048),
            layout: r.tex0 as u32,
        }
    });
    (writes, reads)
}

fn put_f(words: &mut [u32; PRIM_WORDS], at: usize, v: f64) {
    words[at] = (v as f32).to_bits();
}

/// Sets up a draw. `None` when nothing is covered.
pub fn setup(kind: u32, regs: &DrawRegs, v: &[Vertex], clut: u32) -> Option<Prim> {
    let e = env(regs);
    let mut words = [0u32; PRIM_WORDS];
    let mut prim = regs.prim;
    let bbox = match kind {
        0 => {
            let x = (v[0].x as i32 - e.ofx + 8) >> 4;
            let y = (v[0].y as i32 - e.ofy + 8) >> 4;
            prim &= !8;
            words[0] = KIND_RECT;
            let a = attrs(&v[0]);
            for (k, value) in a.iter().enumerate() {
                put_f(&mut words, 14 + 3 * k, *value);
            }
            words[47] = color(&v[0]);
            [x, y, x + 1, y + 1]
        }
        1 | 2 => line(&e, &v[0], &v[1], &mut words, &mut prim)?,
        3..=5 => triangle(&e, v, &mut words)?,
        _ => {
            prim &= !8;
            sprite(&e, &v[0], &v[1], regs, &mut words)?
        }
    };
    let bbox = [
        bbox[0].max(e.clip[0]).max(0),
        bbox[1].max(e.clip[1]).max(0),
        bbox[2].min(e.clip[2]),
        bbox[3].min(e.clip[3]),
    ];
    if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
        return None;
    }
    // Planes are relative to the box's corner: the box may have shrunk.
    if words[0] != KIND_LINE {
        let (di, dj) = (bbox[0] - words[1] as i32, bbox[1] - words[2] as i32);
        if di != 0 || dj != 0 {
            for k in 0..11 {
                let at = 14 + 3 * k;
                let base = f64::from(f32::from_bits(words[at]));
                let dx = f64::from(f32::from_bits(words[at + 1]));
                let dy = f64::from(f32::from_bits(words[at + 2]));
                put_f(&mut words, at, base + dx * f64::from(di) + dy * f64::from(dj));
            }
            if words[0] == KIND_TRIANGLE {
                for k in 0..3 {
                    let w = words[5 + k] as i32 as i64
                        + i64::from(words[8 + k] as i32) * i64::from(di)
                        + i64::from(words[11 + k] as i32) * i64::from(dj);
                    words[5 + k] = w.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32 as u32;
                }
            }
        }
    }
    for (k, b) in bbox.iter().enumerate() {
        words[1 + k] = *b as u32;
    }
    let (writes, reads) = registers(&mut words, regs, prim, clut, bbox[3]);
    Some(Prim {
        words,
        bbox,
        writes,
        reads,
    })
}

fn sprite(e: &Env, a: &Vertex, b: &Vertex, _regs: &DrawRegs, words: &mut [u32; PRIM_WORDS]) -> Option<[i32; 4]> {
    let (ax, ay) = (a.x as i32 - e.ofx, a.y as i32 - e.ofy);
    let (bx, by) = (b.x as i32 - e.ofx, b.y as i32 - e.ofy);
    let (x0, x1) = (ax.min(bx), ax.max(bx));
    let (y0, y1) = (ay.min(by), ay.max(by));
    let bbox = [(x0 + 15) >> 4, (y0 + 15) >> 4, (x1 + 15) >> 4, (y1 + 15) >> 4];
    words[0] = KIND_RECT;
    let (va, vb) = (attrs(a), attrs(b));
    // Along each axis, from a to b: s and u with x, t and v with y.
    let span = |p0: i32, p1: i32, c0: f64, c1: f64, start: i32| -> (f64, f64) {
        if p0 == p1 {
            (c0, 0.0)
        } else {
            let d = (c1 - c0) * 16.0 / f64::from(p1 - p0);
            (c0 + (c1 - c0) * f64::from(start * 16 - p0) / f64::from(p1 - p0), d)
        }
    };
    for k in 0..11 {
        let at = 14 + 3 * k;
        let (base, dx, dy) = match k {
            5 | 8 => {
                let (b0, d) = span(ax, bx, va[k], vb[k], bbox[0]);
                (b0, d, 0.0)
            }
            6 | 9 => {
                let (b0, d) = span(ay, by, va[k], vb[k], bbox[1]);
                (b0, 0.0, d)
            }
            _ => (vb[k], 0.0, 0.0),
        };
        put_f(words, at, base);
        put_f(words, at + 1, dx);
        put_f(words, at + 2, dy);
    }
    words[47] = color(b);
    words[1] = bbox[0] as u32;
    words[2] = bbox[1] as u32;
    Some(bbox)
}

fn line(e: &Env, a: &Vertex, b: &Vertex, words: &mut [u32; PRIM_WORDS], prim: &mut u64) -> Option<[i32; 4]> {
    let (x0, y0) = (
        f64::from(a.x as i32 - e.ofx) / 16.0,
        f64::from(a.y as i32 - e.ofy) / 16.0,
    );
    let (x1, y1) = (
        f64::from(b.x as i32 - e.ofx) / 16.0,
        f64::from(b.y as i32 - e.ofy) / 16.0,
    );
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).round() as i32;
    let (va, vb) = (attrs(a), attrs(b));
    words[47] = color(b);
    if steps == 0 {
        // A single pixel.
        *prim &= !8;
        words[0] = KIND_RECT;
        for (k, &v) in va.iter().enumerate() {
            put_f(words, 14 + 3 * k, v);
        }
        let (x, y) = (x0.floor() as i32, y0.floor() as i32);
        words[1] = x as u32;
        words[2] = y as u32;
        return Some([x, y, x + 1, y + 1]);
    }
    words[0] = KIND_LINE;
    // Ends in 1/16 pixels: pixel n is floor(a + (b - a) * n / steps), in
    // integers, as the software GS does.
    let (a16, b16) = (
        (a.x as i32 - e.ofx, a.y as i32 - e.ofy),
        (b.x as i32 - e.ofx, b.y as i32 - e.ofy),
    );
    words[5] = a16.0 as u32;
    words[6] = a16.1 as u32;
    words[7] = b16.0 as u32;
    words[8] = b16.1 as u32;
    words[9] = steps as u32;
    // Planes along the line: by its position t in [0, 1).
    for k in 0..11 {
        put_f(words, 14 + 3 * k, va[k]);
        put_f(words, 15 + 3 * k, vb[k] - va[k]);
    }
    // Pixels move monotonically: the first and last bound them.
    let first = line_pixel(a16, b16, 0, steps);
    let last = line_pixel(a16, b16, steps - 1, steps);
    let bbox = [
        first.0.min(last.0),
        first.1.min(last.1),
        first.0.max(last.0) + 1,
        first.1.max(last.1) + 1,
    ];
    words[1] = bbox[0] as u32;
    words[2] = bbox[1] as u32;
    Some(bbox)
}

/// Pixel `n` of a line from `a` to `b` (1/16 pixels) in `steps` pixels.
fn line_pixel(a: (i32, i32), b: (i32, i32), n: i32, steps: i32) -> (i32, i32) {
    let steps = i64::from(steps.max(1));
    let at = |p: i32, q: i32| (i64::from(p) * steps + i64::from(q - p) * i64::from(n)).div_euclid(16 * steps) as i32;
    (at(a.0, b.0), at(a.1, b.1))
}

fn triangle(e: &Env, v: &[Vertex], words: &mut [u32; PRIM_WORDS]) -> Option<[i32; 4]> {
    let p: [(i64, i64); 3] =
        std::array::from_fn(|i| (i64::from(v[i].x as i32 - e.ofx), i64::from(v[i].y as i32 - e.ofy)));
    let edge = |a: (i64, i64), b: (i64, i64), c: (i64, i64)| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
    let area = edge(p[0], p[1], p[2]);
    if area == 0 {
        return None;
    }
    let sign = area.signum();
    let minx = p.iter().map(|q| q.0).min().unwrap_or(0);
    let maxx = p.iter().map(|q| q.0).max().unwrap_or(0);
    let miny = p.iter().map(|q| q.1).min().unwrap_or(0);
    let maxy = p.iter().map(|q| q.1).max().unwrap_or(0);
    // The box is clipped later; the corner the edges start from is the
    // scissor's when the triangle reaches beyond it, to keep them small.
    let bx0 = (((minx + 15) >> 4) as i32).max(e.clip[0]);
    let by0 = (((miny + 15) >> 4) as i32).max(e.clip[1]);
    let bbox = [bx0, by0, ((maxx + 15) >> 4) as i32, ((maxy + 15) >> 4) as i32];
    let edges = [(1, 2), (2, 0), (0, 1)];
    let start = (i64::from(bx0) * 16, i64::from(by0) * 16);
    let mut w = [0i64; 3];
    let mut unbiased = [0i64; 3];
    let mut sx = [0i64; 3];
    let mut sy = [0i64; 3];
    for (k, &(i, j)) in edges.iter().enumerate() {
        let (dx, dy) = ((p[j].0 - p[i].0) * sign, (p[j].1 - p[i].1) * sign);
        // The top-left rule, folded into the value: with the edges oriented
        // so the inside is positive (y down), a left edge goes up (dy < 0)
        // and a top edge right (dy = 0, dx > 0).
        let bias = if (dy == 0 && dx > 0) || dy < 0 { 0 } else { -1 };
        unbiased[k] = edge(p[i], p[j], start) * sign;
        w[k] = unbiased[k] + bias;
        sx[k] = -dy * 16;
        sy[k] = dx * 16;
    }
    // Attribute planes, from the barycentric weights' (as draw.rs).
    let inv = 1.0 / (area * sign) as f64;
    let base_w: [f64; 3] = unbiased.map(|u| u as f64 * inv);
    let a = v.iter().map(attrs).collect::<Vec<_>>();
    for k in 0..11 {
        let at = |weights: [f64; 3]| a[0][k] * weights[0] + a[1][k] * weights[1] + a[2][k] * weights[2];
        put_f(words, 14 + 3 * k, at(base_w));
        put_f(words, 15 + 3 * k, at(sx.map(|s| s as f64 * inv)));
        put_f(words, 16 + 3 * k, at(sy.map(|s| s as f64 * inv)));
    }
    words[0] = KIND_TRIANGLE;
    words[47] = color(&v[2]);
    for k in 0..3 {
        words[5 + k] = w[k].clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32 as u32;
        words[8 + k] = sx[k] as i32 as u32;
        words[11 + k] = sy[k] as i32 as u32;
    }
    words[1] = bx0 as u32;
    words[2] = by0 as u32;
    Some(bbox)
}
