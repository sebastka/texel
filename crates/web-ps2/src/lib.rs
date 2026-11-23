//! WebAssembly bindings of the PlayStation 2 emulator, used from a Web
//! Worker. A separate module from the PlayStation's, loaded only for the
//! PS2; its interface follows the PlayStation's where they overlap, so the
//! worker drives both the same way.
//!
//! So far: a BIOS boots to its browser, or a game's disc (straight to its
//! executable), with two controllers, two memory cards and sound (48 kHz;
//! with reverb), and save states.
//!
//! The GS draws in software (`new`, pictures read with `frame`), or on the
//! GPU through WebGPU (`withGpu`, pictures shown on the canvas given, with
//! `present`).

#[cfg(target_arch = "wasm32")]
mod jit;

use texel_ps2::Ps2;
use texel_ps2::bus::FRAME_CYCLES;
#[cfg(target_arch = "wasm32")]
use texel_ps2::gs::Rendering;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Ps2Emulator {
    ps2: Ps2,
    bios: Vec<u8>,
    frame: Vec<u8>,
    size: (usize, usize),
    /// The EE's code is compiled (see `setCompiler`), VU1's microprograms
    /// too (`setVuCompiler`).
    compiler: bool,
    vu_compiler: bool,
    audio: Vec<i16>,
    #[cfg(target_arch = "wasm32")]
    gpu: Option<texel_ps2_gpu::GpuGs>,
}

#[wasm_bindgen]
impl Ps2Emulator {
    #[wasm_bindgen(constructor)]
    pub fn new(bios: &[u8]) -> Result<Ps2Emulator, JsError> {
        if texel_ps2::bios::identify(bios).is_none() {
            return Err(JsError::new("not a PlayStation 2 BIOS"));
        }
        Ok(Self {
            ps2: Ps2::new(bios),
            bios: bios.to_vec(),
            frame: Vec::new(),
            size: (0, 0),
            compiler: false,
            vu_compiler: false,
            audio: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            gpu: None,
        })
    }

    /// With the GS on the GPU, showing pictures on `canvas` (which must have
    /// no context yet): fails without WebGPU.
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen(js_name = withGpu)]
    pub async fn with_gpu(bios: Vec<u8>, canvas: web_sys::OffscreenCanvas) -> Result<Ps2Emulator, JsError> {
        let mut emu = Self::new(&bios)?;
        let gpu = texel_ps2_gpu::GpuGs::with_surface(texel_ps2_gpu::wgpu::SurfaceTarget::OffscreenCanvas(canvas))
            .await
            .map_err(|e| JsError::new(&e))?;
        emu.ps2.bus.gs.rendering = Rendering::Record;
        emu.gpu = Some(gpu);
        Ok(emu)
    }

    /// Compiles the EE's code to WebAssembly as it runs (`true`), or
    /// interprets it all (`false`). Fails where WebAssembly modules cannot
    /// be made at run time.
    #[wasm_bindgen(js_name = setCompiler)]
    pub fn set_compiler(&mut self, on: bool) -> Result<(), JsError> {
        self.compiler = on;
        self.ps2.ee.jit = None;
        #[cfg(target_arch = "wasm32")]
        if on {
            let backend = jit::WebBackend::new().map_err(|e| JsError::new(&format!("{e:?}")))?;
            self.ps2.ee.jit = Some(Box::new(texel_ps2::ee::jit::Jit::new(Box::new(backend))));
        }
        Ok(())
    }

    /// Compiles the VUs' microprograms to WebAssembly (`true`), or
    /// interprets them (`false`). Fails where WebAssembly modules cannot be
    /// made at run time.
    #[wasm_bindgen(js_name = setVuCompiler)]
    pub fn set_vu_compiler(&mut self, on: bool) -> Result<(), JsError> {
        self.vu_compiler = on;
        self.ps2.bus.vu0.jit = None;
        self.ps2.bus.vu1.jit = None;
        #[cfg(target_arch = "wasm32")]
        if on {
            for vu in [&mut self.ps2.bus.vu0, &mut self.ps2.bus.vu1] {
                let backend = jit::WebBackend::new().map_err(|e| JsError::new(&format!("{e:?}")))?;
                vu.jit = Some(Box::new(texel_ps2::vu_jit::VuJit::new(Box::new(backend))));
            }
        }
        Ok(())
    }

    /// What the VUs' compiler did, for diagnostics.
    #[wasm_bindgen(js_name = vuCompilerStats)]
    pub fn vu_compiler_stats(&self) -> String {
        let stats = |vu: &texel_ps2::vu::Vu| vu.jit.as_ref().map_or("off".to_owned(), |j| format!("{:?}", j.stats));
        format!("VU0 {}; VU1 {}", stats(&self.ps2.bus.vu0), stats(&self.ps2.bus.vu1))
    }

    /// What the compiler did, for diagnostics.
    #[wasm_bindgen(js_name = compilerStats)]
    pub fn compiler_stats(&self) -> String {
        match &self.ps2.ee.jit {
            Some(j) => format!("{:?}", j.stats),
            None => "off".into(),
        }
    }

    /// A hash of the EE's registers and memory and of the GS memory: equal
    /// on two runs when they went the same way.
    #[wasm_bindgen(js_name = stateHash)]
    pub fn state_hash(&self) -> String {
        // FNV-1a, 64 bits.
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
            }
        };
        let ee = &self.ps2.ee;
        for r in ee.gpr {
            eat(&r.to_le_bytes());
        }
        eat(&ee.pc.to_le_bytes());
        eat(&ee.cycles.to_le_bytes());
        eat(&self.ps2.bus.ram);
        for w in &self.ps2.bus.gs.mem.words {
            eat(&w.to_le_bytes());
        }
        // The vector units: registers, flags, data memory.
        for vu in [&self.ps2.bus.vu0, &self.ps2.bus.vu1] {
            for r in vu.vf.iter().flatten() {
                eat(&r.to_le_bytes());
            }
            for r in vu.vi {
                eat(&r.to_le_bytes());
            }
            for r in vu
                .acc
                .iter()
                .chain([&vu.q, &vu.p, &vu.i, &vu.r, &vu.mac, &vu.status, &vu.clip])
            {
                eat(&r.to_le_bytes());
            }
            eat(&vu.data);
        }
        format!("{h:016x}")
    }

    /// VU1 instructions run since the start (diagnostics).
    #[wasm_bindgen(js_name = vu1Steps)]
    pub fn vu1_steps(&self) -> f64 {
        self.ps2.bus.vu1.steps as f64
    }

    /// The GPU's name, when the GS is on it (empty when the browser does
    /// not tell).
    #[wasm_bindgen(getter)]
    pub fn renderer(&self) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &self.gpu {
            return Some(gpu.adapter.clone());
        }
        None
    }

    /// Saves the machine's state (not the BIOS, the disc or the memory
    /// cards): a promise of its bytes (the GS memory is read back from the
    /// GPU when it draws there).
    #[wasm_bindgen(js_name = saveState)]
    pub fn save_state(&self) -> js_sys::Promise {
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &self.gpu {
            let core = self.ps2.save_core();
            let read = gpu.read_state();
            return wasm_bindgen_futures::future_to_promise(async move {
                let (vram, clut) = read.await;
                Ok(js_sys::Uint8Array::from(&Ps2::pack_state(&core, &vram, &clut)[..]).into())
            });
        }
        let state: JsValue = js_sys::Uint8Array::from(&self.ps2.save_state()[..]).into();
        js_sys::Promise::resolve(&state)
    }

    /// Restores a state saved with `saveState`, with the same BIOS (the
    /// disc and memory cards stay as they are).
    #[wasm_bindgen(js_name = loadState)]
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), JsError> {
        self.ps2.load_state(data).map_err(|e| JsError::new(&e))?;
        self.audio.clear();
        // Fresh blocks for the restored memory, and programs.
        self.set_compiler(self.compiler)?;
        self.set_vu_compiler(self.vu_compiler)?;
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.restore(&self.ps2.bus.gs.mem.words, self.ps2.bus.gs.clut());
        }
        Ok(())
    }

    /// Puts a game's disc in the drive (`files`: its files' names and
    /// sizes, `read`: `(file, offset, length) => Uint8Array`), and boots it
    /// directly. Returns its executable's path.
    #[wasm_bindgen(js_name = insertDisc)]
    pub fn insert_disc(
        &mut self,
        cue: Option<String>,
        names: Vec<String>,
        sizes: Vec<f64>,
        read: js_sys::Function,
    ) -> Result<String, JsError> {
        let disc = texel_web_disc::open(cue, &names, &sizes, read)?;
        let cnf = self.ps2.insert_disc(disc, true).map_err(|e| JsError::new(&e))?;
        Ok(cnf.boot)
    }

    /// Puts a memory card in slot `slot` (0 or 1): its image (8 MiB and its
    /// ECC), or a fresh, unformatted card.
    #[wasm_bindgen(js_name = insertMemoryCard)]
    pub fn insert_memory_card(&mut self, slot: usize, data: Option<Vec<u8>>) {
        if let Some(s) = self.ps2.bus.iop.sio2.cards.get_mut(slot) {
            *s = Some(Box::new(texel_ps2::memcard::MemoryCard::new(data.as_deref())));
        }
    }

    /// The memory card's image, if it was written since the last call.
    #[wasm_bindgen(js_name = takeWrittenMemoryCard)]
    pub fn take_written_memory_card(&mut self, slot: usize) -> Option<Vec<u8>> {
        let card = self.ps2.bus.iop.sio2.cards.get_mut(slot)?.as_mut()?;
        card.take_written().map(<[u8]>::to_vec)
    }

    pub fn reset(&mut self) {
        let rendering = self.ps2.bus.gs.rendering;
        let disc = self.ps2.bus.iop.cdvd.disc.take();
        let cards = std::mem::take(&mut self.ps2.bus.iop.sio2.cards);
        self.ps2 = Ps2::new(&self.bios);
        self.ps2.bus.iop.sio2.cards = cards;
        // The same disc, booted again.
        if let Some(d) = disc {
            let _ = self.ps2.insert_disc(d.medium, true);
        }
        self.ps2.bus.gs.rendering = rendering;
        // Fresh blocks for the fresh memory.
        let _ = self.set_compiler(self.compiler);
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.clear();
        }
    }

    /// Runs a frame's worth of time (1/60 s), then reads the picture out
    /// (in software) or sends the frame's drawing to the GPU.
    #[wasm_bindgen(js_name = runFrame)]
    pub fn run_frame(&mut self) {
        self.ps2.run(FRAME_CYCLES);
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.submit(&self.ps2.bus.gs.take_commands());
            let (w, h) = texel_ps2_gpu::GpuGs::display_size(&self.ps2.bus.gs.privileged);
            self.size = (w as usize, h as usize);
            return;
        }
        self.size = self.ps2.bus.gs.display(&mut self.frame);
    }

    /// With the GS on the GPU: shows the last frame on the canvas, sized
    /// `width`×`height` (bilinear when `smooth`).
    pub fn present(&mut self, width: u32, height: u32, smooth: bool) {
        #[cfg(target_arch = "wasm32")]
        if let Some(gpu) = &mut self.gpu {
            gpu.present(&self.ps2.bus.gs.privileged, width, height, smooth);
        }
        let _ = (width, height, smooth);
    }

    #[wasm_bindgen(getter, js_name = frameWidth)]
    pub fn frame_width(&self) -> usize {
        self.size.0
    }

    #[wasm_bindgen(getter, js_name = frameHeight)]
    pub fn frame_height(&self) -> usize {
        self.size.1
    }

    /// The last frame, RGBA (software GS only: empty on the GPU).
    pub fn frame(&self) -> js_sys::Uint8ClampedArray {
        let len = (self.size.0 * self.size.1 * 4).min(self.frame.len());
        js_sys::Uint8ClampedArray::from(&self.frame[..len])
    }

    /// Draws nothing from now on (software GS only): for measuring the
    /// rest of the console.
    #[wasm_bindgen(js_name = disableGraphics)]
    pub fn disable_graphics(&mut self) {
        self.ps2.bus.gs.rendering = texel_ps2::gs::Rendering::Off;
    }

    /// The sound since the last call: interleaved stereo at 48 kHz.
    #[wasm_bindgen(js_name = takeAudio)]
    pub fn take_audio(&mut self) -> js_sys::Int16Array {
        self.audio.clear();
        self.ps2.bus.iop.spu2.drain_output(&mut self.audio);
        js_sys::Int16Array::from(&self.audio[..])
    }

    /// Frames are paced at 60 per second for now, PAL consoles included.
    #[wasm_bindgen(getter, js_name = isPal)]
    pub fn is_pal(&self) -> bool {
        false
    }

    #[wasm_bindgen(js_name = setResolutionScale)]
    pub fn set_resolution_scale(&mut self, _scale: usize) {}

    #[wasm_bindgen(js_name = setButtons)]
    pub fn set_buttons(&mut self, port: usize, pressed: u16) {
        if let Some(pad) = self.ps2.bus.iop.sio2.pads.get_mut(port) {
            pad.buttons = pressed;
        }
    }

    #[wasm_bindgen(js_name = setAxes)]
    pub fn set_axes(&mut self, port: usize, lx: u8, ly: u8, rx: u8, ry: u8) {
        if let Some(pad) = self.ps2.bus.iop.sio2.pads.get_mut(port) {
            pad.axes = [lx, ly, rx, ry];
        }
    }

    #[wasm_bindgen(js_name = pressAnalogButton)]
    pub fn press_analog_button(&mut self, port: usize) -> bool {
        match self.ps2.bus.iop.sio2.pads.get_mut(port) {
            Some(pad) => {
                pad.press_analog_button();
                pad.analog
            }
            None => false,
        }
    }

    #[wasm_bindgen(js_name = isAnalog)]
    pub fn is_analog(&self, port: usize) -> bool {
        self.ps2.bus.iop.sio2.pads.get(port).is_some_and(|p| p.analog)
    }

    /// Small motor (high byte) and large (low byte).
    pub fn rumble(&self, port: usize) -> u16 {
        self.ps2
            .bus
            .iop
            .sio2
            .pads
            .get(port)
            .map_or(0, |p| u16::from(p.rumble.0) << 8 | u16::from(p.rumble.1))
    }

    /// The BIOS, as the player shows it.
    #[wasm_bindgen(js_name = biosInfo)]
    pub fn bios_info(&self) -> String {
        match texel_ps2::bios::identify(&self.bios) {
            Some(i) => format!("PlayStation 2 BIOS {} ({}, {})", i.version, i.date, i.region.as_str()),
            None => String::new(),
        }
    }
}
