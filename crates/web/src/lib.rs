//! WebAssembly bindings of the emulation core, used from a Web Worker.
//!
//! Disc images are not loaded in memory: sectors are read on demand (see
//! `texel-web-disc`).
//!
//! Frames are drawn in software (`new`, pictures read with `frame`), or,
//! scaled, on the GPU through WebGPU (`withGpu`, pictures shown on the
//! canvas given, with `present`): the native resolution is still drawn in
//! software.

#[cfg(target_arch = "wasm32")]
mod jit;

use texel_disc::Subchannel;
use texel_ps1::System;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Emulator {
    system: System,
    audio: Vec<i16>,
    /// Room for a picture wrapping around VRAM's edges (see `picture`).
    wrapped: Vec<u16>,
    #[cfg(target_arch = "wasm32")]
    gpu: Option<texel_ps1_gpu::GpuRenderer>,
}

#[wasm_bindgen]
impl Emulator {
    #[wasm_bindgen(constructor)]
    pub fn new(bios: &[u8]) -> Result<Emulator, JsError> {
        Ok(Self {
            system: System::new(bios)?,
            audio: Vec::with_capacity(4096),
            wrapped: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            gpu: None,
        })
    }

    /// Drawing scaled on the GPU, showing pictures on `canvas` (which must
    /// have no context yet): fails without WebGPU.
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen(js_name = withGpu)]
    pub async fn with_gpu(bios: Vec<u8>, canvas: web_sys::OffscreenCanvas) -> Result<Emulator, JsError> {
        let mut emu = Self::new(&bios)?;
        let gpu = texel_ps1_gpu::GpuRenderer::with_surface(texel_ps1_gpu::wgpu::SurfaceTarget::OffscreenCanvas(canvas))
            .await
            .map_err(|e| JsError::new(&e))?;
        emu.system.set_gpu_recording(true);
        emu.gpu = Some(gpu);
        Ok(emu)
    }

    /// Compiles the CPU's code to WebAssembly as it runs (`true`), or
    /// interprets it all (`false`). Fails where WebAssembly modules cannot
    /// be made at run time.
    #[wasm_bindgen(js_name = setCompiler)]
    pub fn set_compiler(&mut self, on: bool) -> Result<(), JsError> {
        self.system.set_compiler(None);
        #[cfg(target_arch = "wasm32")]
        if on {
            let backend = jit::WebBackend::new().map_err(|e| JsError::new(&format!("{e:?}")))?;
            self.system.set_compiler(Some(Box::new(backend)));
        }
        let _ = on;
        Ok(())
    }

    /// Diagnostics: one step (a compiled block or an instruction): the
    /// cycles run so far.
    #[wasm_bindgen(js_name = debugStep)]
    pub fn debug_step(&mut self) -> f64 {
        self.system.debug_step() as f64
    }

    /// Diagnostics: the CPU's state's checksum (hex) and pc.
    #[wasm_bindgen(js_name = debugCpu)]
    pub fn debug_cpu(&self) -> String {
        let (h, pc) = self.system.debug_cpu();
        format!("{h:016x} {pc:08x}")
    }

    /// Diagnostics: the cycles between the peripherals' catching up.
    #[wasm_bindgen(js_name = debugSetSlice)]
    pub fn debug_set_slice(&mut self, cycles: u32) {
        self.system.debug_set_slice(cycles);
    }

    /// Diagnostics: the CPU's state, a field per line.
    #[wasm_bindgen(js_name = debugCpuFields)]
    pub fn debug_cpu_fields(&self) -> String {
        self.system
            .debug_cpu_fields()
            .iter()
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect()
    }

    /// What the compiler did, for diagnostics.
    #[wasm_bindgen(js_name = compilerStats)]
    pub fn compiler_stats(&self) -> String {
        match self.system.compiler_stats() {
            Some(s) => format!("{s:?}"),
            None => "off".into(),
        }
    }

    /// The GPU's name, when drawing there (empty when the browser does not
    /// tell).
    #[wasm_bindgen(getter)]
    pub fn renderer(&self) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &self.gpu {
            return Some(gpu.adapter.clone());
        }
        None
    }

    /// On the GPU: shows the last frame on the canvas, sized `width`×`height`
    /// (bilinear when `smooth`), with a half hidden (netplay's split screen:
    /// 1 top, 2 bottom, 3 left, 4 right; 0 none).
    pub fn present(&mut self, width: u32, height: u32, smooth: bool, hidden: u32) {
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.present(
                self.system.gpu_display(),
                self.system.vram(),
                width,
                height,
                smooth,
                hidden,
            );
        }
        let _ = (width, height, smooth, hidden);
    }

    /// Inserts a disc. `cue` is the CUE sheet, or `undefined` for a single
    /// ISO/BIN file; `names` and `sizes` describe the image files (in the
    /// order of the CUE sheet); `read` fetches bytes from them. `sbi` is the
    /// disc's SBI file, for discs protected by LibCrypt.
    #[wasm_bindgen(js_name = insertDisc)]
    pub fn insert_disc(
        &mut self,
        cue: Option<String>,
        names: Vec<String>,
        sizes: Vec<f64>,
        read: js_sys::Function,
        sbi: Option<Vec<u8>>,
    ) -> Result<(), JsError> {
        let mut disc = texel_web_disc::open(cue, &names, &sizes, read)?;
        if let Some(sbi) = sbi {
            disc = disc.with_subchannel(Subchannel::parse_sbi(&sbi)?);
        }
        self.system.insert_disc(Some(disc));
        Ok(())
    }

    /// Opens the lid, to change discs; `insertDisc` closes it.
    #[wasm_bindgen(js_name = openLid)]
    pub fn open_lid(&mut self) {
        self.system.open_lid();
    }

    pub fn reset(&mut self) {
        self.system.reset();
    }

    /// Emulates one video frame.
    #[wasm_bindgen(js_name = runFrame)]
    pub fn run_frame(&mut self) {
        self.system.run_frame();
        self.system.take_audio(&mut self.audio);
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.submit(&self.system.take_gpu_commands());
        }
    }

    #[wasm_bindgen(getter, js_name = frameWidth)]
    pub fn frame_width(&self) -> usize {
        self.system.frame_size().0
    }

    #[wasm_bindgen(getter, js_name = frameHeight)]
    pub fn frame_height(&self) -> usize {
        self.system.frame_size().1
    }

    /// The last frame, RGBA8888 (made now: `picture` is cheaper).
    pub fn frame(&mut self) -> js_sys::Uint8ClampedArray {
        js_sys::Uint8ClampedArray::from(self.system.frame().0)
    }

    /// The last frame as VRAM holds it, to show without converting it: where
    /// its 15-bit pixels are in this module's memory (see `layout`), or
    /// nothing when only `frame` has it.
    pub fn picture(&mut self) -> Option<Vec<u32>> {
        self.system.picture(&mut self.wrapped).map(|p| layout(&p))
    }

    /// Interleaved stereo samples at 44.1 kHz produced since the last call.
    #[wasm_bindgen(js_name = takeAudio)]
    pub fn take_audio(&mut self) -> js_sys::Int16Array {
        let out = js_sys::Int16Array::from(&self.audio[..]);
        self.audio.clear();
        out
    }

    /// Saves the machine's state (not the BIOS, disc or memory cards).
    #[wasm_bindgen(js_name = saveState)]
    pub fn save_state(&self) -> Vec<u8> {
        self.system.save_state()
    }

    /// Restores a state saved with `saveState`, with the same BIOS.
    #[wasm_bindgen(js_name = loadState)]
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), JsError> {
        self.system.load_state(data)?;
        Ok(())
    }

    /// Precise geometry (with the GPU): triangles the GTE projected are
    /// drawn from its exact positions, textures in perspective. What the
    /// console computes does not change.
    #[wasm_bindgen(js_name = setPreciseGeometry)]
    pub fn set_precise_geometry(&mut self, on: bool) {
        self.system.set_precise_geometry(on);
    }

    /// Widescreen: 3D squeezed for a 16:9 picture (changes what the console
    /// computes: not in netplay).
    #[wasm_bindgen(js_name = setWidescreen)]
    pub fn set_widescreen(&mut self, on: bool) {
        self.system.set_widescreen(on);
    }

    /// Draws the scaled picture elsewhere (`true`): its drawing is recorded
    /// (`takeDrawing`) for a `Replayer` in another worker, on another core;
    /// the native resolution is still drawn here. Without the GPU only.
    #[wasm_bindgen(js_name = setDrawingElsewhere)]
    pub fn set_drawing_elsewhere(&mut self, on: bool) {
        self.system.set_gpu_recording(on);
    }

    /// The drawing recorded since the last call, and the displayed
    /// picture's place, encoded for `Replayer::apply`.
    #[wasm_bindgen(js_name = takeDrawing)]
    pub fn take_drawing(&mut self) -> Vec<u8> {
        let commands = self.system.take_gpu_commands_for_replay();
        let display = self.system.gpu_display();
        postcard::to_stdvec(&(commands, display)).unwrap_or_default()
    }

    /// Internal resolution multiplier, 1 to 4.
    #[wasm_bindgen(js_name = setResolutionScale)]
    pub fn set_resolution_scale(&mut self, scale: usize) {
        self.system.set_resolution_scale(scale);
    }

    #[wasm_bindgen(getter, js_name = isPal)]
    pub fn is_pal(&self) -> bool {
        self.system.is_pal()
    }

    /// Sets the pressed buttons of a controller (bit layout of the
    /// controller protocol, 1 = pressed).
    #[wasm_bindgen(js_name = setButtons)]
    pub fn set_buttons(&mut self, port: usize, pressed: u16) {
        self.system.set_buttons(port, pressed);
    }

    /// Sets the analog sticks: left X, left Y, right X, right Y (0-255,
    /// 128 at rest).
    #[wasm_bindgen(js_name = setAxes)]
    pub fn set_axes(&mut self, port: usize, lx: u8, ly: u8, rx: u8, ry: u8) {
        self.system.set_axes(port, [lx, ly, rx, ry]);
    }

    /// Presses the controller's Analog button; returns whether analog mode is on.
    #[wasm_bindgen(js_name = pressAnalogButton)]
    pub fn press_analog_button(&mut self, port: usize) -> bool {
        self.system.press_analog_button(port)
    }

    #[wasm_bindgen(js_name = isAnalog)]
    pub fn is_analog(&self, port: usize) -> bool {
        self.system.is_analog(port)
    }

    /// Rumble motors as `small << 8 | large`.
    pub fn rumble(&self, port: usize) -> u16 {
        let (small, large) = self.system.rumble(port);
        (u16::from(small) << 8) | u16::from(large)
    }

    /// Inserts a memory card; with no data, a freshly formatted one.
    #[wasm_bindgen(js_name = insertMemoryCard)]
    pub fn insert_memory_card(&mut self, slot: usize, data: Option<Vec<u8>>) -> Result<(), JsError> {
        Ok(self.system.set_memory_card(slot, Some(data))?)
    }

    /// The memory card image, if one is inserted.
    #[wasm_bindgen(js_name = memoryCard)]
    pub fn memory_card(&self, slot: usize) -> Option<Vec<u8>> {
        self.system.memory_card(slot).map(<[u8]>::to_vec)
    }

    /// A checksum of the machine state, folded to 32 bits: equal on two
    /// machines that ran in step.
    #[wasm_bindgen(js_name = stateChecksum)]
    pub fn state_checksum(&self) -> u32 {
        let h = self.system.state_checksum();
        (h ^ (h >> 32)) as u32
    }

    /// The memory card image if the game wrote to it since the last call.
    #[wasm_bindgen(js_name = takeWrittenMemoryCard)]
    pub fn take_written_memory_card(&mut self, slot: usize) -> Option<Vec<u8>> {
        self.system.memory_card_if_written(slot).map(<[u8]>::to_vec)
    }

    /// The BIOS version and region, as JSON.
    #[wasm_bindgen(js_name = biosInfo)]
    pub fn bios_info(&self) -> String {
        let info = self.system.bios_info();
        format!(
            r#"{{"version":{},"region":"{}"}}"#,
            info.version.as_ref().map_or("null".to_owned(), |v| format!("{v:?}")),
            info.region.as_str()
        )
    }
}

/// Draws the scaled picture from an `Emulator`'s recorded drawing (see
/// `setDrawingElsewhere`), in another worker.
#[wasm_bindgen]
pub struct Replayer {
    replay: texel_ps1::gpu_record::Replay,
    display: Option<texel_ps1::gpu_record::Display>,
    frame: Vec<u8>,
    size: (usize, usize),
    /// Room for a picture wrapping around VRAM's edges (see `picture`).
    wrapped: Vec<u16>,
}

/// A picture's place in this module's memory, for JavaScript to read it
/// there (a view on the memory, no copy): the address of its pixels (bytes),
/// how many there are (16-bit), the length of their rows, and the picture's
/// position and size in them: `[address, length, stride, x, y, w, h]`. The
/// view is valid until the module runs again.
fn layout(p: &texel_ps1::Picture<'_>) -> Vec<u32> {
    // In WebAssembly, an address is an offset in the memory.
    [p.pixels.as_ptr() as usize, p.pixels.len(), p.stride, p.x, p.y, p.w, p.h]
        .iter()
        .map(|&v| v as u32)
        .collect()
}

#[wasm_bindgen]
impl Replayer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Replayer {
        Self {
            replay: texel_ps1::gpu_record::Replay::new(),
            display: None,
            frame: Vec::new(),
            size: (0, 0),
            wrapped: Vec::new(),
        }
    }

    /// Runs drawing from `takeDrawing`.
    pub fn apply(&mut self, drawing: &[u8]) -> Result<(), JsError> {
        let (commands, display): (Vec<texel_ps1::gpu_record::Command>, texel_ps1::gpu_record::Display) =
            postcard::from_bytes(drawing).map_err(|e| JsError::new(&e.to_string()))?;
        self.replay.apply(&commands);
        self.display = Some(display);
        Ok(())
    }

    /// Renders the displayed picture (read with `frame`, sized by
    /// `frameWidth` and `frameHeight`).
    pub fn render(&mut self) {
        if let Some(d) = self.display {
            self.size = self.replay.render_display(d, &mut self.frame);
        }
    }

    #[wasm_bindgen(getter, js_name = frameWidth)]
    pub fn frame_width(&self) -> usize {
        self.size.0
    }

    #[wasm_bindgen(getter, js_name = frameHeight)]
    pub fn frame_height(&self) -> usize {
        self.size.1
    }

    /// The last picture rendered, RGBA8888.
    pub fn frame(&self) -> js_sys::Uint8ClampedArray {
        js_sys::Uint8ClampedArray::from(&self.frame[..])
    }

    /// The displayed picture as the scaled VRAM holds it, to show without
    /// converting it (see `Emulator::picture`); nothing when only `render`
    /// and `frame` give it.
    pub fn picture(&mut self) -> Option<Vec<u32>> {
        let d = self.display?;
        self.replay.picture(d, &mut self.wrapped).map(|p| layout(&p))
    }
}

impl Default for Replayer {
    fn default() -> Self {
        Self::new()
    }
}
