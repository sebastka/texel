//! The PlayStation 2's Graphics Synthesizer on the GPU, with wgpu: Vulkan,
//! Metal or DX12 natively, WebGPU in browsers.
//!
//! The GS's 4 MiB local memory is a storage buffer, in the GS's own layouts
//! (as the software GS keeps it), and compute shaders write it: the
//! commands the software GS records (draws, transfers, copies, CLUT loads)
//! run in their order. Draws are batched: each batch is one dispatch over
//! the area it covers, where every thread owns a pixel and goes through the
//! batch's primitives in order. A batch ends at a transfer, a copy, a CLUT
//! load, or a primitive whose texture is memory the batch writes.

pub mod prims;

/// For surface targets (a browser's canvas, say).
pub use wgpu;

use texel_ps2::gs::{Command, Privileged};
use wgpu::util::DeviceExt;

use prims::{PRIM_WORDS, Prim, Region};

/// GS local memory, in 32-bit words.
const VRAM_WORDS: u64 = 1024 * 1024;
/// CLUT slots (256 entries each): every load writes the next one, and
/// draws use the slot current when they were recorded.
const CLUT_SLOTS: u32 = 1024;
/// Primitives per batch, at most.
const BATCH: usize = 256;
/// Uniform slots are 256 bytes apart (WebGPU's minimum offset alignment).
const JOB_STRIDE: u64 = 256;

/// One dispatch's parameters (the shader's `Job`).
#[derive(Clone, Copy, Default)]
struct Job {
    words: [u32; 16],
}

enum Kernel {
    Draw,
    Pixels,
    Copy,
    Clut,
}

struct Dispatch {
    kernel: Kernel,
    job: Job,
    groups: (u32, u32),
}

pub struct GpuGs {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    draw: wgpu::ComputePipeline,
    pixels: wgpu::ComputePipeline,
    copy: wgpu::ComputePipeline,
    clut: wgpu::ComputePipeline,
    display: wgpu::ComputePipeline,
    vram: wgpu::Buffer,
    cluts: wgpu::Buffer,
    screen: wgpu::Buffer,
    /// The CLUT slot draws use now.
    clut_slot: u32,
    /// Batches and primitives submitted, for diagnostics.
    pub batches: u64,
    pub primitives: u64,
    /// The adapter's name and backend (empty in browsers).
    pub adapter: String,
    /// Where pictures are shown, if anywhere.
    output: Option<Output>,
}

impl GpuGs {
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
                label: Some("gs"),
                required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no GPU device: {e}"))?;
        // Browsers keep the adapter's name to themselves.
        let name = if info.name.is_empty() {
            String::new()
        } else {
            format!("{} ({:?})", info.name, info.backend)
        };
        let mut gs = Self::with_device(device, queue, name);
        if let Some(surface) = surface {
            gs.output = Some(Output::new(&gs.device, surface, &adapter));
        }
        Ok(gs)
    }

    fn with_device(device: wgpu::Device, queue: wgpu::Queue, adapter: String) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gs.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gs.wgsl").into()),
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
            label: Some("gs"),
            entries: &[
                storage(0, false),
                storage(1, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(64),
                    },
                    count: None,
                },
                storage(3, false),
                storage(4, true),
                storage(5, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gs"),
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
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let copyable = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
        Self {
            draw: pipeline("draw"),
            pixels: pipeline("pixels"),
            copy: pipeline("copy"),
            clut: pipeline("clut"),
            display: pipeline("display"),
            vram: buffer("vram", VRAM_WORDS * 4, copyable),
            cluts: buffer("cluts", u64::from(CLUT_SLOTS) * 256 * 4, copyable),
            screen: buffer("screen", 1024 * 1024 * 4, copyable),
            layout,
            device,
            queue,
            clut_slot: 0,
            batches: 0,
            primitives: 0,
            adapter,
            output: None,
        }
    }

    /// Runs commands recorded by the software GS, in order.
    pub fn submit(&mut self, commands: &[Command]) {
        let mut prims: Vec<u32> = Vec::new();
        let mut data: Vec<u32> = Vec::new();
        let mut dispatches: Vec<Dispatch> = Vec::new();
        let mut batch = Batch::default();
        for command in commands {
            match command {
                Command::Draw { kind, regs, vertices } => {
                    let Some(prim) = prims::setup(*kind, regs, vertices, self.clut_slot) else {
                        continue;
                    };
                    if batch.count == BATCH || batch.conflicts(&prim) {
                        batch.flush(&mut dispatches, &mut self.batches);
                    }
                    if batch.count == 0 {
                        batch.first = (prims.len() / PRIM_WORDS) as u32;
                    }
                    prims.extend_from_slice(&prim.words);
                    batch.add(&prim);
                    self.primitives += 1;
                }
                Command::Pixels {
                    bp,
                    bw,
                    psm,
                    x,
                    y,
                    w,
                    first,
                    data: pixels,
                } => {
                    batch.flush(&mut dispatches, &mut self.batches);
                    let offset = data.len() as u32;
                    data.extend_from_slice(pixels);
                    let count = pixels.len() as u32;
                    let mut job = Job::default();
                    job.words[..12].copy_from_slice(&[offset, count, *first, *w, *bp, *bw, *psm, 0, *x, *y, 0, 0]);
                    // Up to 65535 groups of 64 along x, then rows of them.
                    let groups = count.div_ceil(64);
                    dispatches.push(Dispatch {
                        kernel: Kernel::Pixels,
                        job,
                        groups: (groups.min(65535), groups.div_ceil(65535)),
                    });
                }
                Command::Copy {
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
                } => {
                    batch.flush(&mut dispatches, &mut self.batches);
                    let mut job = Job::default();
                    job.words[..12]
                        .copy_from_slice(&[*sbp, *sbw, *spsm, *w, *dbp, *dbw, *dpsm, *h, *sx, *sy, *dx, *dy]);
                    dispatches.push(Dispatch {
                        kernel: Kernel::Copy,
                        job,
                        groups: (w.div_ceil(8), h.div_ceil(8)),
                    });
                }
                Command::Clut { tex0 } => {
                    batch.flush(&mut dispatches, &mut self.batches);
                    let previous = self.clut_slot;
                    self.clut_slot = (self.clut_slot + 1) % CLUT_SLOTS;
                    let mut job = Job::default();
                    job.words[..6].copy_from_slice(&[
                        self.clut_slot,
                        previous,
                        0,
                        0,
                        *tex0 as u32,
                        (*tex0 >> 32) as u32,
                    ]);
                    dispatches.push(Dispatch {
                        kernel: Kernel::Clut,
                        job,
                        groups: (4, 1),
                    });
                }
            }
        }
        batch.flush(&mut dispatches, &mut self.batches);
        if dispatches.is_empty() {
            return;
        }
        self.run(&prims, &data, &dispatches);
    }

    /// Encodes and submits dispatches, with their primitives and data.
    fn run(&self, prims: &[u32], data: &[u32], dispatches: &[Dispatch]) {
        let storage = |label: &str, words: &[u32]| {
            // Bindings may not be empty.
            let contents: Vec<u8> = if words.is_empty() {
                vec![0; 4]
            } else {
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            };
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &contents,
                usage: wgpu::BufferUsages::STORAGE,
            })
        };
        let prims = storage("prims", prims);
        let data = storage("data", data);
        let mut jobs = vec![0u8; dispatches.len() * JOB_STRIDE as usize];
        for (k, d) in dispatches.iter().enumerate() {
            for (i, w) in d.job.words.iter().enumerate() {
                let at = k * JOB_STRIDE as usize + 4 * i;
                jobs[at..at + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let jobs = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("jobs"),
            contents: &jobs,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = self.bind_group(&prims, &jobs, &data);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gs") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gs"),
                timestamp_writes: None,
            });
            for (k, d) in dispatches.iter().enumerate() {
                pass.set_pipeline(match d.kernel {
                    Kernel::Draw => &self.draw,
                    Kernel::Pixels => &self.pixels,
                    Kernel::Copy => &self.copy,
                    Kernel::Clut => &self.clut,
                });
                pass.set_bind_group(0, &bind, &[(k as u64 * JOB_STRIDE) as u32]);
                pass.dispatch_workgroups(d.groups.0.max(1), d.groups.1.max(1), 1);
            }
        }
        self.queue.submit([encoder.finish()]);
    }

    fn bind_group(&self, prims: &wgpu::Buffer, jobs: &wgpu::Buffer, data: &wgpu::Buffer) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gs"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.vram.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: prims.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: jobs,
                        offset: 0,
                        size: wgpu::BufferSize::new(64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.cluts.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: data.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: self.screen.as_entire_binding(),
                },
            ],
        })
    }

    /// The displayed picture's read circuit (as `Gs::display`): base, width,
    /// format, position and size; `None` when the display is off.
    fn display_area(p: &Privileged) -> Option<[u32; 7]> {
        let field = |v: u64, lo: u32, n: u32| ((v >> lo) & ((1 << n) - 1)) as u32;
        let pmode = p.regs[0];
        let circuit = if pmode & 2 != 0 {
            1
        } else if pmode & 1 != 0 {
            0
        } else {
            return None;
        };
        let dispfb = p.regs[7 + 2 * circuit];
        let display = p.regs[8 + 2 * circuit];
        let magh = field(display, 23, 4) + 1;
        let magv = field(display, 27, 2) + 1;
        let w = ((field(display, 32, 12) + 1) / magh).clamp(1, 1024);
        let mut h = ((field(display, 44, 11) + 1) / magv).clamp(1, 1024);
        let smode2 = p.regs[2];
        if smode2 & 1 != 0 && smode2 & 2 != 0 {
            h /= 2;
        }
        Some([
            field(dispfb, 0, 9) * 32,
            field(dispfb, 9, 6),
            field(dispfb, 15, 5),
            w,
            field(dispfb, 32, 11),
            field(dispfb, 43, 11),
            h,
        ])
    }

    /// Reads the displayed picture into `screen` (RGBA, one u32 a pixel)
    /// and returns its size.
    fn render_display(&self, p: &Privileged) -> Option<(u32, u32)> {
        let [fbp, fbw, psm, w, x, y, h] = Self::display_area(p)?;
        if w == 0 || h == 0 {
            return None;
        }
        let mut job = Job::default();
        job.words[..8].copy_from_slice(&[fbp, fbw, psm, w, x, y, h, 0]);
        let empty = |label: &str| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &[0; 4],
                usage: wgpu::BufferUsages::STORAGE,
            })
        };
        let mut jobs = vec![0u8; JOB_STRIDE as usize];
        for (i, v) in job.words.iter().enumerate() {
            jobs[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        let jobs = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("jobs"),
            contents: &jobs,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = self.bind_group(&empty("prims"), &jobs, &empty("data"));
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("display") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("display"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.display);
            pass.set_bind_group(0, &bind, &[0]);
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
        self.queue.submit([encoder.finish()]);
        Some((w, h))
    }

    /// The displayed picture's size, (0, 0) when the display is off.
    pub fn display_size(p: &Privileged) -> (u32, u32) {
        Self::display_area(p).map_or((0, 0), |a| (a[3], a[6]))
    }

    /// Clears the GS memory (a console reset).
    pub fn clear(&mut self) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
        encoder.clear_buffer(&self.vram, 0, None);
        self.queue.submit([encoder.finish()]);
    }

    /// Shows the displayed picture on the surface, `width`×`height` pixels,
    /// scaled to fill it (`smooth`: bilinear, else nearest). Without a
    /// surface, or with the display off, it shows nothing.
    pub fn present(&mut self, p: &Privileged, width: u32, height: u32, smooth: bool) {
        let Some((w, h)) = (if self.output.is_some() {
            self.render_display(p)
        } else {
            None
        }) else {
            return;
        };
        let Some(output) = self.output.as_mut() else {
            return;
        };
        output.show(&self.device, &self.queue, &self.screen, (w, h), (width, height), smooth);
    }

    /// The GS memory and the CLUT draws use now, for a save state: copied
    /// when called (later commands do not change them), ready when the
    /// future is (natively, at once).
    pub fn read_state(&self) -> impl std::future::Future<Output = (Vec<u32>, Vec<u32>)> + 'static {
        let words = VRAM_WORDS + 256;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("state"),
            size: words * 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&self.vram, 0, &staging, 0, VRAM_WORDS * 4);
        encoder.copy_buffer_to_buffer(
            &self.cluts,
            u64::from(self.clut_slot) * 256 * 4,
            &staging,
            VRAM_WORDS * 4,
            256 * 4,
        );
        self.queue.submit([encoder.finish()]);
        let mapped = Mapped::default();
        let signal = mapped.0.clone();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let mut s = signal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.0 = Some(r.is_ok());
            if let Some(w) = s.1.take() {
                w.wake();
            }
        });
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        async move {
            let ok = mapped.await;
            let mut out = match staging.slice(..).get_mapped_range() {
                Ok(view) if ok => view.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect(),
                _ => vec![0; words as usize],
            };
            staging.unmap();
            let clut = out.split_off(VRAM_WORDS as usize);
            (out, clut)
        }
    }

    /// Restores the GS memory and the CLUT draws use (from a save state).
    pub fn restore(&mut self, vram: &[u32], clut: &[u32]) {
        let bytes = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let n = vram.len().min(VRAM_WORDS as usize);
        self.queue.write_buffer(&self.vram, 0, &bytes(&vram[..n]));
        self.clut_slot = 0;
        self.queue
            .write_buffer(&self.cluts, 0, &bytes(&clut[..clut.len().min(256)]));
        self.queue.submit([]);
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

    /// The GS memory (native: for tests and comparisons).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn read_vram(&self) -> Vec<u32> {
        self.read_back(&self.vram, VRAM_WORDS)
    }

    /// The displayed picture, as RGBA8888, and its size (native).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn display(&self, p: &Privileged) -> Option<(usize, usize, Vec<u8>)> {
        let (w, h) = self.render_display(p)?;
        let words = self.read_back(&self.screen, u64::from(w * h));
        Some((
            w as usize,
            h as usize,
            words.iter().flat_map(|v| v.to_le_bytes()).collect(),
        ))
    }
}

/// A buffer's mapping: done (and whether it worked), and who waits.
#[derive(Default)]
struct Mapped(std::sync::Arc<std::sync::Mutex<(Option<bool>, Option<std::task::Waker>)>>);

impl std::future::Future for Mapped {
    type Output = bool;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<bool> {
        let mut s = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match s.0 {
            Some(ok) => std::task::Poll::Ready(ok),
            None => {
                s.1 = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
}

/// Draws gathered for one dispatch.
#[derive(Default)]
struct Batch {
    first: u32,
    count: usize,
    area: Option<[i32; 4]>,
    writes: Vec<Region>,
    reads: Vec<Region>,
}

impl Batch {
    /// Whether `p` must wait for the batch: a thread per pixel runs the
    /// batch's primitives in order, but threads run in no order, so `p` may
    /// not read what the batch writes, write what it reads, or write what it
    /// writes through another layout (another thread's pixel).
    fn conflicts(&self, p: &Prim) -> bool {
        let raw = p.reads.is_some_and(|r| self.writes.iter().any(|w| w.overlaps(&r)));
        let war = p
            .writes
            .iter()
            .flatten()
            .any(|w| self.reads.iter().any(|r| r.overlaps(w)));
        let waw = p
            .writes
            .iter()
            .flatten()
            .any(|w| self.writes.iter().any(|o| o.overlaps(w) && o.layout != w.layout));
        raw || war || waw
    }

    fn add(&mut self, p: &Prim) {
        self.count += 1;
        self.area = Some(match self.area {
            None => p.bbox,
            Some(a) => [
                a[0].min(p.bbox[0]),
                a[1].min(p.bbox[1]),
                a[2].max(p.bbox[2]),
                a[3].max(p.bbox[3]),
            ],
        });
        for w in p.writes.into_iter().flatten() {
            if !self.writes.contains(&w) {
                self.writes.push(w);
            }
        }
        if let Some(r) = p.reads
            && !self.reads.contains(&r)
        {
            self.reads.push(r);
        }
    }

    fn flush(&mut self, dispatches: &mut Vec<Dispatch>, batches: &mut u64) {
        if let (true, Some(area)) = (self.count > 0, self.area) {
            let mut job = Job::default();
            job.words[..4].copy_from_slice(&[self.first, self.count as u32, area[0] as u32, area[1] as u32]);
            dispatches.push(Dispatch {
                kernel: Kernel::Draw,
                job,
                groups: (
                    ((area[2] - area[0]) as u32).div_ceil(8),
                    ((area[3] - area[1]) as u32).div_ceil(8),
                ),
            });
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
        smooth: bool,
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
        for (i, v) in [picture.0, picture.1, u32::from(smooth), 0].iter().enumerate() {
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
