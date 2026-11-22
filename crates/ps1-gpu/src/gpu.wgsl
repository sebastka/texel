// The PlayStation's GPU at a higher internal resolution: compute kernels
// replaying what the software GPU records (texel-ps1's gpu_record), with its
// integer arithmetic, so that both draw the same pixels.
//
// `hires`: the scaled VRAM, a 16-bit pixel per u32, (1024 * scale) by
// (512 * scale). `native`: the native VRAM as drawn by the software GPU
// (sent when textures need it), 1024 by 512. `words`: the primitives (see
// lib.rs for their layout) and the data of uploads, writes and copies.

@group(0) @binding(0) var<storage, read_write> hires: array<u32>;
@group(0) @binding(1) var<storage, read_write> native: array<u32>;
@group(0) @binding(2) var<storage, read> words: array<u32>;
@group(0) @binding(3) var<uniform> job: Job;
@group(0) @binding(4) var<storage, read_write> screen: array<u32>;

struct Job {
    a: vec4<u32>,
    b: vec4<u32>,
    c: vec4<u32>,
    d: vec4<u32>,
}

const PRIM_WORDS: u32 = 64u;

// --- 64-bit integers (two's complement, as lo and hi words) ----------------

fn word64(at: u32) -> vec2<u32> {
    return vec2<u32>(words[at], words[at + 1u]);
}

fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    return vec2<u32>(lo, a.y + b.y + select(0u, 1u, lo < a.x));
}

// a * k, k below 65536.
fn mul64(a: vec2<u32>, k: u32) -> vec2<u32> {
    let ll = (a.x & 0xffffu) * k;
    let lh = (a.x >> 16u) * k;
    let lo = ll + (lh << 16u);
    return vec2<u32>(lo, a.y * k + (lh >> 16u) + select(0u, 1u, lo < ll));
}

// (a >> 16) as i32.
fn shr16(a: vec2<u32>) -> i32 {
    return bitcast<i32>((a.y << 16u) | (a.x >> 16u));
}

// --- Pixels ----------------------------------------------------------------

fn stride() -> u32 {
    return 1024u * scale_now();
}

// The scale of the current job (every job carries it in d.w).
fn scale_now() -> u32 {
    return job.d.w;
}

const DITHER = array<array<i32, 4>, 4>(
    array<i32, 4>(-4, 0, -3, 1),
    array<i32, 4>(2, -2, 3, -1),
    array<i32, 4>(-3, 1, -4, 0),
    array<i32, 4>(3, -1, 2, -2),
);

fn vram(i: u32) -> u32 {
    return native[i] & 0xffffu;
}

fn texel(p: u32, u_in: i32, v_in: i32) -> u32 {
    let mask_x = bitcast<i32>(words[p + 4u]);
    let mask_y = bitcast<i32>(words[p + 5u]);
    let off_x = bitcast<i32>(words[p + 6u]);
    let off_y = bitcast<i32>(words[p + 7u]);
    let base_x = words[p + 8u];
    let base_y = words[p + 9u];
    let clut_row = words[p + 10u];
    let clut_x = words[p + 11u];
    let u = u32(((u_in & 0xff) & ~mask_x) | off_x);
    let v = u32(((v_in & 0xff) & ~mask_y) | off_y);
    let row = ((base_y + v) & 0x1ffu) * 1024u;
    switch words[p + 3u] {
        case 0u: {
            let w = vram(row + ((base_x + u / 4u) & 0x3ffu));
            let idx = (w >> ((u & 3u) * 4u)) & 0xfu;
            return vram(clut_row + ((clut_x + idx) & 0x3ffu));
        }
        case 1u: {
            let w = vram(row + ((base_x + u / 2u) & 0x3ffu));
            let idx = (w >> ((u & 1u) * 8u)) & 0xffu;
            return vram(clut_row + ((clut_x + idx) & 0x3ffu));
        }
        default: {
            return vram(row + ((base_x + u) & 0x3ffu));
        }
    }
}

// A pixel of primitive `p` at x, y (inside the drawing area), as the
// software GPU's `Raster::pixel`.
fn pixel(p: u32, x: i32, y: i32, r: i32, g: i32, b: i32, u: i32, v: i32) {
    let flags = words[p + 1u];
    let s = i32(words[p + 12u]);
    let idx = u32(y) * stride() + u32(x);
    let dst = hires[idx];
    if ((flags & 32u) != 0u && (dst & 0x8000u) != 0u) {
        return;
    }
    var dither = 0;
    if ((flags & 16u) != 0u) {
        dither = DITHER[(y / s) & 3][(x / s) & 3];
    }
    var cr: i32;
    var cg: i32;
    var cb: i32;
    var semi: bool;
    var mask_bit: u32;
    if ((flags & 1u) != 0u) {
        let t = texel(p, u, v);
        if (t == 0u) {
            return;
        }
        let tr = i32(t & 0x1fu);
        let tg = i32((t >> 5u) & 0x1fu);
        let tb = i32((t >> 10u) & 0x1fu);
        if ((flags & 8u) != 0u) {
            cr = tr;
            cg = tg;
            cb = tb;
        } else {
            cr = clamp((((tr << 3u) * r) >> 7u) + dither, 0, 255) >> 3u;
            cg = clamp((((tg << 3u) * g) >> 7u) + dither, 0, 255) >> 3u;
            cb = clamp((((tb << 3u) * b) >> 7u) + dither, 0, 255) >> 3u;
        }
        semi = (flags & 4u) != 0u && (t & 0x8000u) != 0u;
        mask_bit = t & 0x8000u;
    } else {
        cr = clamp(r + dither, 0, 255) >> 3u;
        cg = clamp(g + dither, 0, 255) >> 3u;
        cb = clamp(b + dither, 0, 255) >> 3u;
        semi = (flags & 4u) != 0u;
        mask_bit = 0u;
    }
    if (semi) {
        let br = i32(dst & 0x1fu);
        let bg = i32((dst >> 5u) & 0x1fu);
        let bb = i32((dst >> 10u) & 0x1fu);
        switch words[p + 2u] {
            case 0u: {
                cr = (br + cr) >> 1u;
                cg = (bg + cg) >> 1u;
                cb = (bb + cb) >> 1u;
            }
            case 1u: {
                cr = min(br + cr, 31);
                cg = min(bg + cg, 31);
                cb = min(bb + cb, 31);
            }
            case 2u: {
                cr = max(br - cr, 0);
                cg = max(bg - cg, 0);
                cb = max(bb - cb, 0);
            }
            default: {
                cr = min(br + (cr >> 2u), 31);
                cg = min(bg + (cg >> 2u), 31);
                cb = min(bb + (cb >> 2u), 31);
            }
        }
    }
    let set_mask = select(0u, 0x8000u, (flags & 64u) != 0u);
    hires[idx] = (u32(cr) & 0xffffu) | ((u32(cg) << 5u) & 0xffffu) | ((u32(cb) << 10u) & 0xffffu) | mask_bit | set_mask;
}

// --- Primitives --------------------------------------------------------------

fn triangle(p: u32, x: i32, y: i32) {
    let xs = bitcast<i32>(words[p + 13u]);
    let ys = bitcast<i32>(words[p + 15u]);
    let k = x - xs;
    let j = y - ys;
    for (var e = 0u; e < 3u; e++) {
        let w = bitcast<i32>(words[p + 17u + e]) + bitcast<i32>(words[p + 21u + 2u * e]) * j
            + bitcast<i32>(words[p + 20u + 2u * e]) * k;
        if (w < 0) {
            return;
        }
    }
    let flags = words[p + 1u];
    // Attributes at the pixel: at the first row's start, plus j rows and
    // k pixels of steps (16.16, 64 bits, as the software steps them).
    var attr: array<i32, 5>;
    for (var a = 0u; a < 5u; a++) {
        if ((a < 3u && (flags & 2u) == 0u) || (a >= 3u && (flags & 1u) == 0u)) {
            attr[a] = 0;
            continue;
        }
        var v = word64(p + 26u + 2u * a);
        v = add64(v, mul64(word64(p + 46u + 2u * a), u32(j)));
        v = add64(v, mul64(word64(p + 36u + 2u * a), u32(k)));
        attr[a] = shr16(v);
    }
    var r = bitcast<i32>(words[p + 56u]);
    var g = bitcast<i32>(words[p + 57u]);
    var b = bitcast<i32>(words[p + 58u]);
    if ((flags & 2u) != 0u) {
        r = attr[0];
        g = attr[1];
        b = attr[2];
    }
    pixel(p, x, y, r, g, b, attr[3], attr[4]);
}

fn rect(p: u32, x: i32, y: i32) {
    let s = i32(words[p + 12u]);
    let x0 = bitcast<i32>(words[p + 17u]);
    let y0 = bitcast<i32>(words[p + 18u]);
    let du = (x - x0) / s;
    let dv = (y - y0) / s;
    var u = bitcast<i32>(words[p + 19u]) + du;
    if (words[p + 24u] != 0u) {
        u = bitcast<i32>(words[p + 19u]) - du;
    }
    var v = bitcast<i32>(words[p + 20u]) + dv;
    if (words[p + 25u] != 0u) {
        v = bitcast<i32>(words[p + 20u]) - dv;
    }
    pixel(p, x, y, bitcast<i32>(words[p + 21u]), bitcast<i32>(words[p + 22u]), bitcast<i32>(words[p + 23u]), u, v);
}

// A precise triangle (from the GTE's exact projections): float edge
// functions with the same top-left rule, colours interpolated linearly,
// texture coordinates in perspective (from the vertices' depths).
fn fv(p: u32, k: u32, i: u32) -> f32 {
    return bitcast<f32>(words[p + 17u + 8u * k + i]);
}

fn iv(p: u32, k: u32, i: u32) -> i32 {
    return bitcast<i32>(words[p + 17u + 8u * k + i]);
}

// The edge function of a to b at (x, y), less a little when the edge is
// not a top or left one (its pixels belong to the neighbour).
fn edge(ax: f32, ay: f32, bx: f32, by: f32, x: f32, y: f32) -> bool {
    let e = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
    let top_left = (ay == by && bx > ax) || by < ay;
    return select(e > 0.0, e >= 0.0, top_left);
}

fn exact_triangle(p: u32, x: i32, y: i32) {
    let px = f32(x);
    let py = f32(y);
    let x0 = fv(p, 0u, 0u);
    let y0 = fv(p, 0u, 1u);
    let x1 = fv(p, 1u, 0u);
    let y1 = fv(p, 1u, 1u);
    let x2 = fv(p, 2u, 0u);
    let y2 = fv(p, 2u, 1u);
    if (!edge(x1, y1, x2, y2, px, py) || !edge(x2, y2, x0, y0, px, py) || !edge(x0, y0, x1, y1, px, py)) {
        return;
    }
    let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
    let l = vec3<f32>(
        ((x2 - x1) * (py - y1) - (y2 - y1) * (px - x1)) / area,
        ((x0 - x2) * (py - y2) - (y0 - y2) * (px - x2)) / area,
        ((x1 - x0) * (py - y0) - (y1 - y0) * (px - x0)) / area,
    );
    let flags = words[p + 1u];
    var r = iv(p, 0u, 3u);
    var g = iv(p, 0u, 4u);
    var b = iv(p, 0u, 5u);
    if ((flags & 2u) != 0u) {
        r = i32(floor(dot(l, vec3<f32>(f32(iv(p, 0u, 3u)), f32(iv(p, 1u, 3u)), f32(iv(p, 2u, 3u)))) + 0.5));
        g = i32(floor(dot(l, vec3<f32>(f32(iv(p, 0u, 4u)), f32(iv(p, 1u, 4u)), f32(iv(p, 2u, 4u)))) + 0.5));
        b = i32(floor(dot(l, vec3<f32>(f32(iv(p, 0u, 5u)), f32(iv(p, 1u, 5u)), f32(iv(p, 2u, 5u)))) + 0.5));
    }
    var u = 0;
    var v = 0;
    if ((flags & 1u) != 0u) {
        // In perspective when every vertex has a depth, else affine.
        let z = vec3<f32>(fv(p, 0u, 2u), fv(p, 1u, 2u), fv(p, 2u, 2u));
        var q = l;
        if (min(z.x, min(z.y, z.z)) > 0.0) {
            q = l / z;
        }
        let den = q.x + q.y + q.z;
        let us = vec3<f32>(f32(iv(p, 0u, 6u)), f32(iv(p, 1u, 6u)), f32(iv(p, 2u, 6u)));
        let vs = vec3<f32>(f32(iv(p, 0u, 7u)), f32(iv(p, 1u, 7u)), f32(iv(p, 2u, 7u)));
        u = i32(floor(dot(q, us) / den + 0.5));
        v = i32(floor(dot(q, vs) / den + 0.5));
    }
    pixel(p, x, y, r, g, b, u, v);
}

// A batch of triangles and rectangles: a thread per pixel of the area they
// cover goes through them in order. a: first primitive, count, area x, y.
@compute @workgroup_size(8, 8)
fn draw(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = bitcast<i32>(job.a.z) + i32(id.x);
    let y = bitcast<i32>(job.a.w) + i32(id.y);
    let first = job.a.x;
    for (var n = 0u; n < job.a.y; n++) {
        let p = (first + n) * PRIM_WORDS;
        if (x < bitcast<i32>(words[p + 13u]) || x > bitcast<i32>(words[p + 14u])
            || y < bitcast<i32>(words[p + 15u]) || y > bitcast<i32>(words[p + 16u])) {
            continue;
        }
        switch words[p] {
            case 0u: {
                triangle(p, x, y);
            }
            case 3u: {
                exact_triangle(p, x, y);
            }
            default: {
                rect(p, x, y);
            }
        }
    }
}

// A line, in one thread (its blocks overlap: their order matters), as the
// software GPU's `Raster::line`. a.x: the primitive.
@compute @workgroup_size(1)
fn line() {
    let p = job.a.x * PRIM_WORDS;
    let s = i32(words[p + 12u]);
    let flags = words[p + 1u];
    let ax = bitcast<i32>(words[p + 17u]);
    let ay = bitcast<i32>(words[p + 18u]);
    let bx = bitcast<i32>(words[p + 22u]);
    let by = bitcast<i32>(words[p + 23u]);
    let x1 = bitcast<i32>(words[p + 27u]);
    let y1 = bitcast<i32>(words[p + 28u]);
    let x2 = bitcast<i32>(words[p + 29u]);
    let y2 = bitcast<i32>(words[p + 30u]);
    let steps = max(abs(bx - ax), abs(by - ay));
    for (var i = 0; i <= steps; i++) {
        let x = lerp(ax, bx, i, steps);
        let y = lerp(ay, by, i, steps);
        var r = bitcast<i32>(words[p + 19u]);
        var g = bitcast<i32>(words[p + 20u]);
        var b = bitcast<i32>(words[p + 21u]);
        if ((flags & 2u) != 0u) {
            r = lerp(r, bitcast<i32>(words[p + 24u]), i, steps);
            g = lerp(g, bitcast<i32>(words[p + 25u]), i, steps);
            b = lerp(b, bitcast<i32>(words[p + 26u]), i, steps);
        }
        for (var dy = 0; dy < s; dy++) {
            for (var dx = 0; dx < s; dx++) {
                let px = x + dx;
                let py = y + dy;
                if (px >= x1 && px <= x2 && py >= y1 && py <= y2) {
                    pixel(p, px, py, r, g, b, 0, 0);
                }
            }
        }
    }
}

// As the software's: p + ((q - p) * i + steps / 2 * sign(q - p)) / steps.
fn lerp(p: i32, q: i32, i: i32, steps: i32) -> i32 {
    if (steps == 0) {
        return p;
    }
    return p + ((q - p) * i + steps / 2 * sign(q - p)) / steps;
}

// --- VRAM operations ---------------------------------------------------------

// The scaled pixel (bx, by) of the native pixel (nx, ny).
fn sub_index(nx: u32, ny: u32, bx: u32, by: u32) -> u32 {
    let s = scale_now();
    return (ny * s + by) * stride() + nx * s + bx;
}

// Native VRAM from the software GPU. a: x, y, w, h; b.x: data's start.
@compute @workgroup_size(8, 8)
fn upload(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= job.a.z || id.y >= job.a.w) {
        return;
    }
    let v = words[job.b.x + id.y * job.a.z + id.x];
    native[(job.a.y + id.y) * 1024u + job.a.x + id.x] = v;
}

// The scaled VRAM from the native one, a block per native pixel.
@compute @workgroup_size(8, 8)
fn rescale(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = scale_now();
    if (id.x >= 1024u * s || id.y >= 512u * s) {
        return;
    }
    hires[id.y * stride() + id.x] = native[(id.y / s) * 1024u + id.x / s];
}

// a: x, y, w, h (native, wrapping); b.x: colour.
@compute @workgroup_size(8, 8)
fn fill(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = scale_now();
    if (id.x >= job.a.z * s || id.y >= job.a.w * s) {
        return;
    }
    let nx = (job.a.x + id.x / s) & 0x3ffu;
    let ny = (job.a.y + id.y / s) & 0x1ffu;
    hires[sub_index(nx, ny, id.x % s, id.y % s)] = job.b.x;
}

// Whether pixel `i` of a copy is among those the native mask kept (the
// sorted list at c.x, c.y long).
fn copy_skipped(i: u32) -> bool {
    var lo = 0u;
    var hi = job.c.y;
    while (lo < hi) {
        let mid = (lo + hi) / 2u;
        let v = words[job.c.x + mid];
        if (v == i) {
            return true;
        }
        if (v < i) {
            lo = mid + 1u;
        } else {
            hi = mid;
        }
    }
    return false;
}

// One scaled pixel (bx, by) of pixel i of a copy. a: sx, sy, dx, dy;
// b: w, h, check mask, set mask.
fn copy_pixel(i: u32, bx: u32, by: u32) {
    let w = job.b.x;
    let sx = (job.a.x + i % w) & 0x3ffu;
    let sy = (job.a.y + i / w) & 0x1ffu;
    let dx = (job.a.z + i % w) & 0x3ffu;
    let dy = (job.a.w + i / w) & 0x1ffu;
    let to = sub_index(dx, dy, bx, by);
    if (job.b.z != 0u && (hires[to] & 0x8000u) != 0u) {
        return;
    }
    hires[to] = hires[sub_index(sx, sy, bx, by)] | select(0u, 0x8000u, job.b.w != 0u);
}

// A copy whose source and destination do not overlap: a thread per scaled
// pixel.
@compute @workgroup_size(8, 8)
fn copy(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = scale_now();
    if (id.x >= job.b.x * s || id.y >= job.b.y * s) {
        return;
    }
    let i = (id.y / s) * job.b.x + id.x / s;
    if (copy_skipped(i)) {
        return;
    }
    copy_pixel(i, id.x % s, id.y % s);
}

// Any copy, in one thread, in the software's order.
@compute @workgroup_size(1)
fn copy_in_order() {
    let s = scale_now();
    for (var i = 0u; i < job.b.x * job.b.y; i++) {
        if (copy_skipped(i)) {
            continue;
        }
        for (var by = 0u; by < s; by++) {
            for (var bx = 0u; bx < s; bx++) {
                copy_pixel(i, bx, by);
            }
        }
    }
}

// Pixels of a CPU to VRAM transfer. a: x, y, w (native, wrapping), first
// pixel; b: count, data's start, check mask. A thread per scaled pixel:
// x the transfer's pixel, y the block's.
@compute @workgroup_size(64)
fn write(@builtin(global_invocation_id) id: vec3<u32>) {
    let s = scale_now();
    if (id.x >= job.b.x) {
        return;
    }
    let v = words[job.b.y + id.x];
    if ((v & 0x10000u) != 0u) {
        return;
    }
    let i = job.a.w + id.x;
    let nx = (job.a.x + i % job.a.z) & 0x3ffu;
    let ny = (job.a.y + i / job.a.z) & 0x1ffu;
    for (var by = 0u; by < s; by++) {
        for (var bx = 0u; bx < s; bx++) {
            let to = sub_index(nx, ny, bx, by);
            if (job.b.z != 0u && (hires[to] & 0x8000u) != 0u) {
                continue;
            }
            hires[to] = v & 0xffffu;
        }
    }
}

// --- Display -----------------------------------------------------------------

fn expand(c: u32) -> u32 {
    let e = ((c & 0x1fu) << 3u) | ((c & 0x1fu) >> 2u);
    return e;
}

// The displayed picture, RGBA, into `screen`. a: x, y, w, h (output
// pixels); b.x: 24-bit (from the native VRAM, unscaled), b.y: off.
@compute @workgroup_size(8, 8)
fn display(@builtin(global_invocation_id) id: vec3<u32>) {
    let w = job.a.z;
    if (id.x >= w || id.y >= job.a.w) {
        return;
    }
    var rgba = 0xff000000u;
    if (job.b.y == 0u) {
        if (job.b.x != 0u) {
            let row = ((job.a.y + id.y) & 0x1ffu) * 1024u;
            let byte = job.a.x * 2u + id.x * 3u;
            var c = 0u;
            for (var k = 0u; k < 3u; k++) {
                let b = byte + k;
                let word = vram(row + ((b / 2u) & 0x3ffu));
                c |= ((word >> ((b & 1u) * 8u)) & 0xffu) << (8u * k);
            }
            rgba |= c;
        } else {
            let s = scale_now();
            let vy = (job.a.y * s + id.y) % (512u * s);
            let vx = (job.a.x * s + id.x) % stride();
            let c = hires[vy * stride() + vx];
            rgba |= expand(c) | (expand(c >> 5u) << 8u) | (expand(c >> 10u) << 16u);
        }
    }
    screen[id.y * w + id.x] = rgba;
}
