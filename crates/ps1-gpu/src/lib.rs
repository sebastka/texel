//! The PlayStation's GPU at higher internal resolutions, on the GPU with
//! wgpu: Vulkan, Metal or DX12 natively, WebGPU in browsers.
//!
//! The software GPU (texel-ps1) still draws the native VRAM, exactly, as
//! games read it back; with recording on, it records what changes the
//! scaled VRAM instead of drawing it: primitives set up as it would draw
//! them, fills, copies and transfers, and the native VRAM textures are read
//! from when it changed. Compute kernels replay them with its integer
//! arithmetic, so both draw the same pixels (the tests compare them).
//! Triangles and rectangles are batched: each batch is one dispatch over
//! the area it covers, where every thread owns a pixel and goes through
//! the batch's primitives in order. Lines, whose scaled blocks overlap, and
//! copies whose source and destination overlap, run in order, in one
//! thread.

/// For surface targets (a browser's canvas, say).
pub use wgpu;

use texel_ps1::gpu_record::{Command, Display, PixelMode, Spans};
use wgpu::util::DeviceExt;

/// The native VRAM's size, and the largest scale.
const VRAM_WIDTH: u32 = 1024;
const VRAM_HEIGHT: u32 = 512;
const MAX_SCALE: u32 = 4;
/// Words per primitive (see the layout in `encode`).
const PRIM_WORDS: usize = 64;
/// Primitives per batch, at most.
const BATCH: usize = 64;
/// Uniform slots are 256 bytes apart (WebGPU's minimum offset alignment).
const JOB_STRIDE: u64 = 256;

#[derive(Clone, Copy)]
enum Kernel {
    Draw,
    Line,
    Upload,
    Rescale,
    Fill,
    Copy,
    CopyInOrder,
    Write,
}

struct Dispatch {
    kernel: Kernel,
    job: [u32; 16],
    groups: (u32, u32),
}

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    display: wgpu::ComputePipeline,
    hires: wgpu::Buffer,
    native: wgpu::Buffer,
    screen: wgpu::Buffer,
    /// The scale the commands are at (from their last `Rescale`).
    scale: u32,
    /// Batches and primitives drawn, for diagnostics.
    pub batches: u64,
    pub primitives: u64,
    /// The adapter's name and backend (empty in browsers).
    pub adapter: String,
    output: Option<Output>,
}

impl GpuRenderer {
    /// Sets up on the best adapter of this machine (native).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_blocking() -> Result<Self, String> {
        pollster::block_on(Self::new())
    }

    /// Sets up on the best adapter, without a surface: pictures are read
    /// back (`display`).
    pub async fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        Self::setup(&instance, None).await
    }

    /// Sets up to show pictures on `target` (a canvas, in browsers): see
    /// `present`.
    pub async fn with_surface(target: impl Into<wgpu::SurfaceTarget<'static>>) -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(target)
            .map_err(|e| format!("no GPU surface: {e}"))?;
        Self::setup(&instance, Some(surface)).await
    }

    async fn setup(instance: &wgpu::Instance, surface: Option<wgpu::Surface<'static>>) -> Result<Self, String> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: surface.as_ref(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no GPU adapter: {e}"))?;
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ps1-gpu"),
                required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no GPU device: {e}"))?;
        let name = if info.name.is_empty() {
            String::new()
        } else {
            format!("{} ({:?})", info.name, info.backend)
        };
        let mut r = Self::with_device(device, queue, name);
        if let Some(surface) = surface {
            r.output = Some(Output::new(&r.device, surface, &adapter));
        }
        Ok(r)
    }

    fn with_device(device: wgpu::Device, queue: wgpu::Queue, adapter: String) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu.wgsl").into()),
        });
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ps1-gpu"),
            entries: &[
                storage(0, false),
                storage(1, false),
                storage(2, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(64),
                    },
                    count: None,
                },
                storage(4, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ps1-gpu"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let buffer = |label: &str, words: u32| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: u64::from(words) * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        // In `Kernel` order.
        let pipelines = [
            "draw",
            "line",
            "upload",
            "rescale",
            "fill",
            "copy",
            "copy_in_order",
            "write",
        ]
        .map(pipeline)
        .into();
        Self {
            pipelines,
            display: pipeline("display"),
            hires: buffer("hires", VRAM_WIDTH * VRAM_HEIGHT * MAX_SCALE * MAX_SCALE),
            native: buffer("native", VRAM_WIDTH * VRAM_HEIGHT),
            // The largest picture: 640 by 512, scaled.
            screen: buffer("screen", 640 * 512 * MAX_SCALE * MAX_SCALE),
            layout,
            device,
            queue,
            scale: 1,
            batches: 0,
            primitives: 0,
            adapter,
            output: None,
        }
    }

    /// The scale drawn at (from the commands).
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Runs commands the software GPU recorded, in order.
    pub fn submit(&mut self, commands: &[Command]) {
        let mut words: Vec<u32> = Vec::new();
        let mut dispatches: Vec<Dispatch> = Vec::new();
        let mut batch = Batch::default();
        for command in commands {
            let s = self.scale;
            match command {
                Command::Triangle { pixel, spans } => {
                    let bbox = [spans.xs, spans.ys, spans.xe, spans.ye];
                    if bbox[0] > bbox[2] || bbox[1] > bbox[3] {
                        continue;
                    }
                    batch.prepare(&words, &mut dispatches, bbox, s, &mut self.batches);
                    encode_triangle(&mut words, pixel, spans);
                    batch.add(bbox);
                    self.primitives += 1;
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
                    let bbox = [*xs, *ys, *xe, *ye];
                    if bbox[0] > bbox[2] || bbox[1] > bbox[3] {
                        continue;
                    }
                    batch.prepare(&words, &mut dispatches, bbox, s, &mut self.batches);
                    let at = header(&mut words, 1, pixel, bbox);
                    let w = [
                        *x0,
                        *y0,
                        *u,
                        *v,
                        color.0,
                        color.1,
                        color.2,
                        i32::from(flip.0),
                        i32::from(flip.1),
                    ];
                    words[at + 17..at + 26].copy_from_slice(&w.map(|v| v as u32));
                    batch.add(bbox);
                    self.primitives += 1;
                }
                Command::Precise { pixel, vertices, bbox } => {
                    let bbox = [bbox[0], bbox[1], bbox[2], bbox[3]];
                    batch.prepare(&words, &mut dispatches, bbox, s, &mut self.batches);
                    let at = header(&mut words, 3, pixel, bbox);
                    for (k, v) in vertices.iter().enumerate() {
                        let w = [
                            v.x.to_bits(),
                            v.y.to_bits(),
                            v.z.to_bits(),
                            v.color.0 as u32,
                            v.color.1 as u32,
                            v.color.2 as u32,
                            v.uv.0 as u32,
                            v.uv.1 as u32,
                        ];
                        words[at + 17 + 8 * k..at + 25 + 8 * k].copy_from_slice(&w);
                    }
                    batch.add(bbox);
                    self.primitives += 1;
                }
                Command::Line { pixel, a, b, area } => {
                    batch.flush(&mut dispatches, s, &mut self.batches);
                    let at = header(&mut words, 2, pixel, *area);
                    let w = [
                        a.0, a.1, a.2, a.3, a.4, b.0, b.1, b.2, b.3, b.4, area[0], area[1], area[2], area[3],
                    ];
                    words[at + 17..at + 31].copy_from_slice(&w.map(|v| v as u32));
                    dispatches.push(job(
                        Kernel::Line,
                        [(at / PRIM_WORDS) as u32, 0, 0, 0],
                        [0; 4],
                        s,
                        (1, 1),
                    ));
                    self.primitives += 1;
                }
                other => {
                    batch.flush(&mut dispatches, s, &mut self.batches);
                    self.vram_op(other, &mut words, &mut dispatches);
                }
            }
        }
        batch.flush(&mut dispatches, self.scale, &mut self.batches);
        if !dispatches.is_empty() {
            self.run(&words, &dispatches, None);
        }
    }

    /// Uploads, rescales, fills, copies, transfers.
    fn vram_op(&mut self, command: &Command, words: &mut Vec<u32>, dispatches: &mut Vec<Dispatch>) {
        let s = self.scale;
        match command {
            Command::Upload { x, y, w, h, data } => {
                let at = words.len() as u32;
                words.extend(data.iter().map(|&v| u32::from(v)));
                dispatches.push(job(
                    Kernel::Upload,
                    [*x, *y, *w, *h],
                    [at, 0, 0, 0],
                    s,
                    (w.div_ceil(8), h.div_ceil(8)),
                ));
            }
            Command::Rescale { scale } => {
                self.scale = (*scale).clamp(1, MAX_SCALE);
                let s = self.scale;
                dispatches.push(job(
                    Kernel::Rescale,
                    [0; 4],
                    [0; 4],
                    s,
                    ((VRAM_WIDTH * s).div_ceil(8), (VRAM_HEIGHT * s).div_ceil(8)),
                ));
            }
            Command::Fill { x, y, w, h, color } => {
                dispatches.push(job(
                    Kernel::Fill,
                    [*x, *y, *w, *h],
                    [u32::from(*color), 0, 0, 0],
                    s,
                    ((w * s).div_ceil(8), (h * s).div_ceil(8)),
                ));
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
            } => {
                let at = words.len() as u32;
                words.extend_from_slice(skipped);
                let mut j = job(
                    Kernel::Copy,
                    [*sx, *sy, *dx, *dy],
                    [*w, *h, u32::from(*check_mask), u32::from(*set_mask)],
                    s,
                    ((w * s).div_ceil(8), (h * s).div_ceil(8)),
                );
                j.job[8] = at;
                j.job[9] = skipped.len() as u32;
                // Overlapping (or wrapping) areas: the order matters.
                let wraps = sx + w > VRAM_WIDTH || dx + w > VRAM_WIDTH || sy + h > VRAM_HEIGHT || dy + h > VRAM_HEIGHT;
                let overlap = sx < &(dx + w) && dx < &(sx + w) && sy < &(dy + h) && dy < &(sy + h);
                if wraps || overlap {
                    j.kernel = Kernel::CopyInOrder;
                    j.groups = (1, 1);
                }
                dispatches.push(j);
            }
            Command::Write {
                x,
                y,
                w,
                first,
                check_mask,
                pixels,
                ..
            } => {
                let at = words.len() as u32;
                words.extend_from_slice(pixels);
                let n = pixels.len() as u32;
                dispatches.push(job(
                    Kernel::Write,
                    [*x, *y, *w, *first],
                    [n, at, u32::from(*check_mask), 0],
                    s,
                    (n.div_ceil(64), 1),
                ));
            }
            Command::Triangle { .. } | Command::Precise { .. } | Command::Rect { .. } | Command::Line { .. } => {}
        }
    }

    /// Encodes and submits dispatches over `words`; then the display
    /// kernel (its job), when given.
    fn run(&self, words: &[u32], dispatches: &[Dispatch], display: Option<(&Dispatch, &wgpu::ComputePipeline)>) {
        let contents: Vec<u8> = if words.is_empty() {
            vec![0; 4]
        } else {
            words.iter().flat_map(|w| w.to_le_bytes()).collect()
        };
        let words = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("words"),
            contents: &contents,
            usage: wgpu::BufferUsages::STORAGE,
        });
        let all: Vec<&Dispatch> = dispatches.iter().chain(display.map(|d| d.0)).collect();
        let mut jobs = vec![0u8; all.len().max(1) * JOB_STRIDE as usize];
        for (k, d) in all.iter().enumerate() {
            for (i, w) in d.job.iter().enumerate() {
                let at = k * JOB_STRIDE as usize + 4 * i;
                jobs[at..at + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let jobs = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("jobs"),
            contents: &jobs,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ps1-gpu"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.hires.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.native.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: words.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &jobs,
                        offset: 0,
                        size: wgpu::BufferSize::new(64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.screen.as_entire_binding(),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ps1-gpu") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ps1-gpu"),
                timestamp_writes: None,
            });
            for (k, d) in all.iter().enumerate() {
                let pipeline = match display {
                    Some((_, p)) if k == dispatches.len() => p,
                    _ => &self.pipelines[d.kernel as usize],
                };
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &bind, &[(k as u64 * JOB_STRIDE) as u32]);
                pass.dispatch_workgroups(d.groups.0.max(1), d.groups.1.max(1), 1);
            }
        }
        self.queue.submit([encoder.finish()]);
    }

    /// Renders the displayed picture into `screen` and returns its size: as
    /// the software GPU's `render_display` (24-bit pictures from `vram`,
    /// the native VRAM, unscaled).
    fn render_display(&self, d: Display, vram: &[u16]) -> Option<(u32, u32)> {
        let s = self.scale;
        let (w, h) = if d.depth24 { (d.w, d.h) } else { (d.w * s, d.h * s) };
        if w == 0 || h == 0 {
            return None;
        }
        let mut words = Vec::new();
        let mut dispatches = Vec::new();
        if d.depth24 && !d.off {
            // The displayed rows of the native VRAM, as they are now.
            for row in 0..d.h {
                let y = (d.y + row) % VRAM_HEIGHT;
                let at = words.len() as u32;
                let line = &vram[(y * VRAM_WIDTH) as usize..((y + 1) * VRAM_WIDTH) as usize];
                words.extend(line.iter().map(|&v| u32::from(v)));
                dispatches.push(job(
                    Kernel::Upload,
                    [0, y, VRAM_WIDTH, 1],
                    [at, 0, 0, 0],
                    s,
                    (VRAM_WIDTH / 8, 1),
                ));
            }
        }
        let display = job(
            Kernel::Draw,
            [d.x, d.y, w, h],
            [u32::from(d.depth24), u32::from(d.off), 0, 0],
            s,
            (w.div_ceil(8), h.div_ceil(8)),
        );
        self.run(&words, &dispatches, Some((&display, &self.display)));
        Some((w, h))
    }

    /// Shows the displayed picture on the surface, `width`×`height` pixels,
    /// scaled to fill it (`smooth`: bilinear, else nearest), with a half
    /// hidden (netplay's split screen: 1 top, 2 bottom, 3 left, 4 right).
    pub fn present(&mut self, d: Display, vram: &[u16], width: u32, height: u32, smooth: bool, hidden: u32) {
        if self.output.is_none() {
            return;
        }
        let Some(picture) = self.render_display(d, vram) else {
            return;
        };
        if let Some(output) = self.output.as_mut() {
            output.show(
                &self.device,
                &self.queue,
                &self.screen,
                picture,
                (width, height),
                (smooth, hidden),
            );
        }
    }

    /// The displayed picture's size, as `present` shows it.
    pub fn display_size(&self, d: Display) -> (u32, u32) {
        if d.depth24 {
            (d.w, d.h)
        } else {
            (d.w * self.scale, d.h * self.scale)
        }
    }

    /// Copies `words` 32-bit words of a buffer back (native: blocking).
    #[cfg(not(target_arch = "wasm32"))]
    fn read_back(&self, buffer: &wgpu::Buffer, words: u64) -> Vec<u32> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: words * 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, words * 4);
        self.queue.submit([encoder.finish()]);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let out = match slice.get_mapped_range() {
            Ok(view) => view.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect(),
            Err(_) => vec![0; words as usize],
        };
        staging.unmap();
        out
    }

    /// The scaled VRAM, (1024 × scale) by (512 × scale) pixels (native: for
    /// tests and comparisons).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_hires(&self) -> Vec<u16> {
        let s = u64::from(self.scale);
        let n = u64::from(VRAM_WIDTH * VRAM_HEIGHT) * s * s;
        self.read_back(&self.hires, n).into_iter().map(|v| v as u16).collect()
    }

    /// The displayed picture, as RGBA8888, and its size (native).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn display(&self, d: Display, vram: &[u16]) -> Option<(usize, usize, Vec<u8>)> {
        let (w, h) = self.render_display(d, vram)?;
        let words = self.read_back(&self.screen, u64::from(w * h));
        Some((
            w as usize,
            h as usize,
            words.iter().flat_map(|v| v.to_le_bytes()).collect(),
        ))
    }
}

/// A dispatch: its kernel, its job (a, b, c, then d with the scale last),
/// its workgroups.
fn job(kernel: Kernel, a: [u32; 4], b: [u32; 4], scale: u32, groups: (u32, u32)) -> Dispatch {
    let mut job = [0u32; 16];
    job[..4].copy_from_slice(&a);
    job[4..8].copy_from_slice(&b);
    job[15] = scale;
    Dispatch { kernel, job, groups }
}

/// Starts a primitive's words (aligned on `PRIM_WORDS`): kind (0 triangle,
/// 1 rectangle, 2 line, 3 precise triangle: after the header, each vertex's
/// x, y, depth (floats), colour and texture coordinates), flags, blend mode, colour depth, texture window
/// (mask x, y, offset x, y), page base x, y, CLUT row start and x, scale,
/// box (xs, xe, ys, ye). Returns where it starts.
fn header(words: &mut Vec<u32>, kind: u32, p: &PixelMode, bbox: [i32; 4]) -> usize {
    let at = words.len().next_multiple_of(PRIM_WORDS);
    words.resize(at + PRIM_WORDS, 0);
    let m = &p.prim;
    let flags = u32::from(p.textured)
        | u32::from(p.shaded) << 1
        | u32::from(m.semi) << 2
        | u32::from(m.raw) << 3
        | u32::from(m.dither) << 4
        | u32::from(p.check_mask) << 5
        | u32::from(p.set_mask) << 6;
    let w = [
        kind,
        flags,
        m.blend,
        m.depth,
        m.mask_x as u32,
        m.mask_y as u32,
        m.off_x as u32,
        m.off_y as u32,
        m.base_x as u32,
        m.base_y as u32,
        m.clut_row as u32,
        m.clut_x as u32,
        p.scale as u32,
        bbox[0] as u32,
        bbox[2] as u32,
        bbox[1] as u32,
        bbox[3] as u32,
    ];
    words[at..at + 17].copy_from_slice(&w);
    at
}

/// A triangle: after the header, the edge functions at the first row (17,
/// 18, 19), their steps along x and y (20 to 25), the attributes there (26
/// to 35, 64 bits each) and their steps along x (36 to 45) and y (46 to
/// 55), the flat colour (56 to 58).
fn encode_triangle(words: &mut Vec<u32>, pixel: &PixelMode, s: &Spans) {
    let at = header(words, 0, pixel, [s.xs, s.ys, s.xe, s.ye]);
    let w = &mut words[at..at + PRIM_WORDS];
    for e in 0..3 {
        w[17 + e] = s.row[e] as i32 as u32;
        w[20 + 2 * e] = s.steps[e].0 as i32 as u32;
        w[21 + 2 * e] = s.steps[e].1 as i32 as u32;
    }
    let wide = |v: i64| [v as u64 as u32, ((v as u64) >> 32) as u32];
    for a in 0..5 {
        w[26 + 2 * a..28 + 2 * a].copy_from_slice(&wide(s.attr_row[a]));
        w[36 + 2 * a..38 + 2 * a].copy_from_slice(&wide(s.planes[a].1));
        w[46 + 2 * a..48 + 2 * a].copy_from_slice(&wide(s.planes[a].2));
    }
    w[56] = s.flat.0 as u32;
    w[57] = s.flat.1 as u32;
    w[58] = s.flat.2 as u32;
}

/// Triangles and rectangles gathered for one dispatch.
#[derive(Default)]
struct Batch {
    first: usize,
    count: usize,
    area: Option<[i32; 4]>,
    /// The primitives' own areas, summed: a batch stops before its area
    /// grows much past them (threads that mostly find nothing to draw).
    covered: i64,
}

impl Batch {
    /// Makes room for a primitive covering `bbox`.
    fn prepare(
        &mut self,
        words: &[u32],
        dispatches: &mut Vec<Dispatch>,
        bbox: [i32; 4],
        scale: u32,
        batches: &mut u64,
    ) {
        if self.count > 0 {
            let a = self.area.unwrap_or(bbox);
            let union = [
                a[0].min(bbox[0]),
                a[1].min(bbox[1]),
                a[2].max(bbox[2]),
                a[3].max(bbox[3]),
            ];
            let size = |r: [i32; 4]| i64::from(r[2] - r[0] + 1) * i64::from(r[3] - r[1] + 1);
            let sparse = size(union) > 4 * (self.covered + size(bbox)) + 128 * 128;
            if self.count == BATCH || sparse {
                self.flush(dispatches, scale, batches);
            }
        }
        if self.count == 0 {
            self.first = words.len().div_ceil(PRIM_WORDS);
        }
    }

    fn add(&mut self, bbox: [i32; 4]) {
        self.count += 1;
        self.covered += i64::from(bbox[2] - bbox[0] + 1) * i64::from(bbox[3] - bbox[1] + 1);
        self.area = Some(match self.area {
            None => bbox,
            Some(a) => [
                a[0].min(bbox[0]),
                a[1].min(bbox[1]),
                a[2].max(bbox[2]),
                a[3].max(bbox[3]),
            ],
        });
    }

    fn flush(&mut self, dispatches: &mut Vec<Dispatch>, scale: u32, batches: &mut u64) {
        if let (true, Some(a)) = (self.count > 0, self.area) {
            dispatches.push(job(
                Kernel::Draw,
                [self.first as u32, self.count as u32, a[0] as u32, a[1] as u32],
                [0; 4],
                scale,
                (
                    ((a[2] - a[0] + 1) as u32).div_ceil(8),
                    ((a[3] - a[1] + 1) as u32).div_ceil(8),
                ),
            ));
            *batches += 1;
        }
        *self = Batch::default();
    }
}
/// A surface the picture is shown on, and the pipeline scaling it there.
struct Output {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}

impl Output {
    fn new(device: &wgpu::Device, surface: wgpu::Surface<'static>, adapter: &wgpu::Adapter) -> Self {
        let caps = surface.get_capabilities(adapter);
        // Colours as they are (as a 2D canvas shows them): not sRGB-encoded.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: 0,
            height: 0,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: Vec::new(),
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("present.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("present.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("present"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("present"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("present"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            surface,
            config,
            layout,
            pipeline,
        }
    }

    fn show(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &wgpu::Buffer,
        picture: (u32, u32),
        size: (u32, u32),
        (smooth, hidden): (bool, u32),
    ) {
        let size = (size.0.max(1), size.1.max(1));
        if (self.config.width, self.config.height) != size {
            self.config.width = size.0;
            self.config.height = size.1;
            self.surface.configure(device, &self.config);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            _ => {
                // Lost or outdated (resized): set up again, show the next one.
                self.surface.configure(device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut uniform = [0u8; 16];
        for (i, v) in [picture.0, picture.1, u32::from(smooth), hidden].iter().enumerate() {
            uniform[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("view"),
            contents: &uniform,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: screen.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("present") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
        queue.present(frame);
    }
}
