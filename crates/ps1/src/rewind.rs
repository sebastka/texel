//! Rollback's states: the machine kept every frame, to go back to when a
//! netplay input turns out not to be the one predicted, and run the frames
//! again. Saving and loading must take a fraction of a frame, so a state
//! is kept as it is (no compression), the large buffers (RAM, VRAM, sound
//! RAM, memory cards) copied as they are and the rest serialised as save
//! states are (which keeps both alike: what one has, the other has).
//!
//! Going back keeps what is not the machine's own: the compiled code (it
//! is checked again where RAM changed; RAM and the instruction cache stay
//! where they are, their contents copied, as compiled code holds their
//! addresses), the renderers' scaled VRAM (the
//! frames replayed draw again over those dropped), the samples not taken
//! yet.

use std::cell::Cell;

use crate::bus::{Bus, Clock, RAM_PAGE_BITS};
use crate::cpu::Cpu;
use crate::sio::MemoryCard;
use crate::{Error, System, Timing};

thread_local! {
    /// Serialising for a rewind state: the large buffers are left out.
    static LIGHT: Cell<bool> = const { Cell::new(false) };
}

/// Leaves the large buffers out while it lives.
struct Light;

impl Light {
    fn on() -> Self {
        LIGHT.with(|l| l.set(true));
        Self
    }
}

impl Drop for Light {
    fn drop(&mut self) {
        LIGHT.with(|l| l.set(false));
    }
}

/// Serde for the large buffers: as usual in save states, empty in rewind
/// states (copied apart).
pub(crate) mod big {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &[T], s: S) -> Result<S::Ok, S::Error> {
        if super::LIGHT.with(std::cell::Cell::get) {
            <[T]>::serialize(&[], s)
        } else {
            v.serialize(s)
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Box<[T]>, D::Error> {
        Box::<[T]>::deserialize(d)
    }
}

/// A state to go back to (see the module). Made once and reused: saving
/// into it keeps its buffers.
#[derive(Default)]
pub struct Rewind {
    small: Vec<u8>,
    ram: Vec<u8>,
    vram: Vec<u16>,
    spu_ram: Vec<u8>,
    cards: [Option<MemoryCard>; 2],
    clock: Clock,
    cheats: Vec<u32>,
}

#[derive(serde::Deserialize)]
struct LightState {
    #[allow(dead_code)]
    bios_hash: u64,
    cpu: Cpu,
    bus: Bus,
    cpu_debug: [u32; 5],
    timing: Timing,
}

impl Rewind {
    /// Holds a state.
    pub fn is_set(&self) -> bool {
        !self.small.is_empty()
    }

    /// A checksum of the machine it holds (not its memory cards), the same
    /// on every member of a room in step (netplay compares them).
    pub fn checksum(&self) -> u64 {
        let mut h = Hasher(0xcbf2_9ce4_8422_2325);
        h.bytes(&self.small);
        h.bytes(&self.ram);
        h.bytes(&self.spu_ram);
        for chunk in self.vram.chunks(4) {
            let mut w = 0u64;
            for (k, &v) in chunk.iter().enumerate() {
                w |= u64::from(v) << (16 * k);
            }
            h.word(w);
        }
        h.0
    }
}

/// A quick hash of words (FxHash's step): rollback states are a few MB,
/// hashed between two frames.
struct Hasher(u64);

impl Hasher {
    fn word(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn bytes(&mut self, b: &[u8]) {
        let (words, rest) = b.as_chunks::<8>();
        for &w in words {
            self.word(u64::from_le_bytes(w));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.word(u64::from_le_bytes(last) ^ (b.len() as u64) << 56);
    }
}

impl System {
    /// Keeps the machine as it is now in `r` (between two frames).
    pub fn save_rewind(&self, r: &mut Rewind) {
        {
            let _light = Light::on();
            let small = std::mem::take(&mut r.small);
            r.small = postcard::to_extend(&self.snapshot_ref(), {
                let mut v = small;
                v.clear();
                v
            })
            .expect("serialising to memory cannot fail");
        }
        r.ram.clear();
        r.ram.extend_from_slice(&self.bus.ram);
        r.vram.clear();
        r.vram.extend_from_slice(&self.bus.gpu.vram);
        r.spu_ram.clear();
        r.spu_ram.extend_from_slice(&self.bus.spu.ram);
        for (kept, card) in r.cards.iter_mut().zip(&self.bus.sio.cards) {
            match (kept.as_mut(), card) {
                (Some(k), Some(c)) => k.copy_from(c),
                (_, c) => *kept = c.as_ref().map(MemoryCard::duplicate),
            }
        }
        r.clock = self.bus.clock();
        self.cheats.save(&mut r.cheats);
    }

    /// Goes back to the machine kept in `r`, with the same disc, BIOS,
    /// memory size and codes.
    pub fn load_rewind(&mut self, r: &Rewind) -> Result<(), Error> {
        if !r.is_set() {
            return Err(Error::BadState("no state kept".into()));
        }
        if r.ram.len() != self.bus.ram.len() {
            return Err(Error::BadState("kept with another memory size".into()));
        }
        let state: LightState = {
            let _light = Light::on();
            postcard::from_bytes(&r.small).map_err(|e| Error::BadState(e.to_string()))?
        };
        let mut bus = state.bus;
        bus.adopt(&mut self.bus);
        // RAM, in place, counting writes where it changed: compiled code
        // there is checked again.
        let page = 1 << RAM_PAGE_BITS;
        for (k, (now, kept)) in bus.ram.chunks_mut(page).zip(r.ram.chunks(page)).enumerate() {
            if now != kept {
                now.copy_from_slice(kept);
                bus.ram_writes[k] = bus.ram_writes[k].wrapping_add(1);
            }
        }
        bus.gpu.vram.copy_from_slice(&r.vram);
        bus.spu.ram.copy_from_slice(&r.spu_ram);
        for (card, kept) in bus.sio.cards.iter_mut().zip(&r.cards) {
            if let (Some(c), Some(k)) = (card.as_mut(), kept) {
                c.copy_from(k);
            }
        }
        bus.set_clock(r.clock);
        bus.gpu.drawing = state.timing.gpu_drawing;
        bus.gpu_list = state.timing.gpu_list;
        bus.spu.capture = state.timing.spu_capture;
        // The instruction cache in place too: compiled code holds its
        // address.
        let mut cpu = state.cpu;
        let mut icache = std::mem::take(&mut self.cpu.icache);
        icache.copy_from_slice(&cpu.icache);
        cpu.icache = icache;
        cpu.gte.projected = std::mem::take(&mut self.cpu.gte.projected);
        self.bus = bus;
        self.cpu = cpu;
        self.cpu.set_debug_registers(state.cpu_debug);
        self.apply_enhancements();
        self.cheats.restore(&r.cheats);
        self.frame_stale = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::BIOS_SIZE;

    /// A BIOS that writes to RAM all along (a counter at 0x1000, a word
    /// further each time), so that frames differ.
    fn busy_bios() -> Vec<u8> {
        let mut rom = vec![0; BIOS_SIZE];
        rom[0x78..0x80].copy_from_slice(b"OpenBIOS");
        let program = [
            0x3c08_8000u32, // lui t0, 0x8000
            0x3509_1000,    // ori t1, t0, 0x1000
            0x8d2a_0000,    // loop: lw t2, 0(t1)
            0x254a_0001,    // addiu t2, t2, 1
            0xad2a_0000,    // sw t2, 0(t1)
            0x2529_0004,    // addiu t1, t1, 4
            0x3129_0fff,    // andi t1, t1, 0xfff
            0x0bf0_0002,    // j 0xbfc00008 (loop)
            0x3529_1000,    // ori t1, t1, 0x1000 (delay slot)
        ];
        for (k, w) in program.iter().enumerate() {
            rom[4 * k..4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    #[test]
    fn going_back_runs_the_same_frames_again() {
        let mut system = System::new(&busy_bios()).unwrap();
        for _ in 0..3 {
            system.run_frame();
        }
        let ram = system.bus.ram.as_ptr();
        let icache = system.cpu.icache.as_ptr();
        let mut kept = Rewind::default();
        system.save_rewind(&mut kept);
        let sum = kept.checksum();
        let run = |s: &mut System| {
            for k in 0..4u16 {
                s.set_buttons(0, 1 << k);
                s.run_frame();
            }
            s.save_state()
        };
        let first = run(&mut system);
        assert_ne!(system.save_state(), {
            let mut again = Rewind::default();
            system.save_rewind(&mut again);
            assert_ne!(again.checksum(), sum);
            system.load_rewind(&kept).unwrap();
            system.save_state()
        });
        let mut now = Rewind::default();
        system.save_rewind(&mut now);
        assert_eq!(now.checksum(), sum);
        assert_eq!(run(&mut system), first);
        // In place: compiled code holds their addresses.
        assert_eq!(system.bus.ram.as_ptr(), ram);
        assert_eq!(system.cpu.icache.as_ptr(), icache);
    }

    #[test]
    fn save_states_are_unchanged_by_the_light_ones() {
        let mut system = System::new(&busy_bios()).unwrap();
        system.run_frame();
        let before = system.save_state();
        let mut kept = Rewind::default();
        system.save_rewind(&mut kept);
        assert!(kept.small.len() < 64 << 10, "{} bytes", kept.small.len());
        assert_eq!(system.save_state(), before);
    }
}
