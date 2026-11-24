//! The VUs' microprograms compiled to WebAssembly, in browsers (see `compile`):
//! the module running the emulator installs them through a `VuBackend`, as
//! it does the EE's blocks, and `Vu::run` calls them.
//!
//! A program is compiled from its start address when it runs the second
//! time, and kept with the instructions it was compiled from: games upload
//! their microcode again and again (VIF's MPG), the same each time, and a
//! program is checked against micro memory only after it was written to.
//! The interpreter stays the reference: compiled programs do what it does,
//! instruction by instruction, and leave it the rare cases.

mod compile;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

pub use compile::{Compiled, compile};
pub(crate) use compile::{MACRO_I32_LOCALS, MACRO_V128_LOCALS, V128_TYPE, macro_upper};

use crate::gif::Gif;
use crate::gs::Gs;
use crate::vu::Vu;

/// Runs a program before compiling it this many times.
const HOT: u32 = 2;
/// Versions of a program kept (games upload one renderer's microcode after
/// another to the same addresses, every frame: Jak and Daxter more than
/// a dozen).
const VERSIONS: usize = 64;

/// What compiles modules and runs their function: the browser, through the
/// module running the emulator.
pub trait VuBackend {
    /// Compiles `module` (which exports its function as "block") and
    /// returns a handle to it, or `None` if that failed.
    fn install(&mut self, module: &[u8]) -> Option<u32>;

    /// Function `func` will not be called again.
    fn release(&mut self, func: u32);

    /// Runs program `func` on `vu` (its XGKICKs feeding `gif` and `gs`):
    /// 0 when it ended, 1 when it stopped for the interpreter.
    fn call(&mut self, func: u32, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) -> u32;

    /// The function-table index of the function compiled programs call for
    /// the lower instructions they leave to the interpreter: of type
    /// `(vu: i32, gif: i32, gs: i32, instr: i32, pc: i32) -> ()`, doing what
    /// `helpers::lower` does.
    fn helper_index(&self) -> u32;
}

/// How a compiled program's run went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    /// It ended (`vu.run_n` instructions).
    Ended,
    /// It stopped at `vu.pc`, after `vu.run_n` instructions: the
    /// interpreter goes on.
    Stopped,
}

struct Program {
    func: u32,
    /// The instructions compiled: (PC, word).
    words: Vec<(u32, u64)>,
    /// The PCs its blocks start at; and the ones it was compiled with,
    /// besides its start (where jumps through registers went).
    starts: Vec<u32>,
    entries: Vec<u32>,
    /// The micro memory generation it was last found the same at.
    generation: u64,
}

/// Entries a program is recompiled with, at most.
const ENTRIES: usize = 16;

#[derive(Default)]
struct Start {
    runs: u32,
    programs: Vec<Program>,
}

/// Counters, for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub compiled: u64,
    pub failed: u64,
    /// Programs run compiled, and those that stopped for the interpreter.
    pub runs: u64,
    pub stopped: u64,
    /// Instructions run compiled.
    pub instructions: u64,
    /// Programs run by the interpreter (not compiled yet, or changed).
    pub interpreted: u64,
}

pub struct VuJit {
    backend: Box<dyn VuBackend>,
    starts: HashMap<u32, Start>,
    pub stats: Stats,
}

impl VuJit {
    pub fn new(backend: Box<dyn VuBackend>) -> Self {
        Self {
            backend,
            starts: HashMap::new(),
            stats: Stats::default(),
        }
    }

    /// Runs the program at `vu.pc` compiled, if it is (compiling it when
    /// it is hot): None when the interpreter runs it.
    pub fn run(&mut self, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) -> Option<Ran> {
        let pc = vu.pc;
        let generation = vu.micro_gen;
        let start = self.starts.entry(pc).or_default();
        let found = start.programs.iter_mut().position(|p| {
            if p.generation == generation {
                return true;
            }
            let same = p.words.iter().all(|&(at, w)| word(&vu.micro, at) == w);
            if same {
                p.generation = generation;
            }
            same
        });
        if let Some(k) = found {
            // The most recent first: it is checked first next time.
            if k != 0 {
                let p = start.programs.remove(k);
                start.programs.insert(0, p);
            }
            let func = start.programs[0].func;
            vu.run_n = 0;
            let ended = self.backend.call(func, vu, gif, gs) == 0;
            vu.steps += u64::from(vu.run_n);
            self.stats.runs += 1;
            self.stats.instructions += u64::from(vu.run_n);
            if ended {
                return Some(Ran::Ended);
            }
            self.stats.stopped += 1;
            // Stopped where no block starts (a jump through a register,
            // to a PC found at run time): compiled again with a block
            // there, for the next run.
            let p = &start.programs[0];
            let at = vu.pc;
            if !p.starts.contains(&at) && p.entries.len() < ENTRIES {
                let mut entries = p.entries.clone();
                entries.push(at);
                let qwords = (vu.data.len() / 16) as u32;
                if let Some(c) = compile(&vu.micro, pc, &entries, qwords, self.backend.helper_index())
                    && let Some(func) = self.backend.install(&c.module)
                {
                    self.stats.compiled += 1;
                    let old = std::mem::replace(
                        &mut start.programs[0],
                        Program {
                            func,
                            words: c.words,
                            starts: c.starts,
                            entries,
                            generation,
                        },
                    );
                    self.backend.release(old.func);
                }
            }
            return Some(Ran::Stopped);
        }
        self.stats.interpreted += 1;
        start.runs += 1;
        if start.runs < HOT {
            return None;
        }
        start.runs = 0;
        let qwords = (vu.data.len() / 16) as u32;
        let Some(c) = compile(&vu.micro, pc, &[], qwords, self.backend.helper_index()) else {
            self.stats.failed += 1;
            return None;
        };
        let Some(func) = self.backend.install(&c.module) else {
            self.stats.failed += 1;
            return None;
        };
        self.stats.compiled += 1;
        if start.programs.len() >= VERSIONS
            && let Some(old) = start.programs.pop()
        {
            self.backend.release(old.func);
        }
        start.programs.insert(
            0,
            Program {
                func,
                words: c.words,
                starts: c.starts,
                entries: Vec::new(),
                generation,
            },
        );
        // It runs compiled the next time.
        None
    }
}

fn word(micro: &[u8], pc: u32) -> u64 {
    let at = pc as usize * 8;
    micro
        .get(at..at + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

pub mod helpers {
    use crate::gif::Gif;
    use crate::gs::Gs;
    use crate::vu::Vu;

    /// Runs a lower instruction of a compiled program through the
    /// interpreter: `instr`, with the PC at `pc` (as the interpreter has it
    /// then, past the instruction).
    pub fn lower(vu: &mut Vu, gif: &mut Gif, gs: &mut Gs, instr: u32, pc: u32) {
        vu.pc = pc;
        vu.exec_lower(instr, gif, gs);
    }
}
