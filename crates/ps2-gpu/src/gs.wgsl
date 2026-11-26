// The GS on the GPU: its 4 MiB local memory, in its own layouts, and the
// kernels that write it: drawing batches of primitives (one thread per
// pixel, going through the batch's primitives in order), host transfers,
// local copies, CLUT loads, and reading the display out.
//
// The same rules as the software GS (crates/ps2/src/gs), which is the
// reference: see its draw.rs for the pixel pipeline.

const WORDS: u32 = 1048576u;

// Pixel formats.
const CT32: u32 = 0x00u;
const CT24: u32 = 0x01u;
const CT16: u32 = 0x02u;
const CT16S: u32 = 0x0Au;
const T8: u32 = 0x13u;
const T4: u32 = 0x14u;
const T8H: u32 = 0x1Bu;
const T4HL: u32 = 0x24u;
const T4HH: u32 = 0x2Cu;
const Z32: u32 = 0x30u;
const Z24: u32 = 0x31u;
const Z16: u32 = 0x32u;
const Z16S: u32 = 0x3Au;

@group(0) @binding(0) var<storage, read_write> vram: array<atomic<u32>>;
@group(0) @binding(1) var<storage, read> prims: array<u32>;
@group(0) @binding(2) var<uniform> job: Job;
@group(0) @binding(3) var<storage, read_write> cluts: array<u32>;
@group(0) @binding(4) var<storage, read> data: array<u32>;
@group(0) @binding(5) var<storage, read_write> screen: array<u32>;

struct Job {
    // Draw: first primitive, count, origin of the dispatch's area.
    // Pixels: offset in `data`, count, rectangle.
    a: vec4<u32>,
    b: vec4<u32>,
    c: vec4<u32>,
    d: vec4<u32>,
}

// --- Memory layouts (as in mem.rs, as bit operations) ----------------------

fn block32(x: u32, y: u32) -> u32 {
    let bx = (x >> 3u) & 7u;
    let by = (y >> 3u) & 3u;
    return (bx & 1u) | ((by & 1u) << 1u) | ((bx & 2u) << 1u) | ((by & 2u) << 2u) | ((bx & 4u) << 2u);
}

fn column32(x: u32, y: u32) -> u32 {
    return (x & 1u) | ((y & 1u) << 1u) | ((x & 6u) << 1u) | ((y & 6u) << 3u);
}

fn addr32(bp: u32, bw: u32, x: u32, y: u32, z: bool) -> u32 {
    let page = (x >> 6u) + (y >> 5u) * max(bw, 1u);
    var block = block32(x, y);
    if (z) { block ^= 24u; }
    return (bp * 64u + page * 2048u + block * 64u + column32(x, y)) % WORDS;
}

fn addr16(bp: u32, bw: u32, x: u32, y: u32, psm: u32) -> u32 {
    let page = (x >> 6u) + (y >> 6u) * max(bw, 1u);
    let bx = (x >> 4u) & 3u;
    let by = (y >> 3u) & 7u;
    var block: u32;
    if (psm == CT16S || psm == Z16S) {
        block = (by & 1u) | ((bx & 1u) << 1u) | (by & 4u) | ((by & 2u) << 2u) | ((bx & 2u) << 3u);
    } else {
        block = (by & 1u) | ((bx & 1u) << 1u) | ((by & 2u) << 1u) | ((bx & 2u) << 2u) | ((by & 4u) << 2u);
    }
    if (psm == Z16 || psm == Z16S) { block ^= 24u; }
    let column = ((x & 1u) << 1u) | ((x & 6u) << 2u) | ((x & 8u) >> 3u) | ((y & 1u) << 2u) | ((y & 6u) << 4u);
    return (bp * 128u + page * 4096u + block * 128u + column) % (WORDS * 2u);
}

fn addr8(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x >> 7u) + (y >> 6u) * max(bw >> 1u, 1u);
    let block = block32(x >> 1u, y >> 1u);
    let flip = ((x >> 2u) & 1u) ^ ((y >> 1u) & 1u) ^ ((y >> 2u) & 1u);
    let column = ((x & 1u) << 2u) | ((x & 2u) << 3u) | (flip << 5u) | ((x & 8u) >> 2u) | ((y >> 1u) & 1u)
        | ((y & 1u) << 3u) | ((y & 12u) << 4u);
    return (bp * 256u + page * 8192u + block * 256u + column) % (WORDS * 4u);
}

fn addr4(bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    let page = (x >> 7u) + (y >> 7u) * max(bw >> 1u, 1u);
    // BLOCK16's layout, on 32×16 blocks.
    let bx = (x >> 5u) & 3u;
    let by = (y >> 4u) & 7u;
    let block = (by & 1u) | ((bx & 1u) << 1u) | ((by & 2u) << 1u) | ((bx & 2u) << 2u) | ((by & 4u) << 2u);
    let flip = ((x >> 2u) & 1u) ^ ((y >> 1u) & 1u) ^ ((y >> 2u) & 1u);
    let column = ((x & 1u) << 3u) | ((x & 2u) << 4u) | (flip << 6u) | ((x & 24u) >> 2u) | ((y >> 1u) & 1u)
        | ((y & 1u) << 4u) | ((y & 12u) << 5u);
    return (bp * 512u + page * 16384u + block * 512u + column) % (WORDS * 8u);
}

fn bits_of(psm: u32) -> u32 {
    switch psm {
        case 0x02u, 0x0Au, 0x32u, 0x3Au: { return 16u; }
        case 0x13u, 0x1Bu: { return 8u; }
        case 0x14u, 0x24u, 0x2Cu: { return 4u; }
        default: { return 32u; }
    }
}

fn mem_read(psm: u32, bp: u32, bw: u32, x: u32, y: u32) -> u32 {
    // One load after the switch: a driver (Honeykrisp) miscompiled
    // a load in each case.
    var word = 0u;
    var shift = 0u;
    var mask = 0xFFFFFFFFu;
    switch psm {
        case 0x00u, 0x30u: { word = addr32(bp, bw, x, y, psm == Z32); }
        case 0x01u, 0x31u: {
            word = addr32(bp, bw, x, y, psm == Z24);
            mask = 0xFFFFFFu;
        }
        case 0x02u, 0x0Au, 0x32u, 0x3Au: {
            let a = addr16(bp, bw, x, y, psm);
            word = a >> 1u;
            shift = (a & 1u) * 16u;
            mask = 0xFFFFu;
        }
        case 0x13u: {
            let a = addr8(bp, bw, x, y);
            word = a >> 2u;
            shift = (a & 3u) * 8u;
            mask = 0xFFu;
        }
        case 0x14u: {
            let a = addr4(bp, bw, x, y);
            word = a >> 3u;
            shift = (a & 7u) * 4u;
            mask = 0xFu;
        }
        case 0x1Bu: {
            word = addr32(bp, bw, x, y, false);
            shift = 24u;
            mask = 0xFFu;
        }
        case 0x24u: {
            word = addr32(bp, bw, x, y, false);
            shift = 24u;
            mask = 0xFu;
        }
        case 0x2Cu: {
            word = addr32(bp, bw, x, y, false);
            shift = 28u;
            mask = 0xFu;
        }
        default: { mask = 0u; }
    }
    return (atomicLoad(&vram[word]) >> shift) & mask;
}

/// Writes `bits` of a word (the others are other pixels').
fn store_bits(word: u32, shift: u32, mask: u32, v: u32) {
    atomicAnd(&vram[word], ~(mask << shift));
    atomicOr(&vram[word], (v & mask) << shift);
}

fn mem_write(psm: u32, bp: u32, bw: u32, x: u32, y: u32, v: u32) {
    switch psm {
        case 0x00u, 0x30u: { atomicStore(&vram[addr32(bp, bw, x, y, psm == Z32)], v); }
        case 0x01u, 0x31u: { store_bits(addr32(bp, bw, x, y, psm == Z24), 0u, 0xFFFFFFu, v); }
        case 0x02u, 0x0Au, 0x32u, 0x3Au: {
            let a = addr16(bp, bw, x, y, psm);
            store_bits(a >> 1u, (a & 1u) * 16u, 0xFFFFu, v);
        }
        case 0x13u: {
            let a = addr8(bp, bw, x, y);
            store_bits(a >> 2u, (a & 3u) * 8u, 0xFFu, v);
        }
        case 0x14u: {
            let a = addr4(bp, bw, x, y);
            store_bits(a >> 3u, (a & 7u) * 4u, 0xFu, v);
        }
        case 0x1Bu: { store_bits(addr32(bp, bw, x, y, false), 24u, 0xFFu, v); }
        case 0x24u: { store_bits(addr32(bp, bw, x, y, false), 24u, 0xFu, v); }
        case 0x2Cu: { store_bits(addr32(bp, bw, x, y, false), 28u, 0xFu, v); }
        default: {}
    }
}

fn expand16(c: u32) -> u32 {
    let r = (c & 0x1Fu) << 3u;
    let g = ((c >> 5u) & 0x1Fu) << 3u;
    let b = ((c >> 10u) & 0x1Fu) << 3u;
    let a = select(0u, 0x80u, (c & 0x8000u) != 0u);
    return r | (g << 8u) | (b << 16u) | (a << 24u);
}

fn field(v: u32, lo: u32, n: u32) -> u32 {
    return (v >> lo) & ((1u << n) - 1u);
}

// --- Primitives ---------------------------------------------------------------
//
// A primitive is PRIM_WORDS words (see prims.rs, which writes them):
//   0      kind (0 rectangle, 1 triangle, 2 line)
//   1-4    bounding box: x0, y0, x1, y1 (end excluded)
//   5-13   triangles: edge values at (x0, y0), steps per pixel and per row
//          lines: x0, y0, x1, y1 (i32, 1/16 pixels), steps (i32)
//   14-46  attribute planes, 11 × (value at (x0, y0), per pixel, per row):
//          z, r, g, b, a, s, t, q, u, v, fog
//   47     flat color
//   48-63  registers (see REG_*)

const PRIM_WORDS: u32 = 64u;
const REG_PRIM: u32 = 48u;
const REG_FRAME: u32 = 49u;
const REG_FBMSK: u32 = 50u;
const REG_ZBUF: u32 = 51u;
const REG_TEST: u32 = 52u;
const REG_ALPHA: u32 = 53u;
const REG_FIX: u32 = 54u;
const REG_TEX0: u32 = 55u;
const REG_TEX0_HI: u32 = 56u;
const REG_CLAMP: u32 = 57u;
const REG_CLAMP_HI: u32 = 58u;
const REG_TEXA: u32 = 59u;
const REG_TA1: u32 = 60u;
const REG_FOGCOL: u32 = 61u;
const REG_FLAGS: u32 = 62u;  // bit 0: FBA, bit 1: COLCLAMP; TEX1: 8 LCM, 9-10 L, 11 MMAG, 12-14 MMIN, 16-27 K
const REG_CLUT: u32 = 63u;   // the CLUT slot

fn pw(p: u32, i: u32) -> u32 { return prims[p * PRIM_WORDS + i]; }
fn pf(p: u32, i: u32) -> f32 { return bitcast<f32>(prims[p * PRIM_WORDS + i]); }
fn pi(p: u32, i: u32) -> i32 { return bitcast<i32>(prims[p * PRIM_WORDS + i]); }

/// Attribute `k` at pixel offset (i, j) from the bounding box's corner.
fn plane(p: u32, k: u32, i: f32, j: f32) -> f32 {
    let at = 14u + k * 3u;
    return pf(p, at) + pf(p, at + 1u) * i + pf(p, at + 2u) * j;
}

/// floor((a * steps + (b - a) * n) / (16 * steps)): a line's pixel n.
fn line_at(a: i32, b: i32, n: i32, steps: i32) -> i32 {
    let num = a * steps + (b - a) * n;
    let den = 16 * steps;
    // Floor division (WGSL's rounds towards zero).
    return select((num - den + 1) / den, num / den, num >= 0);
}

/// Whether primitive `p` covers pixel (x, y); for lines, the position
/// along them (in the planes' i), else -1.
fn cover(p: u32, x: i32, y: i32) -> vec2<f32> {
    let kind = pw(p, 0u);
    let i = x - pi(p, 1u);
    let j = y - pi(p, 2u);
    if (kind == 0u) {
        return vec2<f32>(1.0, 0.0);
    }
    if (kind == 1u) {
        for (var k = 0u; k < 3u; k++) {
            let w = pi(p, 5u + k) + pi(p, 8u + k) * i + pi(p, 11u + k) * j;
            if (w < 0) { return vec2<f32>(0.0, 0.0); }
        }
        return vec2<f32>(1.0, 0.0);
    }
    // A line: its pixels are floor(a + (b - a) * n / steps), with the ends
    // in 1/16 pixels (exactly, in integers), and each column (or row, along
    // its major axis) has at most one.
    let ax = pi(p, 5u);
    let ay = pi(p, 6u);
    let bx = pi(p, 7u);
    let by = pi(p, 8u);
    let steps = max(pi(p, 9u), 1);
    let major_x = abs(bx - ax) >= abs(by - ay);
    let c = select(f32(y * 16 + 8 - ay), f32(x * 16 + 8 - ax), major_x);
    let span = f32(select(by - ay, bx - ax, major_x));
    let guess = i32(floor(c / span * f32(steps)));
    for (var n = guess - 1; n <= guess + 1; n++) {
        if (n < 0 || n >= steps) { continue; }
        if (line_at(ax, bx, n, steps) == x && line_at(ay, by, n, steps) == y) {
            return vec2<f32>(1.0, f32(n) / f32(steps));
        }
    }
    return vec2<f32>(0.0, 0.0);
}

// --- Textures ------------------------------------------------------------------

fn wrap(mode: u32, c: i32, size: i32, mn: i32, mx: i32) -> i32 {
    switch mode {
        case 0u: { return c & (size - 1); }
        case 1u: { return clamp(c, 0, size - 1); }
        case 2u: { return clamp(c, mn, mx); }
        default: { return (c & mn) | mx; }
    }
}

fn expand16_texa(p: u32, raw: u32) -> u32 {
    let rgb = expand16(raw) & 0xFFFFFFu;
    let texa = pw(p, REG_TEXA);
    var a: u32;
    if ((raw & 0x8000u) != 0u) {
        a = pw(p, REG_TA1) & 0xFFu;
    } else if (((texa >> 15u) & 1u) != 0u && (raw & 0x7FFFu) == 0u) {
        a = 0u;
    } else {
        a = texa & 0xFFu;
    }
    return rgb | (a << 24u);
}

fn texel(p: u32, uu: i32, vv: i32) -> vec4<u32> {
    let t0 = pw(p, REG_TEX0);
    let t1 = pw(p, REG_TEX0_HI);
    let cl = pw(p, REG_CLAMP);
    let ch = pw(p, REG_CLAMP_HI);
    let tbp = field(t0, 0u, 14u);
    let tbw = field(t0, 14u, 6u);
    let psm = field(t0, 20u, 6u);
    let w = 1 << min(field(t0, 26u, 4u), 10u);
    let h = 1 << min(field(t0, 30u, 2u) | (field(t1, 0u, 2u) << 2u), 10u);
    let minu = i32(field(cl, 4u, 10u));
    let maxu = i32(field(cl, 14u, 10u));
    let minv = i32(field(cl, 24u, 8u) | (field(ch, 0u, 2u) << 8u));
    let maxv = i32(field(ch, 2u, 10u));
    let u = wrap(field(cl, 0u, 2u), uu, i32(w), minu, maxu);
    let v = wrap(field(cl, 2u, 2u), vv, i32(h), minv, maxv);
    let raw = mem_read(psm, tbp, tbw, u32(u), u32(v));
    let texa = pw(p, REG_TEXA);
    var c: u32;
    switch psm {
        case 0x00u: { c = raw; }
        case 0x01u: {
            var a = texa & 0xFFu;
            if (((texa >> 15u) & 1u) != 0u && (raw & 0xFFFFFFu) == 0u) { a = 0u; }
            c = (raw & 0xFFFFFFu) | (a << 24u);
        }
        case 0x02u, 0x0Au: { c = expand16_texa(p, raw); }
        default: {
            let cpsm = field(t1, 19u, 4u);
            let csa = field(t1, 24u, 5u);
            var index = raw & 255u;
            if (bits_of(psm) == 4u) { index = (csa * 16u + raw) & 255u; }
            let entry = cluts[pw(p, REG_CLUT) * 256u + index];
            if (cpsm == CT32 || cpsm == CT24) { c = entry; } else { c = expand16_texa(p, entry); }
        }
    }
    return vec4<u32>(c & 0xFFu, (c >> 8u) & 0xFFu, (c >> 16u) & 0xFFu, c >> 24u);
}

// Whether a texel at that Q is filtered (bilinear), as TEX1 says: the
// level of detail decides between the magnifying and the minifying filter.
fn linear(p: u32, q: f32) -> bool {
    let f = pw(p, REG_FLAGS);
    let k = f32(i32(((f >> 16u) & 0xFFFu) << 20u) >> 20u) / 16.0;
    var lod = k;
    if (((f >> 8u) & 1u) == 0u) {
        lod = log2(1.0 / abs(q)) * f32(1u << ((f >> 9u) & 3u)) + k;
    }
    if (lod <= 0.0) { return ((f >> 11u) & 1u) != 0u; }
    let mmin = (f >> 12u) & 7u;
    return mmin == 1u || mmin == 4u || mmin == 5u;
}

// The texel at (u, v), filtered or not (draw.rs's sample).
fn sample(p: u32, tc: vec3<f32>) -> vec4<u32> {
    if (!linear(p, tc.z)) {
        return texel(p, i32(floor(tc.x)), i32(floor(tc.y)));
    }
    // The four texels around (u - 0.5, v - 0.5), weighted in sixteenths;
    // a hair under a sixteenth counts as on it (draw.rs's SIXTEENTH_SLACK).
    let fu = i32(floor((tc.x - 0.5) * 16.0 + 0.015625));
    let fv = i32(floor((tc.y - 0.5) * 16.0 + 0.015625));
    let u0 = fu >> 4u;
    let v0 = fv >> 4u;
    let a = u32(fu & 15);
    let b = u32(fv & 15);
    let t00 = texel(p, u0, v0);
    let t10 = texel(p, u0 + 1, v0);
    let t01 = texel(p, u0, v0 + 1);
    let t11 = texel(p, u0 + 1, v0 + 1);
    let top = t00 * (16u - a) + t10 * a;
    let bottom = t01 * (16u - a) + t11 * a;
    return (top * (16u - b) + bottom * b) >> vec4<u32>(8u);
}

// --- The pixel pipeline (draw.rs's fragment) ------------------------------------

fn shade(p: u32, x: u32, y: u32, z_in: u32, color: vec4<u32>, tc: vec3<f32>, fog_in: u32) {
    let prim = pw(p, REG_PRIM);
    let fge = ((prim >> 5u) & 1u) != 0u;
    let abe = ((prim >> 6u) & 1u) != 0u;
    let tme = ((prim >> 4u) & 1u) != 0u;
    var r = color.x;
    var g = color.y;
    var b = color.z;
    var a = color.w;
    if (tme) {
        let t = sample(p, tc);
        let t1 = pw(p, REG_TEX0_HI);
        let tcc = ((t1 >> 2u) & 1u) != 0u;
        let tfx = (t1 >> 3u) & 3u;
        let mr = min((t.x * r) >> 7u, 255u);
        let mg = min((t.y * g) >> 7u, 255u);
        let mb = min((t.z * b) >> 7u, 255u);
        switch tfx {
            case 0u: {
                if (tcc) { a = min((t.w * a) >> 7u, 255u); }
                r = mr; g = mg; b = mb;
            }
            case 1u: {
                r = t.x; g = t.y; b = t.z;
                if (tcc) { a = t.w; }
            }
            case 2u: {
                r = min(mr + a, 255u); g = min(mg + a, 255u); b = min(mb + a, 255u);
                if (tcc) { a = min(t.w + a, 255u); }
            }
            default: {
                r = min(mr + a, 255u); g = min(mg + a, 255u); b = min(mb + a, 255u);
                if (tcc) { a = t.w; }
            }
        }
    }
    if (fge) {
        let f = min(fog_in, 255u);
        let fc = pw(p, REG_FOGCOL);
        r = (f * r + (255u - f) * (fc & 0xFFu)) >> 8u;
        g = (f * g + (255u - f) * ((fc >> 8u) & 0xFFu)) >> 8u;
        b = (f * b + (255u - f) * ((fc >> 16u) & 0xFFu)) >> 8u;
    }
    let test = pw(p, REG_TEST);
    let zbuf = pw(p, REG_ZBUF);
    let zmsk = ((zbuf >> 31u) & 1u) != 0u;
    var write_fb = true;
    var write_z = !zmsk;
    var write_alpha = true;
    if ((test & 1u) != 0u) {
        let aref = field(test, 4u, 8u);
        var passed: bool;
        switch field(test, 1u, 3u) {
            case 0u: { passed = false; }
            case 1u: { passed = true; }
            case 2u: { passed = a < aref; }
            case 3u: { passed = a <= aref; }
            case 4u: { passed = a == aref; }
            case 5u: { passed = a >= aref; }
            case 6u: { passed = a > aref; }
            default: { passed = a != aref; }
        }
        if (!passed) {
            switch field(test, 12u, 2u) {
                case 0u: { return; }
                case 1u: { write_z = false; }
                case 2u: { write_fb = false; }
                default: { write_z = false; write_alpha = false; }
            }
        }
    }
    let frame = pw(p, REG_FRAME);
    let fbp = field(frame, 0u, 9u) * 32u;
    let fbw = field(frame, 16u, 6u);
    let fpsm = field(frame, 24u, 6u);
    let zbp = field(zbuf, 0u, 9u) * 32u;
    let zpsm = field(zbuf, 24u, 4u) | 0x30u;
    var zmax = 0xFFFFFFFFu;
    if (zpsm == Z24) { zmax = 0xFFFFFFu; }
    if (zpsm == Z16 || zpsm == Z16S) { zmax = 0xFFFFu; }
    let z = min(z_in, zmax);
    let zte = ((test >> 16u) & 1u) != 0u;
    if (zte) {
        let ztst = field(test, 17u, 2u);
        if (ztst == 0u) { return; }
        if (ztst >= 2u) {
            let old = mem_read(zpsm, zbp, fbw, x, y);
            if (ztst == 2u && z < old) { return; }
            if (ztst == 3u && z <= old) { return; }
        }
    }
    let fbmsk = pw(p, REG_FBMSK);
    let date = ((test >> 14u) & 1u) != 0u;
    var old_raw = 0u;
    var old = 0u;
    if (abe || date || fbmsk != 0u || !write_alpha) {
        old_raw = mem_read(fpsm, fbp, fbw, x, y);
        if (fpsm == CT16 || fpsm == CT16S) {
            old = expand16(old_raw);
        } else if (fpsm == CT24) {
            old = old_raw | 0x80000000u;
        } else {
            old = old_raw;
        }
    }
    if (date) {
        var bit: bool;
        if (fpsm == CT16 || fpsm == CT16S) { bit = (old_raw & 0x8000u) != 0u; } else { bit = (old_raw & 0x80000000u) != 0u; }
        let datm = ((test >> 15u) & 1u) != 0u;
        if (bit != datm) { return; }
    }
    let flags = pw(p, REG_FLAGS);
    if (abe) {
        let alpha = pw(p, REG_ALPHA);
        let src = vec3<i32>(i32(r), i32(g), i32(b));
        let dst = vec3<i32>(i32(old & 0xFFu), i32((old >> 8u) & 0xFFu), i32((old >> 16u) & 0xFFu));
        let ad = old >> 24u;
        var cf: i32;
        switch field(alpha, 4u, 2u) {
            case 0u: { cf = i32(a); }
            case 1u: { cf = i32(ad); }
            default: { cf = i32(pw(p, REG_FIX)); }
        }
        var sa = vec3<i32>(0);
        var sb = vec3<i32>(0);
        var sd = vec3<i32>(0);
        switch field(alpha, 0u, 2u) { case 0u: { sa = src; } case 1u: { sa = dst; } default: {} }
        switch field(alpha, 2u, 2u) { case 0u: { sb = src; } case 1u: { sb = dst; } default: {} }
        switch field(alpha, 6u, 2u) { case 0u: { sd = src; } case 1u: { sd = dst; } default: {} }
        let o = (((sa - sb) * cf) >> vec3<u32>(7u)) + sd;
        if ((flags & 2u) != 0u) {
            let c = clamp(o, vec3<i32>(0), vec3<i32>(255));
            r = u32(c.x); g = u32(c.y); b = u32(c.z);
        } else {
            r = u32(o.x) & 0xFFu; g = u32(o.y) & 0xFFu; b = u32(o.z) & 0xFFu;
        }
    }
    if ((flags & 1u) != 0u) { a |= 0x80u; }
    if (write_fb) {
        var nw = r | (g << 8u) | (b << 16u) | (min(a, 255u) << 24u);
        var mask = fbmsk;
        if (!write_alpha) { mask |= 0xFF000000u; }
        if (mask != 0u) { nw = (old & mask) | (nw & ~mask); }
        var raw = nw;
        if (fpsm == CT16 || fpsm == CT16S) {
            raw = ((nw & 0xFFu) >> 3u) | ((((nw >> 8u) & 0xFFu) >> 3u) << 5u)
                | ((((nw >> 16u) & 0xFFu) >> 3u) << 10u) | ((nw >> 31u) << 15u);
        }
        mem_write(fpsm, fbp, fbw, x, y, raw);
    }
    if (write_z && zte) {
        mem_write(zpsm, zbp, fbw, x, y, z);
    }
}

// --- Kernels ------------------------------------------------------------------------

// A batch: job.a = (first primitive, count, x0, y0) of the dispatch's area.
@compute @workgroup_size(8, 8)
fn draw(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = i32(job.a.z + id.x);
    let y = i32(job.a.w + id.y);
    for (var n = 0u; n < job.a.y; n++) {
        let p = job.a.x + n;
        if (x < pi(p, 1u) || y < pi(p, 2u) || x >= pi(p, 3u) || y >= pi(p, 4u)) { continue; }
        let c = cover(p, x, y);
        if (c.x == 0.0) { continue; }
        // Rectangles and triangles: planes by pixel; lines: by position.
        var i = f32(x - pi(p, 1u));
        var j = f32(y - pi(p, 2u));
        if (pw(p, 0u) == 2u) { i = c.y; j = 0.0; }
        let z = u32(clamp(plane(p, 0u, i, j), 0.0, 4294967040.0));
        var color: vec4<u32>;
        if ((pw(p, REG_PRIM) & 8u) != 0u) {
            color = vec4<u32>(
                u32(clamp(plane(p, 1u, i, j), 0.0, 255.0)),
                u32(clamp(plane(p, 2u, i, j), 0.0, 255.0)),
                u32(clamp(plane(p, 3u, i, j), 0.0, 255.0)),
                u32(clamp(plane(p, 4u, i, j), 0.0, 255.0)),
            );
        } else {
            let f = pw(p, 47u);
            color = vec4<u32>(f & 0xFFu, (f >> 8u) & 0xFFu, (f >> 16u) & 0xFFu, f >> 24u);
        }
        var tc = vec3<f32>(0.0, 0.0, 1.0);
        if ((pw(p, REG_PRIM) & 16u) != 0u) {
            if ((pw(p, REG_PRIM) & 0x100u) != 0u) {
                tc = vec3<f32>(plane(p, 8u, i, j) / 16.0, plane(p, 9u, i, j) / 16.0, 1.0);
            } else {
                var q = plane(p, 7u, i, j);
                if (q == 0.0) { q = 1.0; }
                let t0 = pw(p, REG_TEX0);
                let t1 = pw(p, REG_TEX0_HI);
                let w = f32(1u << min(field(t0, 26u, 4u), 10u));
                let h = f32(1u << min(field(t0, 30u, 2u) | (field(t1, 0u, 2u) << 2u), 10u));
                tc = vec3<f32>(plane(p, 5u, i, j) / q * w, plane(p, 6u, i, j) / q * h, q);
            }
        }
        let fog = u32(max(plane(p, 10u, i, j), 0.0));
        shade(p, u32(x), u32(y), z, color, tc, fog);
    }
}

// Host to local transfer: job.a = (data offset, count, first pixel, w),
// job.b = (bp, bw, psm, 0), job.c = (x, y, 0, 0).
@compute @workgroup_size(64)
fn pixels(@builtin(global_invocation_id) id: vec3<u32>) {
    let n = id.x + id.y * 65535u * 64u;
    if (n >= job.a.y) { return; }
    let index = job.a.z + n;
    let w = max(job.a.w, 1u);
    let x = (job.c.x + index % w) % 2048u;
    let y = (job.c.y + index / w) % 2048u;
    mem_write(job.b.z, job.b.x, job.b.y, x, y, data[job.a.x + n]);
}

// Local to local: job.a = (sbp, sbw, spsm, w), job.b = (dbp, dbw, dpsm, h),
// job.c = (sx, sy, dx, dy).
@compute @workgroup_size(8, 8)
fn copy(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= job.a.w || id.y >= job.b.w) { return; }
    let v = mem_read(job.a.z, job.a.x, job.a.y, (job.c.x + id.x) % 2048u, (job.c.y + id.y) % 2048u);
    mem_write(job.b.z, job.b.x, job.b.y, (job.c.z + id.x) % 2048u, (job.c.w + id.y) % 2048u, v);
}

// A CLUT load into slot job.a.x, from slot job.a.y's contents:
// job.b = (TEX0 low, high, 0, 0).
@compute @workgroup_size(64)
fn clut(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= 256u) { return; }
    let slot = job.a.x;
    let prev = job.a.y;
    let t0 = job.b.x;
    let t1 = job.b.y;
    let psm = field(t0, 20u, 6u);
    let cbp = field(t1, 5u, 14u);
    let cpsm = field(t1, 19u, 4u);
    let csa = field(t1, 24u, 5u);
    var src = CT32;
    if (cpsm != CT32 && cpsm != CT24) { src = cpsm; }
    var v = cluts[prev * 256u + i];
    if (bits_of(psm) == 8u) {
        // 16×16 entries; bits 3 and 4 of the index swapped.
        let j = (i & 0xE7u) | ((i & 0x08u) << 1u) | ((i & 0x10u) >> 1u);
        v = mem_read(src, cbp, 1u, j & 15u, j >> 4u);
    } else {
        // 8×2 entries, at the CSA offset.
        let k = (i + 256u - csa * 16u) & 255u;
        if (k < 16u) { v = mem_read(src, cbp, 1u, k & 7u, k >> 3u); }
    }
    cluts[slot * 256u + i] = v;
}

// The display: job.a = (fbp, fbw, psm, w), job.b = (x, y, h, 0); RGBA out.
@compute @workgroup_size(8, 8)
fn display(@builtin(global_invocation_id) id: vec3<u32>) {
    let w = job.a.w;
    let h = job.b.z;
    if (id.x >= w || id.y >= h) { return; }
    let c = mem_read(job.a.z, job.a.x, job.a.y, job.b.x + id.x, job.b.y + id.y);
    var rgba = c;
    if (job.a.z == CT16 || job.a.z == CT16S) { rgba = expand16(c); }
    screen[id.y * w + id.x] = (rgba & 0xFFFFFFu) | 0xFF000000u;
}
