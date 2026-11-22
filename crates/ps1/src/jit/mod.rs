//! The CPU's code compiled to WebAssembly at run time, block by block (see
//! `compile`), in browsers: the module that compiles them gives them to the
//! browser through a `Backend`, which puts them in the emulator's own
//! function table, and the CPU calls them from there. The WebAssembly
//! encoder (`wasm`) is shared with the PS2's EE.
//!
//! The interpreter stays the reference, and compiled blocks do exactly what
//! it would, cycle for cycle: they count each instruction's cycles as it
//! does (the instruction cache included), and stop where it would look at
//! anything outside the CPU: when the bus has 64 cycles pending (the
//! peripherals then catch up, and may raise an interrupt), and after any
//! instruction they leave to it that touches memory outside RAM and the
//! scratchpad, the coprocessor 0, or issues a load (see `helpers::exec`).
//! A block only starts where `step` would run its first instruction
//! plainly: not in a delay slot, with no load in flight, with no interrupt
//! to take. So a console runs the same with blocks compiled or not.

mod compile;
#[cfg(test)]
mod tests;
pub mod wasm;

use std::collections::HashMap;

use crate::bus::{Bus, RAM_SIZE};
use crate::cpu::{Cpu, CpuBus};

pub use compile::{Layout, MAX_LEN, compile};

/// Runs a block this many times before compiling it.
const HOT: u32 = 2;

/// What compiles modules and runs their function: the browser, through the
/// module running the emulator (where the little unsafe code calling into
/// compiled blocks, and back, lives).
pub trait Backend {
    /// Compiles `module` (which exports its function as "block") and
    /// returns a handle to the function, or `None` if it failed.
    fn install(&mut self, module: &[u8]) -> Option<u32>;

    /// Runs function `func` (from `install`) on `cpu` and `bus`: returns
    /// the number of instructions it ran.
    fn call(&mut self, func: u32, cpu: &mut Cpu, bus: &mut Bus) -> u32;

    /// Function `func` will not be called again: its handle may be reused.
    fn release(&mut self, func: u32);

    /// The function-table index compiled blocks call for an instruction
    /// they leave to the interpreter: of type `(cpu: i32, bus: i32, instr:
    /// i32, pc: i32, flags: i32) -> i32`, doing what `helpers::exec` does.
    fn exec_index(&self) -> u32;
}

enum Entry {
    /// Run this many times by the interpreter so far.
    Cold(u32),
    /// Compiled: its function, its code (to check it is still there: RAM
    /// may have been rewritten, by the CPU or DMA), and whether the
    /// instruction cache was on (as its timing assumes).
    Compiled {
        func: u32,
        pa: u32,
        code: Vec<u8>,
        icache: bool,
    },
    /// Left to the interpreter (nothing to compile there).
    Interpreted,
}

/// Counters, for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub compiled: u64,
    pub failed: u64,
    /// Blocks dropped: their code changed.
    pub invalidated: u64,
    /// Blocks run, and the instructions they ran.
    pub runs: u64,
    pub in_blocks: u64,
    /// Of the instructions compiled, those compiled to WebAssembly (the
    /// others call the interpreter).
    pub native: u64,
    pub total: u64,
}

pub struct Jit {
    backend: Box<dyn Backend>,
    /// By the address of their first instruction.
    blocks: HashMap<u32, Entry>,
    pub stats: Stats,
}

impl Jit {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend,
            blocks: HashMap::new(),
            stats: Stats::default(),
        }
    }

    /// Forgets every block (the machine was reset or restored: its memory
    /// is elsewhere now).
    pub fn clear(&mut self) {
        for (_, entry) in self.blocks.drain() {
            if let Entry::Compiled { func, .. } = entry {
                self.backend.release(func);
            }
        }
    }

    /// Runs the block at the CPU's pc, if there is one it may start
    /// (compiling it when it is hot): whether it ran. Its cycles are in
    /// `bus.pending` then, as `step`'s would be.
    pub fn run(&mut self, cpu: &mut Cpu, bus: &mut Bus) -> bool {
        let Some(cause) = cpu.plain_start(bus.irq_active()) else {
            return false;
        };
        let pc = cpu.pc;
        let entry = self.blocks.entry(pc).or_insert(Entry::Cold(0));
        match entry {
            Entry::Interpreted => return false,
            Entry::Compiled { func, pa, code, icache } => {
                let current = code_at(bus, *pa, code.len());
                if current != Some(&code[..]) {
                    self.backend.release(*func);
                    *entry = Entry::Cold(0);
                    self.stats.invalidated += 1;
                    return false;
                }
                if *icache != bus.icache_enabled() {
                    return false;
                }
                let func = *func;
                cpu.set_cause(cause);
                let n = self.backend.call(func, cpu, bus);
                self.stats.runs += 1;
                self.stats.in_blocks += u64::from(n);
                return true;
            }
            Entry::Cold(n) if *n + 1 < HOT => {
                *n += 1;
                return false;
            }
            Entry::Cold(_) => {}
        }
        // Hot: compile it (it runs the next time round).
        *entry = Entry::Interpreted;
        let pa = pc & 0x1fff_ffff;
        let ram = (pa as usize) < 0x0080_0000;
        let bios = (0x1fc0_0000..0x1fc8_0000).contains(&pa);
        if !ram && !bios {
            return false;
        }
        // Up to the end of RAM or the BIOS.
        let end = if ram { 0x0080_0000 } else { 0x1fc8_0000 };
        let count = ((end - pa) / 4).min(MAX_LEN as u32 + 1);
        let words: Vec<u32> = (0..count).map(|k| bus.fetch(pc.wrapping_add(4 * k))).collect();
        let layout = Layout::new(cpu, bus);
        let icache = bus.icache_enabled();
        let ram_range = ram.then(|| {
            let start = pa & (RAM_SIZE as u32 - 1);
            (start, start + 4 * words.len() as u32)
        });
        let Some(compiled) = compile(pc, &words, icache, ram_range, self.backend.exec_index(), &layout) else {
            return false;
        };
        self.stats.native += u64::from(compiled.native);
        self.stats.total += u64::from(compiled.len);
        let Some(func) = self.backend.install(&compiled.module) else {
            self.stats.failed += 1;
            return false;
        };
        self.stats.compiled += 1;
        // The code it was compiled from, as it must still be (RAM only:
        // the BIOS does not change).
        let pa = if ram { pa & (RAM_SIZE as u32 - 1) } else { u32::MAX };
        let code = if ram {
            code_at(bus, pa, 4 * compiled.len as usize)
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.blocks.insert(pc, Entry::Compiled { func, pa, code, icache });
        false
    }
}

/// RAM's bytes at `pa` (`u32::MAX`: not RAM: nothing to check).
fn code_at(bus: &Bus, pa: u32, len: usize) -> Option<&[u8]> {
    if pa == u32::MAX {
        return Some(&[]);
    }
    bus.ram.get(pa as usize..pa as usize + len)
}

pub mod helpers {
    use crate::bus::Bus;
    use crate::cpu::Cpu;

    /// Runs one instruction of a compiled block through the interpreter, as
    /// `step` would (with the fetch's extra cycles the block worked out):
    /// `instr`, at `pc`; `flags`: bit 0, a taken branch's delay slot; above,
    /// the fetch's extra cycles. The block has set the CPU's pc, next pc,
    /// branch flag, pending load and cycles as `step` would find them.
    /// Returns the instruction's cycles, with bit 31 set when the block
    /// must stop after it: an exception, a load issued, or anything beyond
    /// the CPU (memory other than through the fast paths, coprocessor 0).
    pub fn exec(cpu: &mut Cpu, bus: &mut Bus, instr: u32, pc: u32, flags: u32) -> u32 {
        let (cycles, plain) = cpu.step_compiled(bus, instr, pc, flags & 1 != 0, flags >> 1);
        let stop = !plain || !pure(instr);
        cycles | (u32::from(stop) << 31)
    }

    /// Instructions that only change the CPU's (and the GTE's) registers:
    /// a block may go on after them.
    fn pure(i: u32) -> bool {
        let op = i >> 26;
        let rs = (i >> 21) & 0x1f;
        match op {
            0x00 => matches!(i & 0x3f, 0x10..=0x1b | 0x20 | 0x22),
            0x08 => true,
            // GTE commands, MTC2, CTC2.
            0x12 => rs & 0x10 != 0 || rs == 0x04 || rs == 0x06,
            _ => false,
        }
    }
}
