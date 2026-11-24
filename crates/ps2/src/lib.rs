//! PlayStation 2 emulation, in progress.
//!
//! So far: the Emotion Engine's CPU, its DMA, the vector units, the VIFs,
//! the GIF and the Graphics Synthesizer (in software); the IOP (the
//! PlayStation's CPU, from `texel-ps1`) with its devices; and the SIF
//! between them. A retail BIOS boots to its browser. See [`Ps2`].

pub mod bios;
pub mod bus;
pub mod cdvd;
pub mod ee;
pub mod gif;
pub mod gs;
pub mod intc;
pub mod iop;
pub mod iop_printf;
pub mod ipu;
pub mod memcard;
pub mod sif;
pub mod sio2;
pub mod spu2;
pub mod timers;
pub mod vif;
pub mod vu;
pub mod vu1q;
pub mod vu_jit;

use bus::Bus;
use ee::Ee;
use texel_ps1::cpu::{Cpu, CpuBus};

/// Instructions run between updates of the timers and interrupt lines
/// (and the IOP's turn, in its slower cycles). See D50.
const SLICE: u64 = 512;

/// EE cycles a vertical blank's interrupt waits at most, the EE running
/// meanwhile, for a read of INTC_STAT (`Intc::fresh`).
const INTC_LATENCY: u64 = 32;

/// The machine, as far as it goes.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Ps2 {
    pub ee: Ee,
    pub bus: Bus,
    /// The IOP's CPU: the PlayStation's. Its memory is in `bus.iop`.
    pub iop: Cpu,
    /// IOP cycles owed: the IOP runs an eighth as fast as the EE.
    iop_budget: i64,
    /// Fast boot: the executable EELOAD is to start instead of the browser,
    /// until it does; and where EELOAD keeps its target.
    boot: Option<String>,
    boot_slot: Option<u32>,
    /// Fast boot could not change EELOAD's target: the browser starts.
    pub boot_failed: bool,
    /// What the IOP's modules print, when set (diagnostics).
    #[serde(skip)]
    pub iop_printf: Option<iop_printf::IopPrintf>,
    slices: u64,
}

/// Save states: this tag, the format's version, then the rest (`State`).
const STATE_MAGIC: &[u8; 8] = b"TEXELPS2";
const STATE_VERSION: u32 = 2;

#[derive(serde::Serialize)]
struct StateRef<'a> {
    bios_hash: u64,
    /// The machine, serialised apart (see `Ps2::save_core`).
    core: &'a [u8],
    gs_memory: &'a [u32],
    clut: &'a [u32],
}

#[derive(serde::Deserialize)]
struct State {
    bios_hash: u64,
    core: Vec<u8>,
    gs_memory: Vec<u32>,
    clut: Vec<u32>,
}

/// FNV-1a, 64 bits.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
    })
}

/// Where EELOAD runs, and how much room its boot target has (its own 16
/// bytes, and the test mode's after them).
const EELOAD: std::ops::Range<u32> = 0x0008_2000..0x0010_0000;
const EELOAD_TARGET_ROOM: usize = 32;

impl Ps2 {
    /// A machine at reset, with this BIOS image (4 MiB).
    pub fn new(bios: &[u8]) -> Self {
        let mut iop = Cpu::new();
        iop.prid = iop::IOP_PRID;
        let mut bus = Bus::new(bios);
        // The drive's EEPROM: a console of the BIOS's region.
        if let Some(info) = bios::identify(bios) {
            bus.iop.cdvd = cdvd::Cdvd::new(cdvd::ConsoleRegion::for_bios(info.region));
        }
        Self {
            ee: Ee::new(),
            bus,
            iop,
            iop_budget: 0,
            boot: None,
            boot_slot: None,
            boot_failed: false,
            iop_printf: None,
            slices: 0,
        }
    }

    /// Saves the machine's state (not the BIOS, the disc or the memory
    /// cards), with its GS memory and CLUT.
    /// Runs VU1's queued work (`vu1q`): before saving a state, which does
    /// not keep the queue. The machine that saves goes on from there, as a
    /// machine loading the state does.
    pub fn settle(&mut self) {
        self.bus.sync_vu1(crate::vu1q::Why::Save);
    }

    pub fn save_state(&self) -> Vec<u8> {
        Self::pack_state(&self.save_core(), &self.bus.gs.mem.words, self.bus.gs.clut())
    }

    /// The machine's state but its GS memory and CLUT, for `pack_state`:
    /// when the GPU draws, they are on the GPU, read back later.
    pub fn save_core(&self) -> Vec<u8> {
        let mut core = postcard::to_stdvec(self).expect("serialising to memory cannot fail");
        core.extend_from_slice(&fnv1a(&self.bus.bios).to_le_bytes());
        core
    }

    /// A state from `save_core`'s and the GS memory and CLUT.
    pub fn pack_state(core: &[u8], gs_memory: &[u32], clut: &[u32]) -> Vec<u8> {
        let (core, hash) = core.split_at(core.len() - 8);
        let mut out = Vec::with_capacity(core.len() + gs_memory.len() * 4 + 64);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        let state = StateRef {
            bios_hash: u64::from_le_bytes(hash.try_into().expect("8 bytes")),
            core,
            gs_memory,
            clut,
        };
        postcard::to_extend(&state, out).expect("serialising to memory cannot fail")
    }

    /// Restores a state from `save_state`. It must come from the same BIOS;
    /// the disc in the drive should be the one it was saved with. Compiled
    /// code is dropped: set up a compiler again (`ee.jit`) if one was on.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let body = data
            .strip_prefix(STATE_MAGIC.as_slice())
            .ok_or("not a Texel PlayStation 2 save state")?;
        let (version, body) = body.split_at_checked(4).ok_or("truncated")?;
        let version = u32::from_le_bytes(version.try_into().expect("4 bytes"));
        if version != STATE_VERSION {
            return Err(format!(
                "made by another version of the emulator (format {version}, this one reads {STATE_VERSION})"
            ));
        }
        let state: State = postcard::from_bytes(body).map_err(|e| e.to_string())?;
        if state.bios_hash != fnv1a(&self.bus.bios) {
            return Err("saved with another BIOS".into());
        }
        let mut new: Ps2 = postcard::from_bytes(&state.core).map_err(|e| e.to_string())?;
        if state.gs_memory.len() != new.bus.gs.mem.words.len() {
            return Err("the GS memory has the wrong size".into());
        }
        new.bus.gs.mem.words.copy_from_slice(&state.gs_memory);
        new.bus.gs.set_clut(&state.clut);
        new.bus.transplant(&mut self.bus);
        new.iop_printf = self.iop_printf.take();
        *self = new;
        Ok(())
    }

    /// Puts a disc in the drive (an image of 2048-byte sectors is taken
    /// for a DVD, others for a CD), and returns its SYSTEM.CNF. With
    /// `fast_boot`, the console starts the disc's executable itself: the
    /// BIOS's kernel starts EELOAD, as always, and EELOAD starts the game
    /// rather than the browser (no logo, no browser).
    pub fn insert_disc(&mut self, mut medium: cdvd::Medium, fast_boot: bool) -> Result<texel_disc::SystemCnf, String> {
        let cnf = texel_disc::read_system_cnf(&mut medium).map_err(|e| e.to_string())?;
        if !cnf.ps2 {
            return Err(format!("not a PlayStation 2 disc (it boots {})", cnf.boot));
        }
        let dvd = medium
            .layout
            .tracks
            .first()
            .is_some_and(|t| t.mode == texel_disc::TrackMode::Mode1Cooked);
        let kind = if dvd { cdvd::DiscKind::Dvd } else { cdvd::DiscKind::Cd };
        self.bus.iop.cdvd.insert(Some(cdvd::Inserted { medium, kind }));
        self.boot = fast_boot.then(|| cnf.boot.clone());
        Ok(cnf)
    }

    /// Fast boot, between slices: while the EE runs EELOAD, its target is
    /// the disc's executable (EELOAD is loaded again when the IOP restarts,
    /// so this is done each time); once the EE runs above EELOAD, the game
    /// has started.
    fn fast_boot(&mut self) {
        let pc = self.ee.pc & 0x1FFF_FFFF;
        if (EELOAD.end..0x0200_0000).contains(&pc) {
            self.boot = None;
            return;
        }
        if !EELOAD.contains(&pc) {
            return;
        }
        let slot = match self.boot_slot {
            Some(at) => at,
            None => {
                let area = &self.bus.ram[EELOAD.start as usize..EELOAD.end as usize];
                let Some(at) = area.windows(12).position(|w| w == b"rom0:OSDSYS\0") else {
                    return;
                };
                let room = &area[at..(at + EELOAD_TARGET_ROOM).min(area.len())];
                // Only the browser's target, then the test mode's (or
                // nothing): room for the disc's.
                let free = room.len() == EELOAD_TARGET_ROOM
                    && room[11..16].iter().all(|&b| b == 0)
                    && (room[16..].iter().all(|&b| b == 0) || room[16..].starts_with(b"rom0:TESTMODE\0"));
                if !free {
                    self.boot = None;
                    self.boot_failed = true;
                    return;
                }
                let at = EELOAD.start + at as u32;
                self.boot_slot = Some(at);
                at
            }
        };
        let at = slot as usize;
        if &self.bus.ram[at..at + 12] != b"rom0:OSDSYS\0" {
            return;
        }
        let path = self.boot.clone().unwrap_or_default();
        if path.len() >= EELOAD_TARGET_ROOM {
            self.boot = None;
            self.boot_failed = true;
            return;
        }
        for k in 0..EELOAD_TARGET_ROOM {
            self.bus
                .write8(slot + k as u32, path.as_bytes().get(k).copied().unwrap_or(0));
        }
    }

    /// The state of the controller in `port` (0 or 1): buttons pressed
    /// (`texel_ps1::button`), and the sticks (left X, left Y, right X,
    /// right Y).
    pub fn set_pad(&mut self, port: usize, buttons: u16, axes: [u8; 4]) {
        if let Some(pad) = self.bus.iop.sio2.pads.get_mut(port) {
            pad.buttons = buttons;
            pad.axes = axes;
        }
    }

    /// Whether the IOP spins in an idle loop: a jump to itself, with a NOP
    /// in its delay slot (the IOP kernel's idle thread), which only an
    /// interrupt leaves.
    fn iop_idle(&mut self) -> bool {
        let pc = self.iop.pc;
        let jump = if self.iop.in_delay_slot() {
            pc.wrapping_sub(4)
        } else {
            pc
        };
        let to_itself = 0x0800_0000 | ((jump & 0x0FFF_FFFF) >> 2);
        self.bus.iop.fetch(jump) == to_itself && self.bus.iop.fetch(jump.wrapping_add(4)) == 0
    }

    /// The EE, for `cycles` (only time passes in its idle loop).
    fn run_ee(&mut self, cycles: u64) {
        if self.ee.idling() {
            self.ee.skip(cycles);
        } else {
            self.ee.run(&mut self.bus, cycles);
        }
    }

    /// Runs about `cycles` EE cycles.
    pub fn run(&mut self, cycles: u64) {
        let end = self.ee.cycles + cycles;
        while self.ee.cycles < end {
            let mut left = SLICE;
            if self.bus.intc.fresh != 0 {
                // A vertical blank's interrupt, taken right after the next
                // read of INTC_STAT (or a few cycles): code polling it sees
                // the source, then the interrupt is taken, before the code
                // can do anything else (acknowledge it itself, say).
                self.ee.intc_watch = true;
                self.run_ee(INTC_LATENCY);
                self.ee.intc_watch = false;
                left -= INTC_LATENCY;
                self.bus.intc.fresh = 0;
                let (int0, int1) = (self.bus.intc.line(), self.bus.dmac_line());
                self.ee.check_interrupts(int0, int1);
            }
            self.run_ee(left);
            self.iop_budget += (SLICE / iop::IOP_CLOCK_DIVIDER) as i64;
            let mut first = true;
            if let Some(p) = &mut self.iop_printf {
                self.slices += 1;
                if self.slices.is_multiple_of(20_000) {
                    p.scan(&self.bus.iop.ram);
                }
            }
            while self.iop_budget > 0 {
                self.bus.iop.pc = self.iop.pc;
                if let Some(p) = &mut self.iop_printf {
                    p.check(&self.iop, &self.bus.iop.ram);
                }
                self.iop_budget -= i64::from(self.iop.step(&mut self.bus.iop));
                // After a first step (which takes a pending interrupt), the
                // idle loop's time passes without running it.
                if first && self.iop_idle() {
                    self.iop_budget = self.iop_budget.min(0);
                }
                first = false;
            }
            self.bus.pump_sif();
            self.bus.advance(SLICE);
            if self.boot.is_some() {
                self.fast_boot();
            }
            let (int0, int1) = (self.bus.intc.line(), self.bus.dmac_line());
            self.ee.check_interrupts(int0, int1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BIOS counting in a loop, storing the count in RAM.
    fn counting_bios() -> Vec<u8> {
        [
            0x3C08_8000u32, // lui t0, 0x8000
            0x2529_0001,    // addiu t1, t1, 1
            0xAD09_0100,    // sw t1, 0x100(t0)
            0x0BF0_0001,    // j 0xBFC00004
            0x0000_0000,    // nop
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
    }

    #[test]
    fn a_state_runs_on_as_the_machine_did() {
        let mut ps2 = Ps2::new(&counting_bios());
        ps2.run(100_000);
        let state = ps2.save_state();
        ps2.run(300_000);
        let after = ps2.save_state();
        assert_ne!(state, after);
        let mut other = Ps2::new(&counting_bios());
        other.load_state(&state).unwrap();
        assert_eq!(other.save_state(), state);
        other.run(300_000);
        assert_eq!(other.save_state(), after);
        assert!(Ps2::new(&[1]).load_state(&state).is_err());
    }

    /// A console whose EE runs EELOAD, with its boot target and the test
    /// mode's in memory.
    fn in_eeload() -> Ps2 {
        let mut ps2 = Ps2::new(&[]);
        let at = 0x0009_0700;
        ps2.bus.ram[at..at + 12].copy_from_slice(b"rom0:OSDSYS\0");
        ps2.bus.ram[at + 16..at + 30].copy_from_slice(b"rom0:TESTMODE\0");
        ps2.ee.pc = 0x0008_2000;
        ps2.boot = Some("cdrom0:\\SLUS_123.45;1".into());
        ps2
    }

    #[test]
    fn fast_boot_changes_eeloads_target() {
        let mut ps2 = in_eeload();
        ps2.fast_boot();
        assert_eq!(&ps2.bus.ram[0x90700..0x90717], b"cdrom0:\\SLUS_123.45;1\0\0");
        assert!(!ps2.boot_failed);
        // Loaded again (the IOP restarts): changed again.
        ps2.bus.ram[0x90700..0x9070C].copy_from_slice(b"rom0:OSDSYS\0");
        ps2.fast_boot();
        assert_eq!(&ps2.bus.ram[0x90700..0x90706], b"cdrom0");
        // The game runs: done.
        ps2.ee.pc = 0x0010_0008;
        ps2.fast_boot();
        assert!(ps2.boot.is_none());
    }

    #[test]
    fn fast_boot_needs_room() {
        let mut ps2 = in_eeload();
        // Something else right after the target: left alone.
        ps2.bus.ram[0x90710..0x90718].copy_from_slice(b"Another\0");
        ps2.fast_boot();
        assert!(ps2.boot_failed);
        assert_eq!(&ps2.bus.ram[0x90700..0x9070C], b"rom0:OSDSYS\0");
    }
}
