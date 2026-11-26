//! GPU: command processing, a software rasteriser into the 1024x512 VRAM,
//! video timing, and frame output.

pub const VRAM_WIDTH: usize = 1024;
pub const VRAM_HEIGHT: usize = 512;
/// Highest internal resolution multiplier.
pub const MAX_SCALE: usize = 4;

/// What happened during [`Gpu::tick`], for the timers and the interrupts.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuTick {
    pub vblank_start: bool,
    pub hblanks: u32,
    /// Dot clock divider of the current horizontal resolution.
    pub dot_divider: u32,
}

#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct Vertex {
    x: i32,
    y: i32,
    r: i32,
    g: i32,
    b: i32,
    u: i32,
    v: i32,
    /// Where the GTE projected it exactly (x, y with the drawing offset,
    /// and depth), when known (an enhancement: see `Gpu::set_precise`).
    #[serde(skip)]
    exact: Option<[f32; 3]>,
}

impl Vertex {
    fn color(&mut self, c: u32) {
        self.r = (c & 0xff) as i32;
        self.g = ((c >> 8) & 0xff) as i32;
        self.b = ((c >> 16) & 0xff) as i32;
    }

    fn position(&mut self, p: u32) {
        // 11-bit signed coordinates.
        self.x = ((p as i32) << 21) >> 21;
        self.y = (((p >> 16) as i32) << 21) >> 21;
    }

    fn uv(&mut self, t: u32) {
        self.u = (t & 0xff) as i32;
        self.v = ((t >> 8) & 0xff) as i32;
    }
}

/// How a primitive is drawn.
#[derive(Clone, Copy, Default)]
struct DrawMode {
    shaded: bool,
    textured: bool,
    semi: bool,
    /// Texture colour used as-is, not modulated by the vertex colour.
    raw: bool,
    dither: bool,
    /// Texture page bits (E1 format): base, semi-transparency mode, depth.
    tpage: u32,
    clut_x: usize,
    clut_y: usize,
}

impl DrawMode {
    /// GPU cycles per pixel: texturing and semi-transparency (which reads
    /// the destination) each add one.
    fn cost(&self) -> u64 {
        1 + u64::from(self.textured) + u64::from(self.semi)
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Transfer {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    i: u32,
}

impl Transfer {
    fn new(xy: u32, wh: u32) -> Self {
        let w = (((wh & 0xffff).wrapping_sub(1)) & 0x3ff) + 1;
        let h = (((wh >> 16).wrapping_sub(1)) & 0x1ff) + 1;
        Self {
            x: xy & 0x3ff,
            y: (xy >> 16) & 0x1ff,
            w,
            h,
            i: 0,
        }
    }

    /// The next VRAM position, or `None` when the transfer is complete.
    fn next(&mut self) -> Option<(usize, usize)> {
        if self.i >= self.w * self.h {
            return None;
        }
        let x = (self.x + self.i % self.w) & 0x3ff;
        let y = (self.y + self.i / self.w) & 0x1ff;
        self.i += 1;
        Some((x as usize, y as usize))
    }

    fn done(&self) -> bool {
        self.i >= self.w * self.h
    }
}

const DITHER: [[i32; 4]; 4] = [[-4, 0, -3, 1], [2, -2, 3, -1], [-3, 1, -4, 0], [3, -1, 2, -2]];

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Gpu {
    #[serde(with = "crate::rewind::big")]
    pub vram: Box<[u16]>,
    /// Internal resolution multiplier, and the scaled VRAM (empty at 1).
    #[serde(skip)]
    scale: usize,
    #[serde(skip)]
    hires: Box<[u16]>,
    // Drawing state.
    texpage: u32,
    tex_window: u32,
    area_x1: i32,
    area_y1: i32,
    area_x2: i32,
    area_y2: i32,
    offset_x: i32,
    offset_y: i32,
    set_mask: bool,
    check_mask: bool,
    rect_flip_x: bool,
    rect_flip_y: bool,
    // Display state.
    display_x: u32,
    display_y: u32,
    h_range: (u32, u32),
    v_range: (u32, u32),
    display_mode: u32,
    display_disabled: bool,
    texture_disable: bool,
    dma_dir: u32,
    irq: bool,
    /// Set when GP0(1Fh) requests an interrupt, until the bus raises it.
    pub irq_raised: bool,
    // Command processing.
    fifo: Vec<u32>,
    /// Polyline being drawn: its previous vertex, and the colour word of
    /// the next vertex when shaded.
    polyline: Option<(Vertex, Option<u32>)>,
    load: Option<Transfer>,
    store: Option<Transfer>,
    read_latch: u32,
    // Timing.
    line_frac: u32,
    scanline: u32,
    field: bool,
    pub frame_ready: bool,
    /// Display area changes (page flips), for measuring games' frame rates.
    #[serde(skip)]
    pub flips: u32,
    /// GPU cycles of drawing just sent, and the drawing still under way, in
    /// CPU cycles: the GPU draws while the CPU runs (see `wait_drawing`).
    /// `drawing` is saved apart (`crate::Timing`), after the rest.
    busy: u64,
    #[serde(skip)]
    pub(crate) drawing: u64,
    /// Recording for another renderer (see `set_recording`): what changes
    /// the scaled VRAM, and the native area drawn since the recorder last
    /// sent it (inclusive).
    #[serde(skip)]
    record: bool,
    #[serde(skip)]
    commands: Vec<Command>,
    #[serde(skip)]
    dirty: Option<[i32; 4]>,
    /// Precise geometry (recording only): the GTE's exact projections, by
    /// the screen position they were rounded to (`EXACT` slots).
    #[serde(skip)]
    exact: Option<Box<[ExactEntry]>>,
    /// Frames started, to age the table's entries (games draw a frame's
    /// polygons during the next one).
    #[serde(skip)]
    exact_frame: u32,
    /// Recording, the native VRAM drawn by the recording's renderer only
    /// (see `set_deferred`), and whether this one is behind it.
    #[serde(skip)]
    defer: bool,
    #[serde(skip)]
    stale: bool,
    #[serde(skip)]
    fetch: Option<VramFetch>,
    /// Deferred: the VRAMs primitives were last recorded for (native,
    /// scaled; see `Command::Target`).
    #[serde(skip)]
    target: (bool, bool),
}

/// Brings a native VRAM up to date from the renderer the drawing is
/// recorded for (see `Gpu::set_deferred`): it is given the commands
/// recorded since the last ones it was sent, draws them, and writes its
/// native VRAM into the slice.
pub type VramFetch = Box<dyn FnMut(Vec<Command>, &mut [u16])>;

/// An exact projection: the position it was rounded to (packed as SXY),
/// its x, y and depth, and the frame it was made in.
type ExactEntry = (u32, [f32; 3], u32);

/// Slots of the exact projections' table (by a hash of their position),
/// and how many frames an entry stays valid.
const EXACT: usize = 1 << 16;
const EXACT_FRAMES: u32 = 2;

fn exact_slot(sxy: u32) -> usize {
    (sxy.wrapping_mul(0x9e37_79b1) >> 16) as usize & (EXACT - 1)
}

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpu {
    /// Rollback (see `Bus::adopt`): VRAM's buffer, and what the scaled
    /// renderers draw with, which carry on (replayed frames are drawn
    /// again over the ones dropped).
    pub(crate) fn adopt(&mut self, old: &mut Gpu) {
        self.vram = std::mem::take(&mut old.vram);
        self.scale = old.scale;
        self.hires = std::mem::take(&mut old.hires);
        self.flips = old.flips;
        self.record = old.record;
        self.commands = std::mem::take(&mut old.commands);
        self.dirty = old.dirty;
        self.exact = old.exact.take();
        self.exact_frame = old.exact_frame;
        self.defer = old.defer;
        self.stale = old.stale;
        self.fetch = old.fetch.take();
        self.target = old.target;
    }

    pub fn new() -> Self {
        let mut gpu = Self {
            vram: vec![0; VRAM_WIDTH * VRAM_HEIGHT].into_boxed_slice(),
            scale: 1,
            hires: Box::new([]),
            texpage: 0,
            tex_window: 0,
            area_x1: 0,
            area_y1: 0,
            area_x2: 0,
            area_y2: 0,
            offset_x: 0,
            offset_y: 0,
            set_mask: false,
            check_mask: false,
            rect_flip_x: false,
            rect_flip_y: false,
            display_x: 0,
            display_y: 0,
            h_range: (0x200, 0xc00),
            v_range: (0x10, 0x100),
            display_mode: 0,
            display_disabled: true,
            texture_disable: false,
            dma_dir: 0,
            irq: false,
            irq_raised: false,
            fifo: Vec::with_capacity(16),
            polyline: None,
            load: None,
            store: None,
            read_latch: 0,
            line_frac: 0,
            scanline: 0,
            field: false,
            frame_ready: false,
            flips: 0,
            busy: 0,
            drawing: 0,
            record: false,
            commands: Vec::new(),
            dirty: None,
            exact: None,
            exact_frame: 0,
            defer: false,
            stale: false,
            fetch: None,
            target: (false, true),
        };
        gpu.reset();
        gpu
    }

    fn reset(&mut self) {
        self.texpage = 0;
        self.tex_window = 0;
        (self.area_x1, self.area_y1, self.area_x2, self.area_y2) = (0, 0, 0, 0);
        (self.offset_x, self.offset_y) = (0, 0);
        self.set_mask = false;
        self.check_mask = false;
        self.display_x = 0;
        self.display_y = 0;
        self.h_range = (0x200, 0xc00);
        self.v_range = (0x10, 0x100);
        self.display_mode = 0;
        self.display_disabled = true;
        self.dma_dir = 0;
        self.irq = false;
        self.fifo.clear();
        self.polyline = None;
        self.load = None;
        self.store = None;
    }

    pub fn is_pal(&self) -> bool {
        self.display_mode & 8 != 0
    }

    fn interlaced(&self) -> bool {
        self.display_mode & 0x20 != 0
    }

    pub(crate) fn dot_divider(&self) -> u32 {
        if self.display_mode & 0x40 != 0 {
            7
        } else {
            [10, 8, 5, 4][(self.display_mode & 3) as usize]
        }
    }

    pub fn status(&self) -> u32 {
        let mut s = self.texpage & 0x7ff;
        s |= u32::from(self.set_mask) << 11;
        s |= u32::from(self.check_mask) << 12;
        s |= u32::from(!self.interlaced() || self.field) << 13;
        s |= u32::from(self.texture_disable) << 15;
        s |= ((self.display_mode >> 6) & 1) << 16;
        s |= (self.display_mode & 3) << 17;
        s |= ((self.display_mode >> 2) & 1) << 19;
        s |= ((self.display_mode >> 3) & 1) << 20;
        s |= ((self.display_mode >> 4) & 1) << 21;
        s |= ((self.display_mode >> 5) & 1) << 22;
        s |= u32::from(self.display_disabled) << 23;
        s |= u32::from(self.irq) << 24;
        // Ready for a command once the drawing under way is done.
        let ready_cmd = u32::from(self.busy == 0 && self.drawing == 0) << 26;
        let ready_read = u32::from(self.store.is_some()) << 27;
        let ready_dma = 1 << 28;
        s |= ready_cmd | ready_read | ready_dma;
        s |= self.dma_dir << 29;
        s |= match self.dma_dir {
            1 => 1 << 25,
            2 => 1 << 25,
            3 => ready_read >> 2,
            _ => 0,
        };
        let in_vblank = self.scanline < self.v_range.0 || self.scanline >= self.v_range.1;
        let odd = if self.interlaced() {
            self.field
        } else {
            self.scanline & 1 != 0
        };
        s |= u32::from(odd && !in_vblank) << 31;
        s
    }

    pub fn read_data(&mut self) -> u32 {
        if self.store.is_some() {
            self.sync_vram();
        }
        if let Some(mut t) = self.store.take() {
            let mut v = 0;
            for half in 0..2 {
                if let Some((x, y)) = t.next() {
                    v |= u32::from(self.vram[y * VRAM_WIDTH + x]) << (16 * half);
                }
            }
            if !t.done() {
                self.store = Some(t);
            }
            self.read_latch = v;
        }
        self.read_latch
    }

    pub fn gp1(&mut self, v: u32) {
        match v >> 24 {
            0x00 => self.reset(),
            0x01 => {
                self.fifo.clear();
                self.polyline = None;
                self.load = None;
            }
            0x02 => self.irq = false,
            0x03 => self.display_disabled = v & 1 != 0,
            0x04 => self.dma_dir = v & 3,
            0x05 => {
                let (x, y) = (v & 0x3fe, (v >> 10) & 0x1ff);
                if (x, y) != (self.display_x, self.display_y) {
                    self.flips += 1;
                }
                self.display_x = x;
                self.display_y = y;
            }
            0x06 => self.h_range = (v & 0xfff, (v >> 12) & 0xfff),
            0x07 => self.v_range = (v & 0x3ff, (v >> 10) & 0x3ff),
            0x08 => self.display_mode = v & 0xff,
            0x09 => self.texture_disable = v & 1 != 0,
            0x10..=0x1f => {
                self.read_latch = match v & 0xf {
                    2 => self.tex_window,
                    3 => (self.area_x1 as u32) | ((self.area_y1 as u32) << 10),
                    4 => (self.area_x2 as u32) | ((self.area_y2 as u32) << 10),
                    5 => ((self.offset_x as u32) & 0x7ff) | (((self.offset_y as u32) & 0x7ff) << 11),
                    7 => 2,
                    _ => self.read_latch,
                };
            }
            _ => {}
        }
    }

    /// Words of a GP0 command, from its first word; `None` for polylines.
    fn command_len(&self, cmd: u32) -> Option<usize> {
        let op = cmd >> 24;
        Some(match op {
            0x02 => 3,
            0x20..=0x3f => {
                let n = if op & 8 != 0 { 4 } else { 3 };
                let per_vertex = 1 + usize::from(op & 4 != 0) + usize::from(op & 0x10 != 0);
                n * per_vertex + usize::from(op & 0x10 == 0)
            }
            0x40..=0x5f => {
                if op & 8 != 0 {
                    return None;
                }
                if op & 0x10 != 0 { 4 } else { 3 }
            }
            0x60..=0x7f => 2 + usize::from(op & 4 != 0) + usize::from(op & 0x18 == 0),
            0x80..=0x9f => 4,
            0xa0..=0xdf => 3,
            _ => 1,
        })
    }

    pub fn gp0(&mut self, v: u32) {
        if let Some(mut t) = self.load.take() {
            for half in 0..2 {
                let index = t.i;
                if let Some((x, y)) = t.next() {
                    self.write_pixel_masked(x, y, (v >> (16 * half)) as u16, (t.x, t.y, t.w, t.h), index);
                }
            }
            if !t.done() {
                self.load = Some(t);
            }
            return;
        }
        if self.polyline.is_some() {
            self.polyline_word(v);
            return;
        }
        self.fifo.push(v);
        let cmd = self.fifo[0];
        match self.command_len(cmd) {
            Some(n) if self.fifo.len() >= n => {
                let words = std::mem::take(&mut self.fifo);
                self.execute(&words);
                self.fifo = words;
                self.fifo.clear();
            }
            None => {
                // Polyline: the first segment is a regular line.
                let shaded = cmd & 0x1000_0000 != 0;
                let n = if shaded { 4 } else { 3 };
                if self.fifo.len() >= n {
                    let words = std::mem::take(&mut self.fifo);
                    let (a, b) = Self::line_vertices(&words);
                    self.draw_line(a, b, shaded, cmd & 0x0200_0000 != 0);
                    self.fifo = words;
                    self.fifo.truncate(1);
                    self.polyline = Some((b, None));
                }
            }
            _ => {}
        }
    }

    fn line_vertices(w: &[u32]) -> (Vertex, Vertex) {
        let shaded = w[0] & 0x1000_0000 != 0;
        let mut a = Vertex::default();
        let mut b = Vertex::default();
        a.color(w[0]);
        a.position(w[1]);
        if shaded {
            b.color(w[2]);
            b.position(w[3]);
        } else {
            b.color(w[0]);
            b.position(w[2]);
        }
        (a, b)
    }

    fn polyline_word(&mut self, v: u32) {
        let Some((prev, color)) = self.polyline.take() else {
            return;
        };
        if v & 0xf000_f000 == 0x5000_5000 {
            self.fifo.clear();
            return;
        }
        let cmd = self.fifo[0];
        let shaded = cmd & 0x1000_0000 != 0;
        if shaded && color.is_none() {
            self.polyline = Some((prev, Some(v)));
            return;
        }
        let mut next = Vertex::default();
        next.color(color.unwrap_or(cmd));
        next.position(v);
        self.draw_line(prev, next, shaded, cmd & 0x0200_0000 != 0);
        self.polyline = Some((next, None));
    }

    fn execute(&mut self, w: &[u32]) {
        let op = w[0] >> 24;
        match op {
            0x01 => {}
            0x02 => self.fill(w),
            0x1f => {
                self.irq = true;
                self.irq_raised = true;
            }
            0x20..=0x3f => self.polygon(w),
            0x40..=0x5f => {
                let (a, b) = Self::line_vertices(w);
                self.draw_line(a, b, op & 0x10 != 0, op & 2 != 0);
            }
            0x60..=0x7f => self.rect(w),
            0x80..=0x9f => self.copy(w),
            0xa0..=0xbf => self.load = Some(Transfer::new(w[1], w[2])),
            0xc0..=0xdf => self.store = Some(Transfer::new(w[1], w[2])),
            0xe1 => {
                self.texpage = w[0] & 0x7ff;
                self.texture_disable = w[0] & 0x800 != 0;
                self.rect_flip_x = w[0] & 0x1000 != 0;
                self.rect_flip_y = w[0] & 0x2000 != 0;
            }
            0xe2 => self.tex_window = w[0] & 0xf_ffff,
            0xe3 => {
                self.area_x1 = (w[0] & 0x3ff) as i32;
                self.area_y1 = ((w[0] >> 10) & 0x1ff) as i32;
            }
            0xe4 => {
                self.area_x2 = (w[0] & 0x3ff) as i32;
                self.area_y2 = ((w[0] >> 10) & 0x1ff) as i32;
            }
            0xe5 => {
                self.offset_x = ((w[0] as i32) << 21) >> 21;
                self.offset_y = (((w[0] >> 11) as i32) << 21) >> 21;
            }
            0xe6 => {
                self.set_mask = w[0] & 1 != 0;
                self.check_mask = w[0] & 2 != 0;
            }
            _ => {}
        }
    }

    // --- VRAM operations -------------------------------------------------

    /// Sets the internal resolution: 1 (native) to 4 times. Drawing then
    /// also happens in a scaled-up copy of VRAM, which the display shows;
    /// the native VRAM stays exact for VRAM reads and textures.
    pub fn set_scale(&mut self, scale: usize) {
        let scale = scale.clamp(1, MAX_SCALE);
        if scale == self.scale && !self.record {
            return;
        }
        self.scale = scale;
        if self.record {
            // The recorder's scaled VRAM starts from the native one.
            self.hires = Box::new([]);
            self.resync();
            return;
        }
        self.hires = if scale == 1 {
            Box::new([])
        } else {
            vec![0; VRAM_WIDTH * VRAM_HEIGHT * scale * scale].into_boxed_slice()
        };
        // Start from an upscaled copy of what is there.
        for y in 0..VRAM_HEIGHT {
            for x in 0..VRAM_WIDTH {
                let v = self.vram[y * VRAM_WIDTH + x];
                self.hires_block(x, y, v, false);
            }
        }
    }

    pub fn scale(&self) -> usize {
        self.scale
    }

    /// Records, for another renderer (a GPU's), what changes the scaled
    /// VRAM (`take_commands`), rather than drawing it here; the native VRAM
    /// is still drawn here (exact, and what games read back). Recording
    /// starts with the whole native VRAM, scaled up.
    pub fn set_recording(&mut self, on: bool) {
        self.sync_vram();
        self.defer = false;
        self.record = on;
        self.commands.clear();
        self.dirty = None;
        let scale = self.scale;
        self.scale = 0;
        self.set_scale(scale);
    }

    pub fn recording(&self) -> bool {
        self.record
    }

    /// Recording, the native VRAM drawn by the recording's renderer too,
    /// rather than here (`Some`): here, each primitive is recorded at
    /// native resolution as well, and only the pixels it covers counted
    /// (they time the GPU); CPU to VRAM transfers and copies are recorded
    /// with their mask checks left to the renderer. The native VRAM here
    /// is brought up to date from the renderer's (`fetch`) when something
    /// reads it: the game (VRAM to CPU transfers), a save state
    /// (`sync_vram`). `None`: drawn here again, from the renderer's.
    pub fn set_deferred(&mut self, fetch: Option<VramFetch>) {
        self.sync_vram();
        let on = fetch.is_some() && self.record;
        self.fetch = fetch;
        if on == self.defer {
            return;
        }
        if on {
            // The renderer's native VRAM from this one, as it is.
            self.dirty = Some([0, 0, VRAM_WIDTH as i32 - 1, VRAM_HEIGHT as i32 - 1]);
            self.send_dirty();
        }
        self.dirty = None;
        self.defer = on;
        self.target = (false, true);
        self.commands.push(Command::Deferred { on });
    }

    pub fn deferred(&self) -> bool {
        self.defer
    }

    /// The renderer's fetch (see `set_deferred`), taken (another GPU
    /// takes over: a state loaded).
    pub(crate) fn take_fetch(&mut self) -> Option<VramFetch> {
        self.fetch.take()
    }

    /// Deferred drawing (see `set_deferred`): the native VRAM brought up to
    /// date, from the renderer's.
    pub fn sync_vram(&mut self) {
        if !self.stale {
            return;
        }
        self.stale = false;
        let commands = std::mem::take(&mut self.commands);
        if let Some(fetch) = &mut self.fetch {
            fetch(commands, &mut self.vram);
        }
    }

    /// Precise geometry, when recording: triangles whose vertices the GTE
    /// projected are recorded with their exact positions and depths too
    /// (`Command::Precise`), for drawing without the console's rounding,
    /// and with perspective-correct textures.
    pub fn set_precise(&mut self, on: bool) {
        self.exact = on.then(|| vec![(0, [f32::NAN; 3], 0); EXACT].into_boxed_slice());
    }

    /// A vertex the GTE projected (see `set_precise`).
    pub fn exact_vertex(&mut self, sxy: u32, exact: [f32; 3]) {
        if let Some(table) = &mut self.exact {
            table[exact_slot(sxy)] = (sxy, exact, self.exact_frame);
        }
    }

    /// The exact projection of a vertex word, if the GTE made one.
    fn exact_of(&self, word: u32) -> Option<[f32; 3]> {
        let (sxy, exact, frame) = self.exact.as_ref()?[exact_slot(word)];
        let fresh = self.exact_frame.wrapping_sub(frame) <= EXACT_FRAMES;
        (sxy == word && fresh && !exact[2].is_nan()).then_some(exact)
    }

    /// The commands recorded since the last call.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// Recording: the whole native VRAM, then a scaled copy of it.
    fn resync(&mut self) {
        // Deferred, the renderer's native VRAM is its own, exact: what was
        // recorded is still to draw there.
        if !self.defer {
            self.commands.clear();
            self.dirty = Some([0, 0, VRAM_WIDTH as i32 - 1, VRAM_HEIGHT as i32 - 1]);
            self.send_dirty();
        }
        self.commands.push(Command::Rescale {
            scale: self.scale as u32,
        });
        // From here, the renderer knows how its native VRAM is kept.
        self.target = (false, true);
        self.commands.push(Command::Deferred { on: self.defer });
    }

    /// Marks a native area as drawn (recording), inclusive.
    fn mark_dirty(&mut self, r: [i32; 4]) {
        if !self.record {
            return;
        }
        self.dirty = Some(match self.dirty {
            None => r,
            Some(d) => [d[0].min(r[0]), d[1].min(r[1]), d[2].max(r[2]), d[3].max(r[3])],
        });
    }

    /// Marks a native rectangle, which may wrap around VRAM, as drawn.
    fn mark_dirty_wrapping(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
        let (x0, x1) = if x + w > VRAM_WIDTH as i32 {
            (0, VRAM_WIDTH as i32 - 1)
        } else {
            (x, x + w - 1)
        };
        let (y0, y1) = if y + h > VRAM_HEIGHT as i32 {
            (0, VRAM_HEIGHT as i32 - 1)
        } else {
            (y, y + h - 1)
        };
        self.mark_dirty([x0, y0, x1, y1]);
    }

    /// Sends the native area drawn since the last time (recording).
    fn send_dirty(&mut self) {
        let Some([x0, y0, x1, y1]) = self.dirty.take() else {
            return;
        };
        let (x0, y0, x1, y1) = (
            x0.max(0) as usize,
            y0.max(0) as usize,
            x1.min(1023) as usize,
            y1.min(511) as usize,
        );
        if x0 > x1 || y0 > y1 {
            return;
        }
        let mut data = Vec::with_capacity((x1 - x0 + 1) * (y1 - y0 + 1));
        for y in y0..=y1 {
            data.extend_from_slice(&self.vram[y * VRAM_WIDTH + x0..=y * VRAM_WIDTH + x1]);
        }
        self.commands.push(Command::Upload {
            x: x0 as u32,
            y: y0 as u32,
            w: (x1 - x0 + 1) as u32,
            h: (y1 - y0 + 1) as u32,
            data,
        });
    }

    /// Sends the native VRAM a textured primitive reads, if drawn since
    /// it was last sent (recording): its texture page and CLUT.
    fn sync_texture(&mut self, mode: &DrawMode) {
        let Some(d) = self.dirty else { return };
        if !mode.textured {
            return;
        }
        let depth = (mode.tpage >> 7) & 3;
        let (base_x, base_y) = (((mode.tpage & 0xf) * 64) as i32, (((mode.tpage >> 4) & 1) * 256) as i32);
        let page_w = match depth {
            0 => 64,
            1 => 128,
            _ => 256,
        };
        let page = [base_x, base_y, (base_x + page_w - 1).min(1023), base_y + 255];
        let clut_w = if depth == 1 { 256 } else { 16 };
        let (cx, cy) = (mode.clut_x as i32, mode.clut_y as i32);
        let clut = [cx, cy, (cx + clut_w - 1).min(1023), cy];
        let overlaps = |r: [i32; 4]| r[0] <= d[2] && d[0] <= r[2] && r[1] <= d[3] && d[1] <= r[3];
        // A page past VRAM's right edge wraps: then anything may be read.
        if overlaps(page) || overlaps(clut) || base_x + page_w > 1024 || cx + clut_w > 1024 {
            self.send_dirty();
        }
    }

    /// Writes one native pixel's block in the scaled VRAM.
    fn hires_block(&mut self, x: usize, y: usize, v: u16, masked: bool) {
        let s = self.scale;
        if s == 1 || self.record {
            return;
        }
        let stride = VRAM_WIDTH * s;
        for dy in 0..s {
            let row = (y * s + dy) * stride + x * s;
            for p in &mut self.hires[row..row + s] {
                if masked && self.check_mask && *p & 0x8000 != 0 {
                    continue;
                }
                *p = v;
            }
        }
    }

    /// A pixel of a CPU to VRAM transfer (`index` in it, recording).
    fn write_pixel_masked(&mut self, x: usize, y: usize, v: u16, transfer: (u32, u32, u32, u32), index: u32) {
        let at = y * VRAM_WIDTH + x;
        // Deferred: the renderer checks the mask (this VRAM is behind).
        let skip = !self.defer && self.check_mask && self.vram[at] & 0x8000 != 0;
        let v = v | (u16::from(self.set_mask) << 15);
        if self.record {
            self.mark_dirty([x as i32, y as i32, x as i32, y as i32]);
            let word = u32::from(v) | (u32::from(skip) << 16);
            let (tx, ty, tw, th) = transfer;
            if let Some(Command::Write {
                x,
                y,
                w,
                h,
                first,
                pixels,
                ..
            }) = self.commands.last_mut()
                && (*x, *y, *w, *h) == (tx, ty, tw, th)
                && *first + pixels.len() as u32 == index
            {
                pixels.push(word);
            } else {
                self.commands.push(Command::Write {
                    x: tx,
                    y: ty,
                    w: tw,
                    h: th,
                    first: index,
                    check_mask: self.check_mask,
                    pixels: vec![word],
                });
            }
        }
        if self.defer {
            self.stale = true;
            return;
        }
        if skip {
            return;
        }
        self.vram[at] = v;
        self.hires_block(x, y, v, true);
    }

    fn fill(&mut self, w: &[u32]) {
        let color = rgb24_to_15(w[0]);
        let x0 = w[1] & 0x3f0;
        let y0 = (w[1] >> 16) & 0x1ff;
        let width = ((w[2] & 0x3ff) + 0xf) & !0xf;
        let height = (w[2] >> 16) & 0x1ff;
        // Deferred: drawn by the renderer only.
        let height_here = if self.defer { 0 } else { height };
        self.stale |= self.defer && width > 0 && height > 0;
        for y in 0..height_here {
            let vy = ((y0 + y) & 0x1ff) as usize;
            for x in 0..width {
                let vx = ((x0 + x) & 0x3ff) as usize;
                self.vram[vy * VRAM_WIDTH + vx] = color;
                self.hires_block(vx, vy, color, false);
            }
        }
        if self.record && width > 0 && height > 0 {
            self.mark_dirty_wrapping(x0, y0, width, height);
            self.commands.push(Command::Fill {
                x: x0,
                y: y0,
                w: width,
                h: height,
                color,
            });
        }
        // Fills write two pixels per GPU cycle.
        self.charge(u64::from(width * height) / 2, 1);
    }

    fn copy(&mut self, w: &[u32]) {
        let mut src = Transfer::new(w[1], w[3]);
        let mut dst = Transfer::new(w[2], w[3]);
        // A read and a write per pixel.
        self.charge(u64::from(src.w * src.h), 2);
        let s = self.scale;
        // Recording: which pixels the mask kept, as the native VRAM was.
        let mut skipped = Vec::new();
        let recorded = (src.x, src.y, dst.x, dst.y, src.w, src.h);
        // Deferred: copied by the renderer only, its mask its own.
        if self.defer {
            self.stale = true;
            src.i = src.w * src.h;
        }
        while let (Some((sx, sy)), Some((dx, dy))) = (src.next(), dst.next()) {
            let v = self.vram[sy * VRAM_WIDTH + sx];
            let p = &mut self.vram[dy * VRAM_WIDTH + dx];
            if self.check_mask && *p & 0x8000 != 0 {
                if self.record {
                    skipped.push(dst.i - 1);
                }
                continue;
            }
            if self.record {
                *p = v | (u16::from(self.set_mask) << 15);
                continue;
            }
            *p = v | (u16::from(self.set_mask) << 15);
            // Copy the scaled block too, keeping its detail.
            let stride = VRAM_WIDTH * s;
            for by in 0..s {
                for bx in 0..s {
                    if s == 1 {
                        break;
                    }
                    let from = (sy * s + by) * stride + sx * s + bx;
                    let to = (dy * s + by) * stride + dx * s + bx;
                    let hv = self.hires[from];
                    if self.check_mask && self.hires[to] & 0x8000 != 0 {
                        continue;
                    }
                    self.hires[to] = hv | (u16::from(self.set_mask) << 15);
                }
            }
        }
        if self.record {
            let (sx, sy, dx, dy, w, h) = recorded;
            self.mark_dirty_wrapping(dx, dy, w, h);
            self.commands.push(Command::Copy {
                sx,
                sy,
                dx,
                dy,
                w,
                h,
                check_mask: self.check_mask,
                set_mask: self.set_mask,
                skipped,
            });
        }
    }

    /// Runs a drawing function on the native VRAM, then on the scaled one.
    /// Returns the pixels drawn (natively: the scaled copy does not change
    /// timing).
    fn draw(&mut self, mode: &DrawMode, f: impl Fn(&mut Raster)) -> u64 {
        let area = (self.area_x1, self.area_y1, self.area_x2, self.area_y2);
        if self.defer {
            return self.draw_deferred(area, f);
        }
        let mut native = Raster {
            target: &mut self.vram,
            tex: None,
            scale: 1,
            stride: VRAM_WIDTH,
            area,
            check_mask: self.check_mask,
            set_mask: self.set_mask,
            tex_window: self.tex_window,
            pixels: 0,
            bounds: None,
            rec: None,
            count: false,
            precise: false,
            both: false,
        };
        f(&mut native);
        let pixels = native.pixels;
        if self.record {
            if let Some(b) = native.bounds {
                self.mark_dirty(b);
            }
            // What the primitive reads, as the native VRAM is now (as the
            // scaled drawing below reads it).
            self.sync_texture(mode);
            let s = self.scale as i32;
            let mut rec = Raster {
                target: &mut [],
                tex: None,
                scale: s,
                stride: VRAM_WIDTH * self.scale,
                area: (area.0 * s, area.1 * s, area.2 * s + s - 1, area.3 * s + s - 1),
                check_mask: self.check_mask,
                set_mask: self.set_mask,
                tex_window: self.tex_window,
                pixels: 0,
                bounds: None,
                rec: Some(&mut self.commands),
                count: false,
                precise: true,
                both: false,
            };
            f(&mut rec);
            return pixels;
        }
        if self.scale > 1 {
            let s = self.scale as i32;
            let mut hires = Raster {
                target: &mut self.hires,
                tex: Some(&self.vram),
                scale: s,
                stride: VRAM_WIDTH * self.scale,
                area: (area.0 * s, area.1 * s, area.2 * s + s - 1, area.3 * s + s - 1),
                check_mask: self.check_mask,
                set_mask: self.set_mask,
                tex_window: self.tex_window,
                pixels: 0,
                bounds: None,
                rec: None,
                count: false,
                precise: false,
                both: false,
            };
            f(&mut hires);
        }
        pixels
    }

    /// Deferred drawing (see `set_deferred`): the primitive recorded for the
    /// renderer's native VRAM (as drawn here, without precise geometry),
    /// its pixels counted as drawing them would; then for its scaled VRAM.
    /// At native resolution, the same, once for both (but precise
    /// triangles: see `Raster::both`).
    fn draw_deferred(&mut self, area: (i32, i32, i32, i32), f: impl Fn(&mut Raster)) -> u64 {
        self.stale = true;
        let both = self.scale == 1;
        self.retarget(true, both);
        let mut native = Raster {
            target: &mut [],
            tex: None,
            scale: 1,
            stride: VRAM_WIDTH,
            area,
            check_mask: self.check_mask,
            set_mask: self.set_mask,
            tex_window: self.tex_window,
            pixels: 0,
            bounds: None,
            rec: Some(&mut self.commands),
            count: true,
            precise: both,
            both,
        };
        f(&mut native);
        let pixels = native.pixels;
        if both {
            return pixels;
        }
        self.retarget(false, true);
        let s = self.scale as i32;
        let mut rec = Raster {
            target: &mut [],
            tex: None,
            scale: s,
            stride: VRAM_WIDTH * self.scale,
            area: (area.0 * s, area.1 * s, area.2 * s + s - 1, area.3 * s + s - 1),
            check_mask: self.check_mask,
            set_mask: self.set_mask,
            tex_window: self.tex_window,
            pixels: 0,
            bounds: None,
            rec: Some(&mut self.commands),
            count: false,
            precise: true,
            both: false,
        };
        f(&mut rec);
        pixels
    }

    /// Deferred: the VRAMs the primitives recorded next are for.
    fn retarget(&mut self, native: bool, scaled: bool) {
        if self.target != (native, scaled) {
            self.target = (native, scaled);
            self.commands.push(Command::Target { native, scaled });
        }
    }

    /// Charges GPU work: `pixels` drawn at `per_pixel` GPU cycles each, plus
    /// the setup of a primitive.
    fn charge(&mut self, pixels: u64, per_pixel: u64) {
        const SETUP: u64 = 16;
        self.busy = self.busy.saturating_add(SETUP + pixels * per_pixel);
    }

    /// The drawing sent so far joins what is under way (the GPU clock is
    /// 11/7 of the CPU's).
    fn queue_drawing(&mut self) {
        let gpu = std::mem::take(&mut self.busy);
        self.drawing = self.drawing.saturating_add(gpu * 7 / 11);
    }

    /// The CPU cycles of drawing still under way (0: the GPU is idle).
    pub fn drawing(&mut self) -> u64 {
        self.queue_drawing();
        self.drawing
    }

    /// Waits for the drawing under way: returns the CPU cycles that takes
    /// (the CPU stalls), and the GPU is idle after.
    pub fn wait_drawing(&mut self) -> u32 {
        self.queue_drawing();
        std::mem::take(&mut self.drawing).min(u64::from(u32::MAX)) as u32
    }

    // --- Primitives ------------------------------------------------------

    fn polygon(&mut self, w: &[u32]) {
        let op = w[0] >> 24;
        let quad = op & 8 != 0;
        let textured = op & 4 != 0;
        let shaded = op & 0x10 != 0;
        let n = if quad { 4 } else { 3 };
        let mut v = [Vertex::default(); 4];
        let mut i = 1;
        let mut clut = 0;
        let mut tpage = self.texpage;
        for (k, vert) in v.iter_mut().enumerate().take(n) {
            if k > 0 && shaded {
                vert.color(w[i]);
                i += 1;
            } else {
                vert.color(w[0]);
            }
            vert.position(w[i]);
            if self.record {
                vert.exact = self.exact_of(w[i]);
            }
            i += 1;
            if textured {
                vert.uv(w[i]);
                match k {
                    0 => clut = w[i] >> 16,
                    1 => tpage = (w[i] >> 16) & 0x9ff,
                    _ => {}
                }
                i += 1;
            }
        }
        if textured {
            // The polygon's texpage becomes the current one.
            self.texpage = (self.texpage & !0x9ff) | (tpage & 0x1ff);
        }
        let mode = DrawMode {
            shaded,
            textured,
            semi: op & 2 != 0,
            raw: op & 1 != 0,
            dither: self.texpage & 0x200 != 0 && (shaded || (textured && op & 1 == 0)),
            tpage: self.texpage,
            clut_x: ((clut & 0x3f) * 16) as usize,
            clut_y: ((clut >> 6) & 0x1ff) as usize,
        };
        for vert in v.iter_mut().take(n) {
            vert.x += self.offset_x;
            vert.y += self.offset_y;
            if let Some(e) = &mut vert.exact {
                e[0] += self.offset_x as f32;
                e[1] += self.offset_y as f32;
            }
        }
        let pixels = self.draw(&mode, |r| {
            r.triangle([v[0], v[1], v[2]], &mode);
            if quad {
                r.triangle([v[1], v[2], v[3]], &mode);
            }
        });
        self.charge(pixels, mode.cost());
    }

    fn rect(&mut self, w: &[u32]) {
        let op = w[0] >> 24;
        let textured = op & 4 != 0;
        let mut v = Vertex::default();
        v.color(w[0]);
        v.position(w[1]);
        let mut i = 2;
        let mut clut = 0;
        if textured {
            v.uv(w[2]);
            clut = w[2] >> 16;
            i = 3;
        }
        let (width, height) = match (op >> 3) & 3 {
            0 => ((w[i] & 0x3ff) as i32, ((w[i] >> 16) & 0x1ff) as i32),
            1 => (1, 1),
            2 => (8, 8),
            _ => (16, 16),
        };
        let mode = DrawMode {
            shaded: false,
            textured,
            semi: op & 2 != 0,
            raw: op & 1 != 0,
            dither: false,
            tpage: self.texpage,
            clut_x: ((clut & 0x3f) * 16) as usize,
            clut_y: ((clut >> 6) & 0x1ff) as usize,
        };
        v.x += self.offset_x;
        v.y += self.offset_y;
        let flip = (self.rect_flip_x, self.rect_flip_y);
        let pixels = self.draw(&mode, |r| r.rect(v, width, height, flip, &mode));
        self.charge(pixels, mode.cost());
    }

    fn draw_line(&mut self, mut a: Vertex, mut b: Vertex, shaded: bool, semi: bool) {
        a.x += self.offset_x;
        a.y += self.offset_y;
        b.x += self.offset_x;
        b.y += self.offset_y;
        if (b.x - a.x).abs() >= 1024 || (b.y - a.y).abs() >= 512 {
            return;
        }
        let mode = DrawMode {
            shaded,
            semi,
            dither: shaded && self.texpage & 0x200 != 0,
            tpage: self.texpage,
            ..DrawMode::default()
        };
        let pixels = self.draw(&mode, |r| r.line(a, b, &mode));
        self.charge(pixels, mode.cost());
    }

    // --- Timing and output ----------------------------------------------

    fn lines_per_frame(&self) -> u32 {
        if self.is_pal() { 314 } else { 263 }
    }

    /// The CPU cycles until `lines` more lines have started (see `tick`).
    pub fn cycles_to_lines(&self, lines: u32) -> u32 {
        let line_len: u32 = if self.is_pal() { 3406 } else { 3413 } * 7;
        let need = (u64::from(lines) * u64::from(line_len)).saturating_sub(u64::from(self.line_frac));
        need.div_ceil(11).clamp(1, u64::from(u32::MAX)) as u32
    }

    /// The CPU cycles until the vertical blank starts (see `tick`).
    pub fn cycles_to_vblank(&self) -> u32 {
        let lines = self.lines_per_frame();
        let vblank = self.v_range.1.clamp(self.v_range.0 + 1, lines - 1);
        let s = self.scanline;
        let to = if s < vblank {
            vblank - s
        } else if s < lines {
            lines - s + vblank
        } else {
            1 + vblank
        };
        self.cycles_to_lines(to)
    }

    /// Advances video timing by `cycles` CPU cycles.
    pub fn tick(&mut self, cycles: u32) -> GpuTick {
        self.queue_drawing();
        self.drawing = self.drawing.saturating_sub(u64::from(cycles));
        let line_len = if self.is_pal() { 3406 } else { 3413 } * 7;
        self.line_frac += cycles * 11;
        let mut out = GpuTick {
            dot_divider: self.dot_divider(),
            ..GpuTick::default()
        };
        while self.line_frac >= line_len {
            self.line_frac -= line_len;
            out.hblanks += 1;
            self.scanline += 1;
            let vblank_line = self.v_range.1.clamp(self.v_range.0 + 1, self.lines_per_frame() - 1);
            if self.scanline == vblank_line {
                out.vblank_start = true;
                self.frame_ready = true;
                self.exact_frame = self.exact_frame.wrapping_add(1);
            }
            if self.scanline >= self.lines_per_frame() {
                self.scanline = 0;
                self.field = !self.field;
            }
        }
        out
    }

    pub fn debug_display(&self) -> String {
        format!(
            "start=({},{}) h={:?} v={:?} mode={:02x} disabled={} area=({},{})-({},{}) offset=({},{})",
            self.display_x,
            self.display_y,
            self.h_range,
            self.v_range,
            self.display_mode,
            self.display_disabled,
            self.area_x1,
            self.area_y1,
            self.area_x2,
            self.area_y2,
            self.offset_x,
            self.offset_y
        )
    }

    /// The displayed picture size in pixels.
    /// The displayed picture, for another renderer.
    pub fn display(&self) -> Display {
        let (w, h) = self.display_size();
        Display {
            x: self.display_x,
            y: self.display_y,
            w: w as u32,
            h: h as u32,
            depth24: self.display_mode & 0x10 != 0,
            off: self.display_disabled,
        }
    }

    pub fn display_size(&self) -> (usize, usize) {
        let width = if self.display_mode & 0x40 != 0 {
            368
        } else {
            [256, 320, 512, 640][(self.display_mode & 3) as usize]
        };
        let lines = self
            .v_range
            .1
            .saturating_sub(self.v_range.0)
            .clamp(1, if self.is_pal() { 288 } else { 240 });
        let double = self.interlaced() && self.display_mode & 4 != 0;
        (width, (lines * if double { 2 } else { 1 }) as usize)
    }

    /// Renders the displayed area of VRAM as RGBA8888 into `out`; returns
    /// its size: the display size times the internal resolution, except in
    /// 24-bit mode (FMVs), which only exists at native resolution.
    pub fn render_display(&self, out: &mut Vec<u8>) -> (usize, usize) {
        if self.record {
            // The recorder's renderer shows the picture.
            let d = self.display();
            let s = if d.depth24 { 1 } else { self.scale };
            out.clear();
            return (d.w as usize * s, d.h as usize * s);
        }
        let src = if self.scale == 1 {
            &self.vram[..]
        } else {
            &self.hires[..]
        };
        display_rgba(self.display(), self.scale, src, &self.vram, out)
    }

    /// The displayed picture's size: what `render_display` gives.
    pub fn display_dims(&self) -> (usize, usize) {
        let d = self.display();
        let s = if d.depth24 { 1 } else { self.scale };
        (d.w as usize * s, d.h as usize * s)
    }

    /// The displayed picture as VRAM holds it, for a renderer that converts
    /// its colours itself (see `display_15`); `None` while drawing is
    /// recorded (the recording's renderer shows it).
    pub fn picture<'a>(&'a self, wrapped: &'a mut Vec<u16>) -> Option<Picture<'a>> {
        if self.record {
            return None;
        }
        let src = if self.scale == 1 {
            &self.vram[..]
        } else {
            &self.hires[..]
        };
        display_15(self.display(), self.scale, src, wrapped)
    }
}

/// A drawing target: the native VRAM, or its scaled-up copy. Coordinates
/// are in target pixels; primitives come in native coordinates and are
/// scaled here.
struct Raster<'a> {
    target: &'a mut [u16],
    /// Where textures and CLUTs are read from: the native VRAM. `None`
    /// when drawing into the native VRAM itself.
    tex: Option<&'a [u16]>,
    scale: i32,
    stride: usize,
    /// Drawing area, inclusive, in target pixels.
    area: (i32, i32, i32, i32),
    check_mask: bool,
    set_mask: bool,
    tex_window: u32,
    /// Pixels considered (inside the drawing area), for GPU timing.
    pixels: u64,
    /// The area drawn (inclusive), for recording.
    bounds: Option<[i32; 4]>,
    /// Recording: primitives go there, set up, rather than drawn.
    rec: Option<&'a mut Vec<Command>>,
    /// Pixels counted, not drawn (recording, see `Gpu::set_deferred`).
    count: bool,
    /// Recording: triangles from the GTE's exact projections recorded as
    /// such (`Command::Precise`).
    precise: bool,
    /// Recording for both VRAMs, at native resolution (see
    /// `Gpu::draw_deferred`): a precise triangle is recorded for each
    /// apart, the native one as the console draws it.
    both: bool,
}

impl Raster<'_> {
    fn in_area(&self, x: i32, y: i32) -> bool {
        x >= self.area.0 && x <= self.area.2 && y >= self.area.1 && y <= self.area.3
    }

    /// Notes an area drawn (inclusive).
    fn touch(&mut self, r: [i32; 4]) {
        self.bounds = Some(match self.bounds {
            None => r,
            Some(b) => [b[0].min(r[0]), b[1].min(r[1]), b[2].max(r[2]), b[3].max(r[3])],
        });
    }

    fn triangle(&mut self, v: [Vertex; 3], mode: &DrawMode) {
        let prim = Prim::new(mode, self.tex_window);
        self.triangle_prim(v, prim, mode.textured, mode.shaded);
    }

    /// A triangle (native coordinates), its pixels' mode worked out.
    fn triangle_prim(&mut self, mut v: [Vertex; 3], prim: Prim, textured: bool, shaded: bool) {
        // As given (deferred drawing records it so: see `Command::Vertices`).
        let given = v;
        let min_x = v.iter().map(|p| p.x).min().unwrap_or(0);
        let max_x = v.iter().map(|p| p.x).max().unwrap_or(0);
        let min_y = v.iter().map(|p| p.y).min().unwrap_or(0);
        let max_y = v.iter().map(|p| p.y).max().unwrap_or(0);
        if max_x - min_x >= 1024 || max_y - min_y >= 512 {
            return;
        }
        let s = self.scale;
        for p in &mut v {
            p.x *= s;
            p.y *= s;
        }
        let edge = |a: &Vertex, b: &Vertex, x: i32, y: i32| -> i64 {
            i64::from(b.x - a.x) * i64::from(y - a.y) - i64::from(b.y - a.y) * i64::from(x - a.x)
        };
        let area = edge(&v[0], &v[1], v[2].x, v[2].y);
        if area == 0 {
            return;
        }
        if area < 0 {
            v.swap(1, 2);
        }
        let xs = (min_x * s).max(self.area.0);
        let xe = (max_x * s).min(self.area.2);
        let ys = (min_y * s).max(self.area.1);
        let ye = (max_y * s).min(self.area.3);
        if xs > xe || ys > ye {
            return;
        }

        // Attribute plane equations in 16.16 fixed point:
        // a(x, y) = a0 + dadx * (x - x0) + dady * (y - y0).
        let (x0, y0) = (v[0].x, v[0].y);
        let (dx1, dy1) = (i64::from(v[1].x - x0), i64::from(v[1].y - y0));
        let (dx2, dy2) = (i64::from(v[2].x - x0), i64::from(v[2].y - y0));
        let det = dx1 * dy2 - dx2 * dy1;
        let plane = |a0: i32, a1: i32, a2: i32| -> (i64, i64, i64) {
            let (d1, d2) = (i64::from(a1 - a0), i64::from(a2 - a0));
            let dadx = ((d1 * dy2 - d2 * dy1) << 16) / det;
            let dady = ((d2 * dx1 - d1 * dx2) << 16) / det;
            ((i64::from(a0) << 16) + 0x8000, dadx, dady)
        };
        let pr = plane(v[0].r, v[1].r, v[2].r);
        let pg = plane(v[0].g, v[1].g, v[2].g);
        let pb = plane(v[0].b, v[1].b, v[2].b);
        let pu = plane(v[0].u, v[1].u, v[2].u);
        let pv = plane(v[0].v, v[1].v, v[2].v);

        // Top-left fill rule: pixels on right and bottom edges are not drawn.
        // With this winding (positive area, y down), top edges go right and
        // left edges go up.
        let bias = |a: &Vertex, b: &Vertex| -> i64 {
            let top_left = (a.y == b.y && b.x > a.x) || b.y < a.y;
            if top_left { 0 } else { -1 }
        };
        let b0 = bias(&v[1], &v[2]);
        let b1 = bias(&v[2], &v[0]);
        let b2 = bias(&v[0], &v[1]);
        let (flat_r, flat_g, flat_b) = (v[0].r, v[0].g, v[0].b);
        // Everything is linear in x and y: evaluated once per row, then
        // stepped by additions across it.
        let steps = |a: &Vertex, b: &Vertex| (-i64::from(b.y - a.y), i64::from(b.x - a.x));
        let (e0, e1, e2) = (steps(&v[1], &v[2]), steps(&v[2], &v[0]), steps(&v[0], &v[1]));
        let row = [
            edge(&v[1], &v[2], xs, ys) + b0,
            edge(&v[2], &v[0], xs, ys) + b1,
            edge(&v[0], &v[1], xs, ys) + b2,
        ];
        let start = |p: (i64, i64, i64)| p.0 + p.1 * i64::from(xs - x0) + p.2 * i64::from(ys - y0);
        let planes = [pr, pg, pb, pu, pv];
        let attr_row = planes.map(start);
        let spans = Spans {
            row,
            steps: [e0, e1, e2],
            attr_row,
            planes,
            flat: (flat_r, flat_g, flat_b),
            xs,
            xe,
            ys,
            ye,
        };
        if let Some(rec) = &mut self.rec {
            let pixel = PixelMode {
                prim,
                textured,
                shaded,
                check_mask: self.check_mask,
                set_mask: self.set_mask,
                scale: self.scale,
            };
            let precise = if self.precise {
                precise_triangle(&v, s, self.area, pixel)
            } else {
                None
            };
            // Deferred, at native resolution: the vertices, for the
            // renderer to set the triangle up (fewer numbers than its
            // spans, which take a while to encode).
            let plain = if self.count {
                Command::Vertices {
                    pixel,
                    v: given.map(|p| [p.x, p.y, p.r, p.g, p.b, p.u, p.v]),
                    area: [self.area.0, self.area.1, self.area.2, self.area.3],
                }
            } else {
                Command::Triangle { pixel, spans }
            };
            match precise {
                Some(p) if self.both => rec.extend([
                    Command::Target { native: true, scaled: false },
                    plain,
                    Command::Target { native: false, scaled: true },
                    p,
                    Command::Target { native: true, scaled: true },
                ]),
                Some(p) => rec.push(p),
                None => rec.push(plain),
            }
            if self.count {
                self.count_spans(&spans);
            }
            return;
        }
        // Loops specialised for what the primitive interpolates.
        match (textured, shaded) {
            (true, true) => self.triangle_spans::<true, true>(spans, &prim),
            (true, false) => self.triangle_spans::<true, false>(spans, &prim),
            (false, true) => self.triangle_spans::<false, true>(spans, &prim),
            (false, false) => self.triangle_spans::<false, false>(spans, &prim),
        }
    }

    /// A triangle's rows: on each, the span of pixels inside all three
    /// edges, solved from the edge functions (exactly the pixels whose
    /// functions are non-negative), then drawn.
    fn triangle_spans<const TEX: bool, const SHADED: bool>(&mut self, sp: Spans, prim: &Prim) {
        let Spans {
            mut row,
            steps,
            mut attr_row,
            planes,
            flat,
            xs,
            xe,
            ys,
            ye,
        } = sp;
        let n = i64::from(xe - xs);
        for y in ys..=ye {
            let (lo, hi) = span(&row, &steps, n);
            if lo <= hi {
                let mut attr = attr_row;
                for (a, p) in attr.iter_mut().zip(&planes) {
                    *a += p.1 * lo;
                }
                let base = y as usize * self.stride;
                let dither_row = self.dither_row(y, prim);
                self.pixels += (hi - lo + 1) as u64;
                self.touch([xs + lo as i32, y, xs + hi as i32, y]);
                let first = xs + lo as i32;
                let mut steps = DitherSteps::new(first, self.scale);
                if !TEX && !SHADED && !prim.semi && !self.check_mask {
                    // One colour, or four (dithered): the row filled, as
                    // `pixel` would (vectorised where it can be).
                    let colour = |d: i32| {
                        let c = |v: i32| ((v + d).clamp(0, 255) >> 3) as u16;
                        c(flat.0) | (c(flat.1) << 5) | (c(flat.2) << 10) | (u16::from(self.set_mask) << 15)
                    };
                    let row = &mut self.target[base + first as usize..=base + (xs + hi as i32) as usize];
                    match dither_row {
                        None => row.fill(colour(0)),
                        Some(d) => {
                            let colours = [colour(d[0]), colour(d[1]), colour(d[2]), colour(d[3])];
                            for p in row {
                                *p = colours[steps.column()];
                                steps.step();
                            }
                        }
                    }
                } else {
                    // The last texel, reused while the coordinates stay (the
                    // scaled pass, reading the native VRAM it does not write).
                    let mut last: Option<(i32, i32, u16)> = None;
                    let reuse = self.tex.is_some();
                    for x in first..=xs + hi as i32 {
                        let (r, g, b) = if SHADED {
                            ((attr[0] >> 16) as i32, (attr[1] >> 16) as i32, (attr[2] >> 16) as i32)
                        } else {
                            flat
                        };
                        let (u, v) = if TEX {
                            ((attr[3] >> 16) as i32, (attr[4] >> 16) as i32)
                        } else {
                            (0, 0)
                        };
                        let dither = dither_row.map_or(0, |d| d[steps.column()]);
                        steps.step();
                        let t = if TEX {
                            self.texel_reusing(&mut last, reuse, u, v, prim)
                        } else {
                            0
                        };
                        self.pixel::<TEX>(base + x as usize, dither, r, g, b, t, prim);
                        if SHADED {
                            for k in 0..3 {
                                attr[k] += planes[k].1;
                            }
                        }
                        if TEX {
                            attr[3] += planes[3].1;
                            attr[4] += planes[4].1;
                        }
                    }
                }
            }
            for (w, (_, dy)) in row.iter_mut().zip(&steps) {
                *w += dy;
            }
            for (a, p) in attr_row.iter_mut().zip(&planes) {
                *a += p.2;
            }
        }
    }

    /// A triangle's pixels, counted (see `triangle_spans`).
    fn count_spans(&mut self, sp: &Spans) {
        let n = i64::from(sp.xe - sp.xs);
        let mut row = sp.row;
        for _ in sp.ys..=sp.ye {
            let (lo, hi) = span(&row, &sp.steps, n);
            if lo <= hi {
                self.pixels += (hi - lo + 1) as u64;
            }
            for (w, (_, dy)) in row.iter_mut().zip(&sp.steps) {
                *w += dy;
            }
        }
    }

    fn rect(&mut self, v: Vertex, width: i32, height: i32, flip: (bool, bool), mode: &DrawMode) {
        let s = self.scale;
        let (x0, y0) = (v.x * s, v.y * s);
        let xs = x0.max(self.area.0);
        let ys = y0.max(self.area.1);
        let xe = (x0 + width * s - 1).min(self.area.2);
        let ye = (y0 + height * s - 1).min(self.area.3);
        if xs > xe {
            return;
        }
        let prim = Prim::new(mode, self.tex_window);
        if let Some(rec) = &mut self.rec {
            rec.push(Command::Rect {
                pixel: PixelMode::new(mode, prim, self.check_mask, self.set_mask, s),
                x0,
                y0,
                xs,
                xe,
                ys,
                ye,
                u: v.u,
                v: v.v,
                color: (v.r, v.g, v.b),
                flip,
            });
            if self.count && ys <= ye {
                self.pixels += ((ye - ys + 1) * (xe - xs + 1)) as u64;
            }
            return;
        }
        self.touch([xs, ys, xe, ye]);
        if mode.textured {
            self.rect_rows::<true>(v, (x0, y0), (xs, xe, ys, ye), flip, &prim);
        } else {
            self.rect_rows::<false>(v, (x0, y0), (xs, xe, ys, ye), flip, &prim);
        }
    }

    /// A rectangle's pixels: each native texel covers `scale` pixels, so
    /// texture coordinates step every `scale` pixels.
    fn rect_rows<const TEX: bool>(
        &mut self,
        v: Vertex,
        (x0, y0): (i32, i32),
        (xs, xe, ys, ye): (i32, i32, i32, i32),
        flip: (bool, bool),
        prim: &Prim,
    ) {
        let s = self.scale;
        let du_step = if flip.0 { -1 } else { 1 };
        for y in ys..=ye {
            let dv = (y - y0) / s;
            let tv = if flip.1 { v.v - dv } else { v.v + dv };
            let base = y as usize * self.stride;
            let dither_row = self.dither_row(y, prim);
            self.pixels += (xe - xs + 1) as u64;
            let (mut tu, mut sub) = {
                let d = xs - x0;
                (v.u + du_step * (d / s), d % s)
            };
            let mut steps = DitherSteps::new(xs, s);
            let mut last: Option<(i32, i32, u16)> = None;
            let reuse = self.tex.is_some();
            for x in xs..=xe {
                let dither = dither_row.map_or(0, |d| d[steps.column()]);
                steps.step();
                let t = if TEX {
                    self.texel_reusing(&mut last, reuse, tu, tv, prim)
                } else {
                    0
                };
                self.pixel::<TEX>(base + x as usize, dither, v.r, v.g, v.b, t, prim);
                sub += 1;
                if sub == s {
                    sub = 0;
                    tu += du_step;
                }
            }
        }
    }

    fn line(&mut self, a: Vertex, b: Vertex, mode: &DrawMode) {
        let s = self.scale;
        let (ax, ay, bx, by) = (a.x * s, a.y * s, b.x * s, b.y * s);
        if let Some(rec) = &mut self.rec {
            let prim = Prim::new(mode, self.tex_window);
            rec.push(Command::Line {
                pixel: PixelMode::new(mode, prim, self.check_mask, self.set_mask, s),
                a: (ax, ay, a.r, a.g, a.b),
                b: (bx, by, b.r, b.g, b.b),
                area: [self.area.0, self.area.1, self.area.2, self.area.3],
            });
            if self.count {
                let prim = Prim::new(mode, self.tex_window);
                self.line_scaled((ax, ay, a.r, a.g, a.b), (bx, by, b.r, b.g, b.b), mode.shaded, &prim);
            }
            return;
        }
        let prim = Prim::new(mode, self.tex_window);
        self.line_scaled((ax, ay, a.r, a.g, a.b), (bx, by, b.r, b.g, b.b), mode.shaded, &prim);
    }

    /// A line between scaled ends (x, y, colour), as `scale`-pixel blocks
    /// (lines keep their native thickness).
    fn line_scaled(&mut self, a: (i32, i32, i32, i32, i32), b: (i32, i32, i32, i32, i32), shaded: bool, prim: &Prim) {
        let s = self.scale;
        let (ax, ay, bx, by) = (a.0, a.1, b.0, b.1);
        let steps = (bx - ax).abs().max((by - ay).abs());
        for i in 0..=steps {
            let lerp = |p: i32, q: i32| {
                if steps == 0 {
                    p
                } else {
                    p + ((q - p) * i + steps / 2 * (q - p).signum()) / steps
                }
            };
            let (x, y) = (lerp(ax, bx), lerp(ay, by));
            let (r, g, bl) = if shaded {
                (lerp(a.2, b.2), lerp(a.3, b.3), lerp(a.4, b.4))
            } else {
                (a.2, a.3, a.4)
            };
            for dy in 0..s {
                for dx in 0..s {
                    self.shade(x + dx, y + dy, r, g, bl, prim);
                }
            }
        }
    }

    fn vram(&self, i: usize) -> u16 {
        match self.tex {
            Some(t) => t[i],
            None => self.target[i],
        }
    }

    /// A texel, at texture coordinates (before the texture window).
    #[inline(always)]
    fn texel(&self, u: i32, v: i32, p: &Prim) -> u16 {
        let u = ((u & 0xff) & !p.mask_x) | p.off_x;
        let v = ((v & 0xff) & !p.mask_y) | p.off_y;
        let row = ((p.base_y + v as usize) & 0x1ff) * VRAM_WIDTH;
        let u = u as usize;
        match p.depth {
            0 => {
                let word = self.vram(row + ((p.base_x + u / 4) & 0x3ff));
                let idx = usize::from((word >> ((u & 3) * 4)) & 0xf);
                self.vram(p.clut_row + ((p.clut_x + idx) & 0x3ff))
            }
            1 => {
                let word = self.vram(row + ((p.base_x + u / 2) & 0x3ff));
                let idx = usize::from((word >> ((u & 1) * 8)) & 0xff);
                self.vram(p.clut_row + ((p.clut_x + idx) & 0x3ff))
            }
            _ => self.vram(row + ((p.base_x + u) & 0x3ff)),
        }
    }

    /// The texel at `u, v`; `last`: the previous one, reused when at the
    /// same coordinates and `reuse` (textures not changing while drawn).
    #[inline(always)]
    /// A precise triangle (`Command::Precise`), as the GPU renderer draws
    /// it (`crates/ps1-gpu`'s `exact_triangle`): float edge functions at
    /// the pixels' corners with the console's top-left rule, colours
    /// interpolated linearly, texture coordinates in perspective when every
    /// vertex has a depth (else affine); then as any pixel.
    fn exact_triangle(&mut self, v: &[ExactVertex; 3], bbox: [i32; 4], textured: bool, shaded: bool, prim: &Prim) {
        let [(x0, y0), (x1, y1), (x2, y2)] = v.map(|p| (p.x, p.y));
        // The edge function of a to b at (x, y), less a little when the
        // edge is not a top or left one (its pixels are the neighbour's).
        let edge = |ax: f32, ay: f32, bx: f32, by: f32, x: f32, y: f32| {
            let e = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
            let top_left = (ay == by && bx > ax) || by < ay;
            if top_left { e >= 0.0 } else { e > 0.0 }
        };
        let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        let z = v.map(|p| p.z);
        let perspective = z.iter().all(|&z| z > 0.0);
        let mix = |l: [f32; 3], a: [f32; 3]| l[0] * a[0] + l[1] * a[1] + l[2] * a[2];
        let channel = |f: fn(&ExactVertex) -> i32| v.each_ref().map(|p| f(p) as f32);
        let (cr, cg, cb) = (channel(|p| p.color.0), channel(|p| p.color.1), channel(|p| p.color.2));
        let (us, vs) = (channel(|p| p.uv.0), channel(|p| p.uv.1));
        let reuse = self.tex.is_some();
        let mut last = None;
        for y in bbox[1]..=bbox[3] {
            let base = y as usize * self.stride;
            let dither_row = self.dither_row(y, prim);
            let py = y as f32;
            for x in bbox[0]..=bbox[2] {
                let px = x as f32;
                if !edge(x1, y1, x2, y2, px, py) || !edge(x2, y2, x0, y0, px, py) || !edge(x0, y0, x1, y1, px, py) {
                    continue;
                }
                let l = [
                    ((x2 - x1) * (py - y1) - (y2 - y1) * (px - x1)) / area,
                    ((x0 - x2) * (py - y2) - (y0 - y2) * (px - x2)) / area,
                    ((x1 - x0) * (py - y0) - (y1 - y0) * (px - x0)) / area,
                ];
                let (r, g, b) = if shaded {
                    let round = |c: [f32; 3]| (mix(l, c) + 0.5).floor() as i32;
                    (round(cr), round(cg), round(cb))
                } else {
                    v[0].color
                };
                self.pixels += 1;
                self.touch([x, y, x, y]);
                let dither = dither_row.map_or(0, |d| d[((x / self.scale) & 3) as usize]);
                if textured {
                    let q = if perspective {
                        [l[0] / z[0], l[1] / z[1], l[2] / z[2]]
                    } else {
                        l
                    };
                    let den = q[0] + q[1] + q[2];
                    let u = (mix(q, us) / den + 0.5).floor() as i32;
                    let tv = (mix(q, vs) / den + 0.5).floor() as i32;
                    let t = self.texel_reusing(&mut last, reuse, u, tv, prim);
                    self.pixel::<true>(base + x as usize, dither, r, g, b, t, prim);
                } else {
                    self.pixel::<false>(base + x as usize, dither, r, g, b, 0, prim);
                }
            }
        }
    }

    fn texel_reusing(&self, last: &mut Option<(i32, i32, u16)>, reuse: bool, u: i32, v: i32, p: &Prim) -> u16 {
        if let Some((lu, lv, t)) = *last
            && (lu, lv) == (u, v)
        {
            return t;
        }
        let t = self.texel(u, v, p);
        if reuse {
            *last = Some((u, v, t));
        }
        t
    }

    /// The dither offsets of a row (native rows: dithering follows native
    /// pixels), when the primitive dithers.
    fn dither_row(&self, y: i32, p: &Prim) -> Option<&'static [i32; 4]> {
        p.dither.then(|| &DITHER[((y / self.scale) & 3) as usize])
    }

    /// A line's pixel at `x, y` (anything may fall outside the drawing
    /// area; lines are not textured).
    fn shade(&mut self, x: i32, y: i32, r: i32, g: i32, b: i32, prim: &Prim) {
        if !self.in_area(x, y) {
            return;
        }
        self.pixels += 1;
        if self.count {
            return;
        }
        self.touch([x, y, x, y]);
        let idx = y as usize * self.stride + x as usize;
        let dither = self
            .dither_row(y, prim)
            .map_or(0, |d| d[((x / self.scale) & 3) as usize]);
        self.pixel::<false>(idx, dither, r, g, b, 0, prim);
    }

    /// A pixel inside the drawing area, at `idx` of the target: its dither
    /// offset, colour and texel (`TEX`).
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn pixel<const TEX: bool>(&mut self, idx: usize, dither: i32, r: i32, g: i32, b: i32, t: u16, p: &Prim) {
        let dst = self.target[idx];
        if self.check_mask && dst & 0x8000 != 0 {
            return;
        }
        let (mut cr, mut cg, mut cb, semi, mask_bit);
        if TEX {
            if t == 0 {
                return;
            }
            let (tr, tg, tb) = (
                i32::from(t & 0x1f),
                i32::from((t >> 5) & 0x1f),
                i32::from((t >> 10) & 0x1f),
            );
            if p.raw {
                (cr, cg, cb) = (tr, tg, tb);
            } else {
                let m = |tc: i32, c: i32| ((((tc << 3) * c) >> 7) + dither).clamp(0, 255) >> 3;
                (cr, cg, cb) = (m(tr, r), m(tg, g), m(tb, b));
            }
            semi = p.semi && t & 0x8000 != 0;
            mask_bit = t & 0x8000;
        } else {
            let d = |c: i32| (c + dither).clamp(0, 255) >> 3;
            (cr, cg, cb) = (d(r), d(g), d(b));
            semi = p.semi;
            mask_bit = 0;
        }
        if semi {
            let (br, bg, bb) = (
                i32::from(dst & 0x1f),
                i32::from((dst >> 5) & 0x1f),
                i32::from((dst >> 10) & 0x1f),
            );
            let f = |b: i32, f: i32| -> i32 {
                match p.blend {
                    0 => (b + f) >> 1,
                    1 => (b + f).min(31),
                    2 => (b - f).max(0),
                    _ => (b + (f >> 2)).min(31),
                }
            };
            (cr, cg, cb) = (f(br, cr), f(bg, cg), f(bb, cb));
        }
        self.target[idx] =
            (cr as u16) | ((cg as u16) << 5) | ((cb as u16) << 10) | mask_bit | (u16::from(self.set_mask) << 15);
    }
}

/// The dither column along a row: native pixels' (x / scale, of 4),
/// stepped rather than divided.
struct DitherSteps {
    column: usize,
    sub: i32,
    scale: i32,
}

impl DitherSteps {
    /// From column `x` (non-negative).
    fn new(x: i32, scale: i32) -> Self {
        Self {
            column: ((x / scale) & 3) as usize,
            sub: x % scale,
            scale,
        }
    }

    #[inline(always)]
    fn column(&self) -> usize {
        self.column
    }

    #[inline(always)]
    fn step(&mut self) {
        self.sub += 1;
        if self.sub == self.scale {
            self.sub = 0;
            self.column = (self.column + 1) & 3;
        }
    }
}

/// What drawing a primitive's pixels needs, worked out once per primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Prim {
    pub raw: bool,
    pub semi: bool,
    pub dither: bool,
    /// Semi-transparency mode (0-3).
    pub blend: u32,
    /// Texture window: the bits replaced, and what replaces them.
    pub mask_x: i32,
    pub mask_y: i32,
    pub off_x: i32,
    pub off_y: i32,
    /// Texture page: base, colour depth (0: 4 bits, 1: 8, else 15); CLUT
    /// (its row's first pixel, and x).
    pub base_x: usize,
    pub base_y: usize,
    pub depth: u32,
    pub clut_row: usize,
    pub clut_x: usize,
}

/// How a recorded primitive's pixels are drawn: as `Prim` says, with the
/// mask bit settings, at the scale given (dithering follows native pixels).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PixelMode {
    pub prim: Prim,
    pub textured: bool,
    pub shaded: bool,
    pub check_mask: bool,
    pub set_mask: bool,
    pub scale: i32,
}

impl PixelMode {
    fn new(mode: &DrawMode, prim: Prim, check_mask: bool, set_mask: bool, scale: i32) -> Self {
        Self {
            prim,
            textured: mode.textured,
            shaded: mode.shaded,
            check_mask,
            set_mask,
            scale,
        }
    }
}

/// What changes the scaled VRAM, recorded for another renderer (see
/// `Gpu::set_recording`): primitives in scaled coordinates, set up as
/// drawn here; VRAM operations in native ones.
// Triangles are the large and common ones: boxing them would cost an
// allocation each, for commands that only live a frame in one vector.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Command {
    /// Native VRAM, as drawn here: what textures and CLUTs are read from.
    Upload {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        data: Vec<u16>,
    },
    /// The scaled VRAM becomes the native one, scaled up (each native
    /// pixel a block), at this scale from now on.
    Rescale { scale: u32 },
    /// A triangle, set up for its rows (see `Spans`).
    Triangle { pixel: PixelMode, spans: Spans },
    /// A triangle at native resolution, as given (deferred drawing, see
    /// `Gpu::set_deferred`): its vertices (x, y with the drawing offset,
    /// colour, texture coordinates) and the drawing area (inclusive). Set
    /// up where it is drawn, as here.
    Vertices {
        pixel: PixelMode,
        v: [[i32; 7]; 3],
        area: [i32; 4],
    },
    /// A triangle from the GTE's exact projections (precise geometry): its
    /// vertices (scaled), in order (positive area), and the box drawn
    /// (inclusive, within the drawing area).
    Precise {
        pixel: PixelMode,
        vertices: [ExactVertex; 3],
        bbox: [i32; 4],
    },
    /// A rectangle: its corner (may be outside the drawing area), the box
    /// drawn (inclusive), its texture coordinates at the corner and its
    /// colour. Texture coordinates step every `scale` pixels, backwards
    /// when flipped.
    Rect {
        pixel: PixelMode,
        x0: i32,
        y0: i32,
        xs: i32,
        xe: i32,
        ys: i32,
        ye: i32,
        u: i32,
        v: i32,
        color: (i32, i32, i32),
        flip: (bool, bool),
    },
    /// A line between two ends (x, y, then colour), drawn as `scale`-pixel
    /// blocks within the drawing area (inclusive).
    Line {
        pixel: PixelMode,
        a: (i32, i32, i32, i32, i32),
        b: (i32, i32, i32, i32, i32),
        area: [i32; 4],
    },
    /// A fill, wrapping around VRAM; no mask.
    Fill { x: u32, y: u32, w: u32, h: u32, color: u16 },
    /// A VRAM copy (wrapping), its mask settings, and the pixels (indices
    /// in it) the native destination's mask kept.
    Copy {
        sx: u32,
        sy: u32,
        dx: u32,
        dy: u32,
        w: u32,
        h: u32,
        check_mask: bool,
        set_mask: bool,
        skipped: Vec<u32>,
    },
    /// The VRAMs the primitives that follow are drawn into: the native one
    /// (at native resolution, see `Gpu::set_deferred`), the scaled one, or
    /// both (at native resolution, the same; the native one first).
    Target { native: bool, scaled: bool },
    /// Deferred drawing on or off (see `Gpu::set_deferred`): fills, copies
    /// and CPU to VRAM transfers go to the native VRAM too, their masks
    /// checked there (`skipped` and bit 16 unset).
    Deferred { on: bool },
    /// Pixels of a CPU to VRAM transfer to `x, y` (`w` by `h`, wrapping),
    /// from pixel `first` on: each the value written (mask bit set as
    /// asked), and in bit 16 whether the native destination's mask kept it.
    Write {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        first: u32,
        check_mask: bool,
        pixels: Vec<u32>,
    },
}

/// A vertex of a precise triangle: position (scaled) and depth, colour,
/// texture coordinates.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExactVertex {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub color: (i32, i32, i32),
    pub uv: (i32, i32),
}

/// A triangle as `Command::Precise`, when all its vertices have exact
/// positions and its area is still positive with them (else the rounded
/// triangle is drawn). `v` is scaled already (`scale`), in drawing order.
/// A triangle's row (see `Raster::triangle_spans`): its edge functions at
/// its first column and their steps along it, `n` columns after the first:
/// the steps from the first column where all three are non-negative, first
/// and last (empty when the first is after the last).
#[inline(always)]
fn span(row: &[i64; 3], steps: &[(i64, i64); 3], n: i64) -> (i64, i64) {
    let (mut lo, mut hi) = (0i64, n);
    for (w, (a, _)) in row.iter().zip(steps) {
        let (w, a) = (*w, *a);
        if a > 0 {
            if w < 0 {
                lo = lo.max((-w + a - 1) / a);
            }
        } else if a < 0 {
            if w < 0 {
                hi = -1;
            } else {
                hi = hi.min(w / -a);
            }
        } else if w < 0 {
            hi = -1;
        }
    }
    (lo, hi)
}

fn precise_triangle(v: &[Vertex; 3], scale: i32, area: (i32, i32, i32, i32), pixel: PixelMode) -> Option<Command> {
    let s = scale as f32;
    // Each vertex where it is exactly, else where it was rounded to (with
    // no depth: textures are then affine): neighbours sharing a vertex
    // agree on it either way.
    if v.iter().all(|p| p.exact.is_none()) {
        return None;
    }
    let e = v.map(|p| match p.exact {
        Some(x) => [x[0] * s, x[1] * s, x[2]],
        None => [p.x as f32, p.y as f32, 0.0],
    });
    let doubled = (e[1][0] - e[0][0]) * (e[2][1] - e[0][1]) - (e[2][0] - e[0][0]) * (e[1][1] - e[0][1]);
    // The rounded triangle's order makes its area positive (its edge
    // function at the third vertex); the exact one must agree.
    if doubled.is_nan() || doubled <= 0.0 {
        return None;
    }
    let xs = e.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min).floor() as i32;
    let xe = e.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
    let ys = e.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min).floor() as i32;
    let ye = e.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max).ceil() as i32;
    let bbox = [xs.max(area.0), ys.max(area.1), xe.min(area.2), ye.min(area.3)];
    if bbox[0] > bbox[2] || bbox[1] > bbox[3] {
        return None;
    }
    let vertex = |k: usize| ExactVertex {
        x: e[k][0],
        y: e[k][1],
        z: e[k][2],
        color: (v[k].r, v[k].g, v[k].b),
        uv: (v[k].u, v[k].v),
    };
    Some(Command::Precise {
        pixel,
        vertices: [vertex(0), vertex(1), vertex(2)],
        bbox,
    })
}

/// The displayed picture, for another renderer: where it is in VRAM
/// (native pixels), its size, whether it is 24-bit (FMVs, shown from the
/// native VRAM, unscaled) or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Display {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub depth24: bool,
    pub off: bool,
}

impl Prim {
    fn new(mode: &DrawMode, tw: u32) -> Self {
        let (mask_x, mask_y) = ((tw & 0x1f) as i32 * 8, ((tw >> 5) & 0x1f) as i32 * 8);
        let (off_x, off_y) = (((tw >> 10) & 0x1f) as i32 * 8, ((tw >> 15) & 0x1f) as i32 * 8);
        Self {
            raw: mode.raw,
            semi: mode.semi,
            dither: mode.dither,
            blend: (mode.tpage >> 5) & 3,
            mask_x,
            mask_y,
            off_x: off_x & mask_x,
            off_y: off_y & mask_y,
            base_x: ((mode.tpage & 0xf) * 64) as usize,
            base_y: (((mode.tpage >> 4) & 1) * 256) as usize,
            depth: (mode.tpage >> 7) & 3,
            clut_row: mode.clut_y * VRAM_WIDTH,
            clut_x: mode.clut_x,
        }
    }
}

/// A triangle's set-up, for its rows: the edge functions at the first row
/// (bias included: non-negative inside) and their steps along x and y; the
/// attributes there (r, g, b, u, v, in 16.16 fixed point) and their planes
/// (steps along x and y in `.1` and `.2`); the flat colour; the box drawn
/// (inclusive, within the drawing area).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Spans {
    pub row: [i64; 3],
    pub steps: [(i64, i64); 3],
    pub attr_row: [i64; 5],
    pub planes: [(i64, i64, i64); 5],
    pub flat: (i32, i32, i32),
    pub xs: i32,
    pub xe: i32,
    pub ys: i32,
    pub ye: i32,
}

/// The displayed picture as VRAM holds it (15-bit colours, the mask bit
/// on top): `w` by `h` pixels from `x`, `y` in `pixels`, whose rows are
/// `stride` pixels long.
pub struct Picture<'a> {
    pub pixels: &'a [u16],
    pub stride: usize,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// The displayed area of `scaled` (VRAM at `scale`), as it is: `None` in
/// 24-bit mode and with the display off (`display_rgba` then). A picture
/// that wraps around VRAM's edges is copied into `wrapped` (rarely).
pub(crate) fn display_15<'a>(
    d: Display,
    scale: usize,
    scaled: &'a [u16],
    wrapped: &'a mut Vec<u16>,
) -> Option<Picture<'a>> {
    if d.off || d.depth24 {
        return None;
    }
    let (stride, rows) = (VRAM_WIDTH * scale, VRAM_HEIGHT * scale);
    let (w, h) = (d.w as usize * scale, d.h as usize * scale);
    let (x, y) = ((d.x as usize * scale) % stride, (d.y as usize * scale) % rows);
    if x + w <= stride && y + h <= rows {
        return Some(Picture {
            pixels: scaled,
            stride,
            x,
            y,
            w,
            h,
        });
    }
    wrapped.clear();
    for r in 0..h {
        let row = &scaled[(y + r) % rows * stride..][..stride];
        wrapped.extend((0..w).map(|c| row[(x + c) % stride]));
    }
    Some(Picture {
        pixels: wrapped,
        stride: w,
        x: 0,
        y: 0,
        w,
        h,
    })
}

/// Renders the displayed area as RGBA8888 into `out` and returns its size:
/// the display size times `scale` (`scaled`: the VRAM drawn at that scale),
/// except in 24-bit mode (FMVs), which only exists at native resolution
/// (`native`).
pub(crate) fn display_rgba(
    d: Display,
    scale: usize,
    scaled: &[u16],
    native: &[u16],
    out: &mut Vec<u8>,
) -> (usize, usize) {
    let (w, h) = (d.w as usize, d.h as usize);
    let s = if d.depth24 { 1 } else { scale };
    let (ow, oh) = (w * s, h * s);
    // Every pixel is written below: cleared only when the size changes.
    if out.len() != ow * oh * 4 {
        out.clear();
        out.resize(ow * oh * 4, 0);
    }
    if d.off {
        for px in out.as_chunks_mut::<4>().0 {
            *px = [0, 0, 0, 0xff];
        }
        return (ow, oh);
    }
    let (src, stride) = if d.depth24 {
        (native, VRAM_WIDTH)
    } else {
        (scaled, VRAM_WIDTH * s)
    };
    let rows = VRAM_HEIGHT * s;
    for y in 0..oh {
        let vy = (d.y as usize * s + y) % rows;
        let row = &src[vy * stride..(vy + 1) * stride];
        let dst = &mut out[y * ow * 4..(y + 1) * ow * 4];
        let dst = dst.as_chunks_mut::<4>().0;
        if d.depth24 {
            for (x, px) in dst.iter_mut().enumerate() {
                // 3 bytes per pixel, packed across 16-bit words.
                let byte = d.x as usize * 2 + x * 3;
                let get = |b: usize| -> u8 {
                    let word = row[(b / 2) & 0x3ff];
                    (word >> ((b & 1) * 8)) as u8
                };
                *px = [get(byte), get(byte + 1), get(byte + 2), 0xff];
            }
        } else {
            // The row from the display's left edge, wrapping around VRAM
            // at most once (the display is narrower than VRAM).
            let start = (d.x as usize * s) % stride;
            let before_wrap = (stride - start).min(dst.len());
            let (left, right) = dst.split_at_mut(before_wrap);
            widen(&row[start..start + before_wrap], left);
            widen(&row[..right.len()], right);
        }
    }
    (ow, oh)
}

/// 15-bit colours as RGBA8888, each 5-bit channel widened to 8 bits (in
/// 32-bit lanes, without a table: the compiler vectorises it).
fn widen(src: &[u16], dst: &mut [[u8; 4]]) {
    for (px, &c) in dst.iter_mut().zip(src) {
        let c = u32::from(c);
        let e = |v: u32| ((v & 0x1f) << 3) | ((v & 0x1f) >> 2);
        *px = (e(c) | (e(c >> 5) << 8) | (e(c >> 10) << 16) | 0xff00_0000).to_le_bytes();
    }
}

pub mod replay;

fn rgb24_to_15(c: u32) -> u16 {
    let r = (c >> 3) & 0x1f;
    let g = (c >> 11) & 0x1f;
    let b = (c >> 19) & 0x1f;
    (r | (g << 5) | (b << 10)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_15_bit_picture_is_the_rgba_one() {
        let scale = 2;
        let vram: Vec<u16> = (0..VRAM_WIDTH * VRAM_HEIGHT * scale * scale)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761) as u16)
            .collect();
        // In the middle of VRAM, and wrapping around its right and bottom
        // edges.
        for (x, y) in [(64, 16), (900, 400)] {
            let d = Display {
                x,
                y,
                w: 320,
                h: 240,
                depth24: false,
                off: false,
            };
            let mut rgba = Vec::new();
            let (w, h) = display_rgba(d, scale, &vram, &vram, &mut rgba);
            let mut wrapped = Vec::new();
            let p = display_15(d, scale, &vram, &mut wrapped).expect("a 15-bit picture");
            assert_eq!((p.w, p.h), (w, h));
            let mut row = vec![[0u8; 4]; w];
            for r in 0..h {
                let start = (p.y + r) * p.stride + p.x;
                widen(&p.pixels[start..start + w], &mut row);
                assert_eq!(
                    row.as_flattened(),
                    &rgba[r * w * 4..(r + 1) * w * 4],
                    "({x}, {y}), row {r}"
                );
            }
        }
    }

    #[test]
    fn colours_widen_as_the_console_shows_them() {
        let src: Vec<u16> = (0..=0xffff).collect();
        let mut dst = vec![[0u8; 4]; src.len()];
        widen(&src, &mut dst);
        let e = |v: u16| ((v & 0x1f) << 3 | (v & 0x1f) >> 2) as u8;
        for (&c, px) in src.iter().zip(&dst) {
            assert_eq!(*px, [e(c), e(c >> 5), e(c >> 10), 0xff], "{c:04x}");
        }
    }

    fn draw_area(gpu: &mut Gpu) {
        gpu.gp0(0xe300_0000);
        gpu.gp0(0xe400_0000 | (511 << 10) | 1023);
    }

    #[test]
    fn flat_triangle_follows_the_fill_rule() {
        let mut gpu = Gpu::new();
        draw_area(&mut gpu);
        // Flat red triangle (0,0) (4,0) (0,4).
        gpu.gp0(0x2000_00ff);
        gpu.gp0(0);
        gpu.gp0(4);
        gpu.gp0(4 << 16);
        let px = |g: &Gpu, x: usize, y: usize| g.vram[y * VRAM_WIDTH + x];
        assert_eq!(px(&gpu, 0, 0), 0x1f);
        assert_eq!(px(&gpu, 3, 0), 0x1f);
        // Right edge excluded.
        assert_eq!(px(&gpu, 4, 0), 0);
        assert_eq!(px(&gpu, 0, 4), 0);
    }

    #[test]
    fn scaled_drawing_keeps_native_vram_exact() {
        let mut native = Gpu::new();
        let mut scaled = Gpu::new();
        scaled.set_scale(2);
        for gpu in [&mut native, &mut scaled] {
            draw_area(gpu);
            gpu.gp0(0x3000_00ff); // Gouraud triangle
            gpu.gp0(0);
            gpu.gp0(0x0000_ff00);
            gpu.gp0(40);
            gpu.gp0(0x00ff_0000);
            gpu.gp0(30 << 16);
        }
        assert_eq!(native.vram, scaled.vram);
        // Scaled VRAM: twice the pixels in each direction.
        let drawn = |g: &Gpu, buf: &[u16]| buf.iter().filter(|&&p| p != 0).count() as f64 / g.scale.pow(2) as f64;
        let n = drawn(&native, &native.vram);
        let h = drawn(&scaled, &scaled.hires);
        assert!((h - n).abs() / n < 0.1, "{n} vs {h}");
        let mut frame = Vec::new();
        scaled.gp1(0x0300_0000);
        assert_eq!(scaled.render_display(&mut frame), (512, 480));
    }

    /// GP0 words drawing a bit of everything into masked pixels: a texture
    /// (some texels with the mask bit), shaded and textured triangles, a
    /// semi-transparent quad, a rectangle, a line, a fill, a copy and a
    /// transfer with the mask checked.
    fn scene() -> Vec<u32> {
        let xy = |x: u32, y: u32| x | (y << 16);
        let tpage = 1 | (2 << 7);
        let mut w = vec![0xe300_0000, 0xe400_0000 | (511 << 10) | 1023];
        w.extend([0xa000_0000, xy(64, 0), xy(16, 16)]);
        w.extend((0..128u32).map(|i| i.wrapping_mul(0x9e37_79b9) ^ if i % 3 == 0 { 0x8000_8000 } else { 0 }));
        w.extend([0xe600_0002, 0xe100_0200 | tpage]);
        w.extend([0x3000_00ff, xy(60, 2), 0x0000_ff00, xy(90, 10), 0x00ff_0000, xy(70, 30)]);
        w.extend([0xe600_0001]);
        w.extend([0x2480_8080, xy(100, 100), 0, xy(140, 100), (tpage << 16) | 15, xy(100, 140), 15 << 8]);
        let semi = tpage | (1 << 5);
        w.extend([0x2e80_8080, xy(120, 90), 0, xy(170, 95), (semi << 16) | 15, xy(115, 150), 15 << 8, xy(175, 140), 0x0f0f]);
        w.extend([0xe100_0000 | tpage, 0x6480_8080, xy(200, 10), 0, xy(20, 20)]);
        w.extend([0x5000_00ff, xy(10, 200), 0x00ff_ff00, xy(150, 260)]);
        w.extend([0x0200_ff00, xy(300, 300), xy(32, 16)]);
        w.extend([0xe600_0002, 0x8000_0000, xy(60, 0), xy(100, 105), xy(40, 30)]);
        w.extend([0xa000_0000, xy(100, 100), xy(8, 8)]);
        w.extend((0..32u32).map(|i| i.wrapping_mul(0x0123_4567)));
        w
    }

    /// Deferred drawing (see `Gpu::set_deferred`) times the GPU, reads VRAM
    /// back and shows pictures as drawing here does: at native resolution
    /// (primitives recorded once for both VRAMs) and scaled, with precise
    /// geometry (a triangle's vertices projected exactly).
    #[test]
    fn deferred_drawing_is_drawing_here() {
        for scale in [1, 2] {
            deferred_drawing_at(scale);
        }
    }

    fn deferred_drawing_at(scale: usize) {
        use std::{cell::RefCell, rc::Rc};
        let mut here = Gpu::new();
        let mut there = Gpu::new();
        let mut replay_here = replay::Replay::new();
        let replay_there = Rc::new(RefCell::new(replay::Replay::new()));
        for gpu in [&mut here, &mut there] {
            gpu.set_scale(scale);
            gpu.set_recording(true);
            gpu.set_precise(true);
            for (x, y) in [(100, 100), (140, 100), (100, 140)] {
                gpu.exact_vertex(x | (y << 16), [x as f32 + 0.4, y as f32 - 0.3, 200.0 + x as f32]);
            }
        }
        let r = replay_there.clone();
        there.set_deferred(Some(Box::new(move |commands, vram: &mut [u16]| {
            let mut r = r.borrow_mut();
            r.apply(&commands);
            vram.copy_from_slice(r.native());
        })));
        assert!(there.deferred());
        // The scene, VRAM read back, then more drawing over it.
        let read = |gpu: &mut Gpu| {
            for w in scene() {
                gpu.gp0(w);
            }
            gpu.gp0(0xc000_0000);
            gpu.gp0(96 | (96 << 16));
            gpu.gp0(64 | (64 << 16));
            let words: Vec<u32> = (0..64 * 64 / 2).map(|_| gpu.read_data()).collect();
            gpu.gp0(0x2000_ff00);
            gpu.gp0(90 | (90 << 16));
            gpu.gp0(160 | (100 << 16));
            gpu.gp0(100 | (170 << 16));
            words
        };
        let words = read(&mut here);
        let commands = here.take_commands();
        assert!(commands.iter().any(|c| matches!(c, Command::Precise { .. })), "no precise triangle");
        replay_here.apply(&commands);
        assert_eq!(read(&mut there), words);
        assert!(there.stale);
        replay_there.borrow_mut().apply(&there.take_commands());
        assert_eq!(here.drawing(), there.drawing());
        there.sync_vram();
        assert!(here.vram == there.vram, "the native VRAMs differ at {scale}x");
        // The scaled pictures, the whole VRAM.
        let d = Display {
            x: 0,
            y: 0,
            w: VRAM_WIDTH as u32,
            h: VRAM_HEIGHT as u32,
            depth24: false,
            off: false,
        };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        replay_here.render_display(d, &mut a);
        replay_there.borrow().render_display(d, &mut b);
        assert!(a == b, "the scaled pictures differ at {scale}x");
    }

    #[test]
    fn cpu_to_vram_and_back() {
        let mut gpu = Gpu::new();
        gpu.gp0(0xa000_0000);
        gpu.gp0(10 | (20 << 16));
        gpu.gp0(2 | (2 << 16));
        gpu.gp0(0x2222_1111);
        gpu.gp0(0x4444_3333);
        assert_eq!(gpu.vram[20 * VRAM_WIDTH + 11], 0x2222);
        gpu.gp0(0xc000_0000);
        gpu.gp0(10 | (21 << 16));
        gpu.gp0(2 | (1 << 16));
        assert_ne!(gpu.status() & (1 << 27), 0);
        assert_eq!(gpu.read_data(), 0x4444_3333);
    }

    #[test]
    fn fill_and_textured_rect() {
        let mut gpu = Gpu::new();
        draw_area(&mut gpu);
        // 15-bit texture at (64, 0): one white texel.
        gpu.vram[64] = 0x7fff;
        gpu.gp0(0xe100_0001 | (2 << 7)); // texpage x=64, 15-bit
        gpu.gp0(0x6580_8080); // 1x1? no: variable size textured, raw
        gpu.gp0(100 | (100 << 16));
        gpu.gp0(0);
        gpu.gp0(1 | (1 << 16));
        assert_eq!(gpu.vram[100 * VRAM_WIDTH + 100], 0x7fff);
        gpu.gp0(0x0200_00ff);
        gpu.gp0(0);
        gpu.gp0(16 | (1 << 16));
        assert_eq!(gpu.vram[15], 0x1f);
    }
}
