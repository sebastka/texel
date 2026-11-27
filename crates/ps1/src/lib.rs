//! PlayStation emulation core.
//!
//! [`System`] is the whole console: give it a BIOS, optionally a disc, and
//! call [`System::run_frame`] once per video frame. It has no I/O of its
//! own: frames, audio and memory cards go in and out through its methods.

pub mod bus;
mod cdrom;
pub mod cheats;
pub mod cpu;
mod dma;
mod gpu;
mod gte;
mod irq;
mod mdec;
pub mod pad;
mod rewind;
mod sio;
pub mod spu;
mod timers;

pub mod bios;
pub mod jit;

pub use bios::{BiosInfo, Region};
pub use cdrom::DiscImage;
pub use gpu::Picture;
/// What the GPU records for another renderer (see `System::set_gpu_recording`).
pub mod gpu_record {
    pub use crate::gpu::replay::Replay;
    pub use crate::gpu::{Command, Display, ExactVertex, PixelMode, Prim, Spans, VramFetch};
}
pub use sio::{MEMCARD_SIZE, MemoryCard, button};

use bios::fnv1a;
use bus::{BIOS_SIZE, Bus};
use cpu::Cpu;
pub use rewind::Rewind;

/// CPU clock in Hz.
pub const CPU_CLOCK: u32 = 33_868_800;
/// Audio output rate, stereo.
pub const AUDIO_RATE: u32 = 44_100;

/// Save state header and format version: states of other versions are
/// refused (bumped whenever the emulated state changes shape).
const STATE_MAGIC: &[u8; 8] = b"TEXEL\0\0\0";
/// Format 3 adds the CPU's debug registers after the rest, format 4 the
/// timing of the GPU and SPU (`Timing`); older states load without them.
const STATE_VERSION: u32 = 4;

/// What the machine does next that the parts' own state leaves out, kept
/// after it: the GPU's drawing under way and the command list waiting for
/// it, the SPU's capture position. Without them, a restored machine runs
/// differently from the one saved (netplay members, given the host's
/// state, fell out of step again at once).
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
struct Timing {
    gpu_drawing: u64,
    gpu_list: Option<u64>,
    spu_capture: u32,
}

#[derive(serde::Serialize)]
struct SnapshotRef<'a> {
    bios_hash: u64,
    cpu: &'a Cpu,
    bus: &'a Bus,
    cpu_debug: [u32; 5],
    timing: Timing,
}

struct Snapshot {
    bios_hash: u64,
    cpu: Cpu,
    bus: Bus,
    cpu_debug: [u32; 5],
    /// None: an older state.
    timing: Option<Timing>,
}

#[derive(serde::Deserialize)]
struct SnapshotV3 {
    bios_hash: u64,
    cpu: Cpu,
    bus: Bus,
    cpu_debug: [u32; 5],
}

#[derive(serde::Deserialize)]
struct SnapshotV2 {
    bios_hash: u64,
    cpu: Cpu,
    bus: Bus,
}

/// Where the BIOS jumps to the shell, once the kernel is initialised: the
/// moment to start a side-loaded executable.
const SHELL_ENTRY: u32 = 0x8003_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    BadBios,
    BadExe(&'static str),
    BadMemoryCard,
    BadState(String),
    /// The state was saved with another BIOS.
    StateBiosMismatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadBios => write!(f, "not a PlayStation BIOS image ({BIOS_SIZE} bytes expected)"),
            Self::BadExe(why) => write!(f, "not a PS-X EXE: {why}"),
            Self::BadMemoryCard => write!(f, "a memory card image must be exactly {MEMCARD_SIZE} bytes"),
            Self::BadState(why) => write!(f, "unusable save state: {why}"),
            Self::StateBiosMismatch => f.write_str("this save state was made with another BIOS"),
        }
    }
}

impl std::error::Error for Error {}

pub struct System {
    cpu: Cpu,
    bus: Bus,
    bios_info: BiosInfo,
    exe: Option<Vec<u8>>,
    /// The last frame, RGBA8888, made when asked for (`frame`): renderers
    /// that take VRAM's colours (`picture`) do without.
    frame: Vec<u8>,
    frame_size: (usize, usize),
    frame_stale: bool,
    /// The CPU's code compiled to WebAssembly (browsers), when set.
    jit: Option<Box<jit::Jit>>,
    /// Cheat and patch codes enabled (see `set_codes`).
    cheats: cheats::Cheats,
    /// Enhancements on (see `set_precise_geometry`, `set_widescreen`): kept
    /// across resets and restored states.
    precise: bool,
    widescreen: bool,
}

impl System {
    pub fn new(bios: &[u8]) -> Result<Self, Error> {
        let bios_info = bios::identify(bios).ok_or(Error::BadBios)?;
        let mut bus = Bus::new(bios.to_vec().into_boxed_slice());
        bus.cdrom.region = bios_info.region.licence_letter();
        Ok(Self {
            cpu: Cpu::new(),
            bus,
            bios_info,
            exe: None,
            frame: Vec::new(),
            frame_size: (0, 0),
            frame_stale: false,
            jit: None,
            cheats: cheats::Cheats::default(),
            precise: false,
            widescreen: false,
        })
    }

    pub fn bios_info(&self) -> &BiosInfo {
        &self.bios_info
    }

    /// Hard reset; the disc and memory cards stay in.
    pub fn reset(&mut self) {
        let disc = self.bus.cdrom.disc.take();
        let cards = std::mem::take(&mut self.bus.sio.cards);
        let pads = self.bus.sio.pads.clone();
        let bios = std::mem::take(&mut self.bus.bios);
        let region = self.bus.cdrom.region;
        let scale = self.bus.gpu.scale();
        let recording = self.bus.gpu.recording();
        let deferred = self.bus.gpu.deferred().then(|| self.bus.gpu.take_fetch()).flatten();
        self.bus = Bus::new(bios);
        self.bus.gpu.set_scale(scale);
        if recording {
            self.bus.gpu.set_recording(true);
            self.bus.gpu.set_deferred(deferred);
        }
        self.bus.cdrom.region = region;
        self.bus.cdrom.insert_disc(disc);
        self.bus.sio.cards = cards;
        self.bus.sio.pads = pads;
        self.cpu = Cpu::new();
        self.apply_enhancements();
        // Compiled code knows where the old memory was.
        if let Some(jit) = &mut self.jit {
            jit.clear();
        }
    }

    /// Closes the CD drive's lid on a disc (or on nothing).
    pub fn insert_disc(&mut self, disc: Option<DiscImage>) {
        self.bus.cdrom.insert_disc(disc);
    }

    /// Opens the CD drive's lid, to change discs: wait a moment (games poll
    /// for it), then [`System::insert_disc`].
    pub fn open_lid(&mut self) {
        self.bus.cdrom.open_lid();
    }

    pub fn lid_open(&self) -> bool {
        self.bus.cdrom.lid_open()
    }

    /// Loads a PS-X EXE, started once the BIOS has initialised the kernel.
    pub fn sideload_exe(&mut self, exe: Vec<u8>) -> Result<(), Error> {
        if exe.len() < 0x800 || &exe[..8] != b"PS-X EXE" {
            return Err(Error::BadExe("missing the PS-X EXE header"));
        }
        self.exe = Some(exe);
        Ok(())
    }

    fn start_exe(&mut self, exe: &[u8]) {
        let word = |o: usize| u32::from_le_bytes([exe[o], exe[o + 1], exe[o + 2], exe[o + 3]]);
        let (pc, gp, dest, size) = (word(0x10), word(0x14), word(0x18), word(0x1c));
        let (sp_base, sp_off) = (word(0x30), word(0x34));
        let data = &exe[0x800..];
        for (i, &b) in data.iter().take(size as usize).enumerate() {
            self.bus.store8(dest.wrapping_add(i as u32), b);
        }
        self.cpu.set_reg_now(28, gp);
        if sp_base != 0 {
            self.cpu.set_reg_now(29, sp_base.wrapping_add(sp_off));
            self.cpu.set_reg_now(30, sp_base.wrapping_add(sp_off));
        }
        self.cpu.jump(pc);
    }

    /// Runs until the GPU starts its next vertical blank.
    pub fn run_frame(&mut self) {
        self.bus.gpu.frame_ready = false;
        if !self.cheats.is_empty() {
            let pad = self.cheat_pad();
            self.cheats.run(cheats::Activation::StartFrame, &mut self.bus, pad);
        }
        // A frame is ~570k cycles; bail out after two in case video is off.
        let mut budget = 2 * CPU_CLOCK / 50 * self.bus.overclock() / 100;
        // Whether the next instruction may start a block: the compiler is
        // asked where blocks start, not before every instruction it
        // interprets (code not compiled yet runs block by block in the
        // interpreter, without a lookup for each instruction).
        let mut block_start = true;
        while !self.bus.gpu.frame_ready && budget > 0 {
            // A compiled block counts its cycles into `pending` itself (the
            // blocks after it too, run on while nothing else is due).
            let compiled = block_start
                && self.exe.is_none()
                && self
                    .jit
                    .as_mut()
                    .is_some_and(|jit| jit.run_chained(&mut self.cpu, &mut self.bus, |bus| !bus.gpu.frame_ready));
            let cycles = if compiled {
                0
            } else {
                let pc = self.cpu.pc;
                let cycles = self.cpu.step(&mut self.bus);
                // A block ends after a branch's delay slot; an exception or
                // an interrupt goes elsewhere.
                block_start = self.cpu.delay_slot || self.cpu.pc != pc.wrapping_add(4);
                cycles
            };
            if compiled {
                block_start = true;
            }
            self.bus.pending += cycles;
            // Peripherals also catch up on every I/O access (see Bus::sync).
            if self.bus.pending >= self.bus.deadline {
                budget = budget.saturating_sub(self.bus.pending);
                self.bus.sync();
            }
            if self.cpu.pc == SHELL_ENTRY
                && let Some(exe) = self.exe.take()
            {
                self.start_exe(&exe);
            }
        }
        if !self.cheats.is_empty() {
            let pad = self.cheat_pad();
            self.cheats.run(cheats::Activation::EndFrame, &mut self.bus, pad);
        }
        self.frame_size = self.bus.gpu.display_dims();
        self.frame_stale = true;
        // The blocks compiled this frame, installed.
        if let Some(jit) = &mut self.jit {
            jit.end_frame();
        }
    }

    /// Diagnostics: one turn of `run_frame`'s loop (a compiled block, or
    /// an instruction, then the peripherals when due): the cycles run so
    /// far. Two machines stepped so can be compared where their cycle
    /// counts meet (the compiler against the interpreter).
    #[doc(hidden)]
    pub fn debug_step(&mut self) -> u64 {
        let compiled = self.exe.is_none()
            && self
                .jit
                .as_mut()
                .is_some_and(|jit| jit.run(&mut self.cpu, &mut self.bus));
        let cycles = if compiled { 0 } else { self.cpu.step(&mut self.bus) };
        self.bus.pending += cycles;
        if self.bus.pending >= self.bus.deadline {
            self.bus.sync();
        }
        self.bus.elapsed + u64::from(self.bus.pending)
    }

    /// Diagnostics: how many cycles the CPU runs between the peripherals'
    /// catching up (the default: [`bus::SLICE`]).
    #[doc(hidden)]
    pub fn debug_set_slice(&mut self, cycles: u32) {
        self.bus.slice = cycles.max(1);
        self.bus.deadline = self.bus.slice;
    }

    /// Diagnostics: the CPU's pc (cheap, unlike `debug_cpu`).
    #[doc(hidden)]
    pub fn debug_pc(&self) -> u32 {
        self.cpu.pc
    }

    /// Diagnostics: the CPU's state, field by field.
    #[doc(hidden)]
    pub fn debug_cpu_fields(&self) -> Vec<(&'static str, String)> {
        self.cpu.debug_fields()
    }

    /// Diagnostics: a checksum of the CPU's state (registers, COP0, the
    /// GTE), and its pc.
    #[doc(hidden)]
    pub fn debug_cpu(&self) -> (u64, u32) {
        let bytes = postcard::to_stdvec(&self.cpu).unwrap_or_default();
        (fnv1a(&bytes), self.cpu.pc)
    }

    /// The cheat and patch codes enabled from now on, in order (see
    /// `cheats`): (name, body with options filled in, activation). Those
    /// no longer enabled put back what they replaced. Returns the codes
    /// refused, and why. They change what the console computes: netplay
    /// peers must enable the same at the same frames.
    pub fn set_codes(&mut self, codes: &[(String, String, cheats::Activation)]) -> Vec<(String, String)> {
        let mut ok = Vec::new();
        let mut refused = Vec::new();
        for (name, body, activation) in codes {
            match cheats::Code::parse(name, body, *activation) {
                Ok(code) => ok.push(code),
                Err(e) => refused.push((name.clone(), e)),
            }
        }
        let pad = self.cheat_pad();
        self.cheats.set(ok, &mut self.bus, pad);
        refused
    }

    /// Port 1's controller, as codes read it (jokers).
    fn cheat_pad(&self) -> cheats::Pad {
        let p = &self.bus.sio.pads[0];
        cheats::Pad {
            buttons: p.buttons,
            axes: p.axes,
        }
    }

    /// RAM's size: 2 MiB, or 8 MiB as development consoles (some patches
    /// need it). Compiled code is dropped: it knows RAM's place and size.
    pub fn set_ram_size(&mut self, bytes: usize) {
        let bytes = if bytes > bus::RAM_SIZE {
            bus::RAM_MAX
        } else {
            bus::RAM_SIZE
        };
        if bytes != self.bus.ram.len() {
            self.bus.set_ram_size(bytes);
            if let Some(jit) = &mut self.jit {
                jit.clear();
            }
        }
    }

    /// The CPU's speed, in percent (100: the console's; some patches need
    /// more).
    pub fn set_overclock(&mut self, percent: u32) {
        self.bus.set_overclock(percent);
    }

    /// The last frame, RGBA8888, and its size.
    pub fn frame(&mut self) -> (&[u8], usize, usize) {
        if std::mem::take(&mut self.frame_stale) {
            self.frame_size = self.bus.gpu.render_display(&mut self.frame);
        }
        (&self.frame, self.frame_size.0, self.frame_size.1)
    }

    /// The last frame's size (without making it).
    pub fn frame_size(&self) -> (usize, usize) {
        self.frame_size
    }

    /// The last frame as VRAM holds it (15-bit colours), for a renderer
    /// that converts them itself; `None` when only `frame` has it (24-bit
    /// pictures, the display off, drawing recorded). `wrapped`: room for a
    /// picture wrapping around VRAM's edges.
    pub fn picture<'a>(&'a self, wrapped: &'a mut Vec<u16>) -> Option<Picture<'a>> {
        self.bus.gpu.picture(wrapped)
    }

    /// Sets the internal resolution multiplier (1 to 4): polygons are
    /// drawn with more detail, frames get bigger.
    pub fn set_resolution_scale(&mut self, scale: usize) {
        self.bus.gpu.set_scale(scale);
    }

    /// Compiles the CPU's code with `backend` as it runs (blocks run the
    /// same as interpreted), or interprets it all (`None`).
    pub fn set_compiler(&mut self, backend: Option<Box<dyn jit::Backend>>) {
        self.jit = backend.map(|b| Box::new(jit::Jit::new(b)));
    }

    /// What the compiler did, for diagnostics.
    pub fn compiler_stats(&self) -> Option<jit::Stats> {
        self.jit.as_ref().map(|j| j.stats)
    }

    /// Records the GPU's drawing for another renderer, at the resolution
    /// scale, rather than drawing it scaled here (see `take_gpu_commands`):
    /// `frame` is then empty.
    pub fn set_gpu_recording(&mut self, on: bool) {
        self.bus.gpu.set_recording(on);
    }

    /// Recording, the native VRAM drawn by the recording's renderer too
    /// (`Some`: how to fetch it from there when something reads it), not
    /// here: see `Gpu::set_deferred`. What the console computes does not
    /// change.
    pub fn set_gpu_deferred(&mut self, fetch: Option<gpu::VramFetch>) {
        self.bus.gpu.set_deferred(fetch);
    }

    /// Recording, the scaled drawing held back while `on` (frames run again
    /// by rollback, never shown): see `Gpu::set_held`.
    pub fn set_gpu_held(&mut self, on: bool) {
        self.bus.gpu.set_held(on);
    }

    /// Deferred drawing: brings the native VRAM up to date from the
    /// renderer's (before reading the machine's state from outside: a
    /// save state, a checksum, a rollback's state).
    pub fn sync_vram(&mut self) {
        self.bus.gpu.sync_vram();
    }

    /// Precise geometry (an enhancement, when recording): the GTE keeps its
    /// projections' exact positions and depths, and triangles made of them
    /// are recorded with them, to draw without the console's rounding
    /// (no wobbling polygons) and with perspective-correct textures. What
    /// the console computes does not change.
    pub fn set_precise_geometry(&mut self, on: bool) {
        self.precise = on;
        self.apply_enhancements();
    }

    fn apply_enhancements(&mut self) {
        self.cpu.gte.precise = self.precise;
        self.cpu.gte.widescreen = self.widescreen;
        self.bus.gpu.set_precise(self.precise);
    }

    /// Widescreen (an enhancement): the GTE squeezes X by 3/4 around the
    /// screen's centre, for a picture shown 16:9. It changes what the
    /// console computes (3D games' geometry; 2D is unchanged, so stretched).
    pub fn set_widescreen(&mut self, on: bool) {
        self.widescreen = on;
        self.apply_enhancements();
    }

    /// The drawing recorded since the last call.
    pub fn take_gpu_commands(&mut self) -> Vec<gpu_record::Command> {
        self.bus.gpu.take_commands()
    }

    /// The drawing recorded since the last call, for a renderer without the
    /// native VRAM (`gpu_record::Replay`): with the displayed rows when
    /// the picture is 24-bit (FMVs are shown from the native VRAM).
    pub fn take_gpu_commands_for_replay(&mut self) -> Vec<gpu_record::Command> {
        let mut commands = self.bus.gpu.take_commands();
        let d = self.bus.gpu.display();
        // Deferred, the renderer's native VRAM is its own (exact).
        if d.depth24 && !d.off && !self.bus.gpu.deferred() {
            const WIDTH: usize = 1024;
            for row in 0..d.h {
                let y = (d.y + row) % 512;
                let line = &self.bus.gpu.vram[y as usize * WIDTH..(y as usize + 1) * WIDTH];
                commands.push(gpu_record::Command::Upload {
                    x: 0,
                    y,
                    w: WIDTH as u32,
                    h: 1,
                    data: line.to_vec(),
                });
            }
        }
        commands
    }

    /// The displayed picture's place in VRAM, for another renderer.
    pub fn gpu_display(&self) -> gpu_record::Display {
        self.bus.gpu.display()
    }

    /// The native VRAM (1024 by 512 pixels), for another renderer.
    pub fn vram(&self) -> &[u16] {
        &self.bus.gpu.vram
    }

    pub fn resolution_scale(&self) -> usize {
        self.bus.gpu.scale()
    }

    /// Whether the game runs at 50 Hz.
    pub fn is_pal(&self) -> bool {
        self.bus.gpu.is_pal()
    }

    /// Appends the interleaved stereo samples produced since the last call.
    pub fn take_audio(&mut self, out: &mut Vec<i16>) {
        self.bus.spu.drain_output(out);
    }

    /// Sets the pressed buttons of a controller (see [`button`]).
    pub fn set_buttons(&mut self, port: usize, pressed: u16) {
        if let Some(p) = self.bus.sio.pads.get_mut(port) {
            p.buttons = pressed;
        }
    }

    /// Sets the analog sticks: left X, left Y, right X, right Y, each
    /// 0 (left/up) to 255 (right/down), 128 at rest.
    pub fn set_axes(&mut self, port: usize, axes: [u8; 4]) {
        if let Some(p) = self.bus.sio.pads.get_mut(port) {
            p.axes = axes;
        }
    }

    /// The controller's Analog button. Returns whether analog mode is on.
    pub fn press_analog_button(&mut self, port: usize) -> bool {
        let Some(p) = self.bus.sio.pads.get_mut(port) else {
            return false;
        };
        p.press_analog_button();
        p.analog
    }

    /// Whether the controller is in analog mode (games can switch it).
    pub fn is_analog(&self, port: usize) -> bool {
        self.bus.sio.pads.get(port).is_some_and(|p| p.analog)
    }

    /// Rumble motors: small (0 or 255) and large (0-255).
    pub fn rumble(&self, port: usize) -> (u8, u8) {
        self.bus.sio.pads.get(port).map_or((0, 0), |p| p.rumble)
    }

    pub fn set_pad_connected(&mut self, port: usize, connected: bool) {
        if let Some(p) = self.bus.sio.pads.get_mut(port) {
            p.connected = connected;
        }
    }

    /// Inserts a memory card (`None` data: freshly formatted), or removes it.
    pub fn set_memory_card(&mut self, slot: usize, card: Option<Option<Vec<u8>>>) -> Result<(), Error> {
        let card = match card {
            None => None,
            Some(None) => Some(MemoryCard::formatted()),
            Some(Some(data)) if data.len() == MEMCARD_SIZE => Some(MemoryCard::from_data(data.into_boxed_slice())),
            Some(Some(_)) => return Err(Error::BadMemoryCard),
        };
        if let Some(s) = self.bus.sio.cards.get_mut(slot) {
            *s = card;
        }
        Ok(())
    }

    /// Saves the whole machine: CPU, memory, VRAM, sound RAM and every
    /// peripheral. Not included: the BIOS, the disc, the memory cards and
    /// which controllers are plugged in, which stay as they are when the
    /// state is loaded.
    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 << 20);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        postcard::to_extend(&self.snapshot_ref(), out).expect("serialising to memory cannot fail")
    }

    /// What a state holds (see `save_state`, and `rewind`'s).
    fn snapshot_ref(&self) -> SnapshotRef<'_> {
        SnapshotRef {
            bios_hash: fnv1a(&self.bus.bios),
            cpu: &self.cpu,
            bus: &self.bus,
            cpu_debug: self.cpu.debug_registers(),
            timing: Timing {
                gpu_drawing: self.bus.gpu.drawing,
                gpu_list: self.bus.gpu_list,
                spu_capture: self.bus.spu.capture,
            },
        }
    }

    /// Restores a state from [`System::save_state`]. It must come from the
    /// same BIOS; the disc in the drive should be the one it was saved with.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        let body = data
            .strip_prefix(STATE_MAGIC.as_slice())
            .ok_or_else(|| Error::BadState("not a Texel save state".into()))?;
        let (version, body) = body
            .split_at_checked(4)
            .ok_or_else(|| Error::BadState("truncated".into()))?;
        let version = u32::from_le_bytes(version.try_into().expect("4 bytes"));
        let bad = |e: postcard::Error| Error::BadState(e.to_string());
        let snapshot: Snapshot = match version {
            STATE_VERSION => {
                #[derive(serde::Deserialize)]
                struct V4 {
                    bios_hash: u64,
                    cpu: Cpu,
                    bus: Bus,
                    cpu_debug: [u32; 5],
                    timing: Timing,
                }
                let s: V4 = postcard::from_bytes(body).map_err(bad)?;
                Snapshot {
                    bios_hash: s.bios_hash,
                    cpu: s.cpu,
                    bus: s.bus,
                    cpu_debug: s.cpu_debug,
                    timing: Some(s.timing),
                }
            }
            3 => {
                let s: SnapshotV3 = postcard::from_bytes(body).map_err(bad)?;
                Snapshot {
                    bios_hash: s.bios_hash,
                    cpu: s.cpu,
                    bus: s.bus,
                    cpu_debug: s.cpu_debug,
                    timing: None,
                }
            }
            2 => {
                let s: SnapshotV2 = postcard::from_bytes(body).map_err(bad)?;
                Snapshot {
                    bios_hash: s.bios_hash,
                    cpu: s.cpu,
                    bus: s.bus,
                    cpu_debug: [0; 5],
                    timing: None,
                }
            }
            _ => {
                return Err(Error::BadState(format!(
                    "made by another version of the emulator (format {version}, this one reads 2 to {STATE_VERSION})"
                )));
            }
        };
        if snapshot.bios_hash != fnv1a(&self.bus.bios) {
            return Err(Error::StateBiosMismatch);
        }
        let mut bus = snapshot.bus;
        bus.bios = std::mem::take(&mut self.bus.bios);
        bus.cdrom.disc = self.bus.cdrom.disc.take();
        bus.sio.cards = std::mem::take(&mut self.bus.sio.cards);
        // Which controllers are plugged in is a setting, not part of the
        // state (older states had the second one unplugged).
        for (pad, current) in bus.sio.pads.iter_mut().zip(&self.bus.sio.pads) {
            pad.connected = current.connected;
        }
        bus.gpu.set_scale(self.bus.gpu.scale());
        match snapshot.timing {
            Some(t) => {
                bus.gpu.drawing = t.gpu_drawing;
                bus.gpu_list = t.gpu_list;
                bus.spu.capture = t.spu_capture;
            }
            None => bus.settle(),
        }
        if self.bus.gpu.recording() {
            bus.gpu.set_recording(true);
            if self.bus.gpu.deferred() {
                bus.gpu.set_deferred(self.bus.gpu.take_fetch());
            }
        }
        self.bus = bus;
        self.cpu = snapshot.cpu;
        self.apply_enhancements();
        if let Some(jit) = &mut self.jit {
            jit.clear();
        }
        self.cpu.set_debug_registers(snapshot.cpu_debug);
        self.exe = None;
        Ok(())
    }

    /// The memory card contents, if the game wrote to it since the last call.
    pub fn memory_card_if_written(&mut self, slot: usize) -> Option<&[u8]> {
        let card = self.bus.sio.cards.get_mut(slot)?.as_mut()?;
        card.take_dirty().then_some(&card.data[..])
    }

    /// Starts logging the CD-ROM's commands (diagnostics), or returns and
    /// clears what was logged.
    pub fn debug_cd_log(&mut self) -> Vec<String> {
        self.bus.cdrom.take_log()
    }

    /// Diagnostics: records the CPU's stores to a word (physical address),
    /// read back with `debug_watched`.
    pub fn debug_watch(&mut self, addr: Option<u32>) {
        self.cpu.watch = addr.map(|a| a & 0x1fff_fffc);
    }

    /// The stores recorded since the last call: pc, address, value.
    pub fn debug_watched(&mut self) -> Vec<(u32, u32, u32)> {
        std::mem::take(&mut self.cpu.watched)
    }

    /// A one-line summary of the machine state, for debugging.
    pub fn debug_summary(&mut self) -> String {
        format!(
            "pc={:08x} fault={:08x?} {} mdec={:08x} dma2={:08x} i_stat={:03x} gpustat={:08x} drawing={} display={}x{} {}",
            self.cpu.pc,
            self.cpu.last_fault,
            self.bus.cdrom.debug(),
            self.bus.mdec.status(),
            self.bus.dma.channel(dma::Port::MdecOut).control,
            self.bus.irq.read(0),
            self.bus.gpu.status(),
            self.bus.gpu.drawing(),
            self.frame_size.0,
            self.frame_size.1,
            self.bus.gpu.debug_display(),
        )
    }

    /// Overrides the licence region the CD drive reports ('A', 'E', 'I'),
    /// for testing BIOS region checks.
    pub fn debug_set_licence_letter(&mut self, letter: u8) {
        self.bus.cdrom.region = letter;
    }

    /// Page flips (display area changes) since the last call: a game's own
    /// frame rate, for testing.
    pub fn take_flips(&mut self) -> u32 {
        std::mem::take(&mut self.bus.gpu.flips)
    }

    /// Reads memory as the CPU would, for debugging.
    pub fn debug_read32(&mut self, addr: u32) -> u32 {
        self.bus.load32(addr)
    }

    /// A checksum of the whole machine state, to check that two systems
    /// fed the same input stay identical (netplay).
    pub fn state_checksum(&self) -> u64 {
        fnv1a(&self.save_state())
    }

    pub fn memory_card(&self, slot: usize) -> Option<&[u8]> {
        self.bus.sio.cards.get(slot)?.as_ref().map(|c| &c.data[..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM the emulator takes for OpenBIOS, enough to build a machine.
    fn fake_bios() -> Vec<u8> {
        let mut rom = vec![0; BIOS_SIZE];
        rom[0x78..0x80].copy_from_slice(b"OpenBIOS");
        rom
    }

    #[test]
    fn save_states_keep_the_debug_registers() {
        let mut system = System::new(&fake_bios()).unwrap();
        system.cpu.set_debug_registers([1, 2, 3, 0x8007_b56c, 5]);
        let state = system.save_state();
        let mut other = System::new(&fake_bios()).unwrap();
        other.load_state(&state).unwrap();
        assert_eq!(other.cpu.debug_registers(), [1, 2, 3, 0x8007_b56c, 5]);
    }

    /// A fresh machine's state in an older format: format 4 is format 3
    /// followed by the timing (zero, nothing, zero: a byte each in
    /// postcard), format 3 format 2 followed by the five debug registers
    /// (zeros, a byte each).
    fn older_state(version: u32) -> Vec<u8> {
        let system = System::new(&fake_bios()).unwrap();
        let mut state = system.save_state();
        let cut = match version {
            3 => 3,
            2 => 3 + 5,
            _ => unreachable!(),
        };
        assert!(state[state.len() - cut..].iter().all(|&b| b == 0));
        state.truncate(state.len() - cut);
        let at = STATE_MAGIC.len();
        state[at..at + 4].copy_from_slice(&version.to_le_bytes());
        state
    }

    /// Format 2 and 3 states (before the debug registers, the timing) still
    /// load.
    #[test]
    fn older_states_load() {
        for version in [2, 3] {
            let mut other = System::new(&fake_bios()).unwrap();
            other.load_state(&older_state(version)).unwrap();
            assert_eq!(other.cpu.debug_registers(), [0; 5]);
        }
    }

    /// The timing comes back with the state: a restored machine goes on as
    /// the saved one would.
    #[test]
    fn save_states_keep_the_timing() {
        let mut system = System::new(&fake_bios()).unwrap();
        system.bus.gpu.drawing = 12_345;
        system.bus.gpu_list = Some(678);
        system.bus.spu.capture = 0x1ab;
        let state = system.save_state();
        let mut other = System::new(&fake_bios()).unwrap();
        other.load_state(&state).unwrap();
        assert_eq!(other.bus.gpu.drawing, 12_345);
        assert_eq!(other.bus.gpu_list, Some(678));
        assert_eq!(other.bus.spu.capture, 0x1ab);
        assert_eq!(other.save_state(), state);
    }

    #[test]
    fn states_of_something_else_are_refused() {
        let system = System::new(&fake_bios()).unwrap();
        let mut state = system.save_state();
        assert!(state.starts_with(STATE_MAGIC));
        state[..8].copy_from_slice(b"NOTOURS\0");
        let mut other = System::new(&fake_bios()).unwrap();
        assert!(other.load_state(&state).is_err());
    }
}
