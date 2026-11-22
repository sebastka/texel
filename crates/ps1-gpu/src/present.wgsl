// Shows the displayed picture (the display kernel's output: RGBA, a u32 a
// pixel) on a surface, scaled to fill it: nearest or bilinear; and in
// netplay's split screen, the other player's half hidden.

struct View {
    // The picture's size, and whether to filter it (bilinear).
    size: vec2<u32>,
    bilinear: u32,
    // The half hidden: 0 none, 1 top, 2 bottom, 3 left, 4 right.
    hidden: u32,
}

@group(0) @binding(0) var<storage, read> screen: array<u32>;
@group(0) @binding(1) var<uniform> view: View;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the whole surface.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    var out: Out;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

fn pixel(x: i32, y: i32) -> vec4<f32> {
    let w = i32(view.size.x);
    let h = i32(view.size.y);
    let c = screen[u32(clamp(y, 0, h - 1) * w + clamp(x, 0, w - 1))];
    return vec4<f32>(unpack4x8unorm(c).rgb, 1.0);
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    let hidden = (view.hidden == 1u && in.uv.y < 0.5) || (view.hidden == 2u && in.uv.y >= 0.5)
        || (view.hidden == 3u && in.uv.x < 0.5) || (view.hidden == 4u && in.uv.x >= 0.5);
    if (hidden) {
        return vec4<f32>(0x16, 0x18, 0x1d, 0xff) / 255.0;
    }
    let p = in.uv * vec2<f32>(view.size);
    if (view.bilinear == 0u) {
        return pixel(i32(floor(p.x)), i32(floor(p.y)));
    }
    let q = p - 0.5;
    let i = floor(q);
    let f = q - i;
    let x = i32(i.x);
    let y = i32(i.y);
    let top = mix(pixel(x, y), pixel(x + 1, y), f.x);
    let bottom = mix(pixel(x, y + 1), pixel(x + 1, y + 1), f.x);
    return mix(top, bottom, f.y);
}
