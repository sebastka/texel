//! The EE's code compiled to WebAssembly at run time, block by block (see
//! `compile`), in browsers: the module that compiles them gives them to the
//! browser through a `Backend`, which puts them in the emulator's own
//! function table, and the EE calls them from there.
//!
//! The interpreter stays the reference, and runs what is not compiled:
//! code run once (a block is compiled when it runs the second time), delay
//! slots the EE stops in, blocks that would run past the end of a slice. So
//! timing, exceptions and interrupts are the interpreter's exactly, and a
//! console runs the same with blocks compiled or not.
//!
//! Blocks go to the browser in batches (`BATCH` to a module, see `flush`),
//! compiled in the background: until their module is ready, they stay
//! interpreted.

mod compile;
#[cfg(test)]
mod tests;
pub mod wasm;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use super::Ee;
use crate::bus::{Bus, CODE_LINE};

pub use compile::{Compiled, Layout, MAX_LEN, Source, batch, compile, idle_candidate};

/// The type index of `exec` in compiled modules.
const EXEC_TYPE: u32 = 1;

/// Runs a block before compiling it this many times.
const HOT: u32 = 2;

/// Blocks compiled together, in one module (see `Jit::flush`).
const BATCH: usize = 64;

/// Blocks compiled in a frame, at most: when a game loads much code at
/// once, the rest stay interpreted a few frames longer rather than one
/// frame taking long.
const PER_FRAME: u32 = 1024;

/// What compiles modules and runs their function: the browser, through
/// the module running the emulator (where the little unsafe code calling
/// into compiled blocks, and back, lives).
pub trait Backend {
    /// Starts compiling `module`, which exports `count` functions (`b0`,
    /// `b1`..., see `batch`), in the background: its number, which `ready`
    /// gives back with the functions.
    fn submit(&mut self, module: &[u8], count: usize) -> u32;

    /// The modules compiled since the last call: their numbers and their
    /// functions' handles, in order (`None`: it failed).
    fn ready(&mut self) -> Vec<(u32, Option<Vec<u32>>)>;

    /// Runs function `func` (from `ready`) on `ee` and `bus`, for at
    /// most `left` instructions: returns the number it ran.
    fn call(&mut self, func: u32, ee: &mut Ee, bus: &mut Bus, left: u32) -> u32;

    /// Function `func` (from `ready`) will not be called again: its
    /// handle may be given to another.
    fn release(&mut self, func: u32);

    /// The function-table index compiled blocks call for an instruction
    /// they leave to the interpreter: a function of type
    /// `(ee: i32, bus: i32, instr: i32, pc: i32, flags: i32) -> i32` doing
    /// what `helpers::exec` does.
    fn exec_index(&self) -> u32;
}

enum Entry {
    /// Run this many times by the interpreter so far.
    Cold(u32),
    /// Compiled: at physical address `pa`, needing `len` instructions left
    /// to start.
    Compiled { pa: u32, len: u32, func: u32 },
    /// Compiled, waiting for its batch's module (interpreted meanwhile);
    /// the ticket tells it from a later compilation of the same address.
    Queued(u64),
    /// Left to the interpreter (nothing to compile there).
    Interpreted,
}

/// Counters, for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub compiled: u64,
    pub failed: u64,
    pub invalidated: u64,
    /// Instructions run in compiled blocks, and by the interpreter.
    pub in_blocks: u64,
    pub interpreted: u64,
    /// Blocks run.
    pub runs: u64,
    /// Instructions interpreted while their block's module compiled.
    pub queued: u64,
    /// Why instructions were interpreted: in a delay slot, the block did
    /// not fit in the slice, not compiled yet, left to the interpreter.
    pub delay: u64,
    pub tail: u64,
    pub cold: u64,
    pub kept: u64,
    /// Of the instructions compiled, those compiled to WebAssembly (the
    /// others call the interpreter).
    pub native: u64,
    pub total: u64,
}

/// Hashes block addresses: one multiplication (the default hasher is made
/// to resist attacks, and slow).
#[derive(Default)]
struct AddressHasher(u64);

impl Hasher for AddressHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u32(&mut self, v: u32) {
        self.0 = u64::from(v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type Map<V> = HashMap<u32, V, BuildHasherDefault<AddressHasher>>;

/// A compiled block in the dispatch cache (`len`: the instructions it
/// needs left to start), and the mapping it was looked up with.
#[derive(Clone, Copy, Default)]
struct Slot {
    va: u32,
    mapping: u32,
    func: u32,
    len: u32,
}

/// Dispatch cache slots (by address bits 2 and up).
const SLOTS: usize = 1 << 14;

pub struct Jit {
    backend: Box<dyn Backend>,
    /// The blocks run lately, found without hashing or translating: valid
    /// while the mapping has not changed (and the block is not dropped).
    slots: Box<[Slot]>,
    /// By virtual address of their first instruction.
    blocks: Map<Entry>,
    /// The blocks compiled from each line of RAM (`CODE_LINE` bytes).
    lines: Map<Vec<u32>>,
    /// Blocks compiled, not sent to the browser yet; and those sent, by
    /// their module's number.
    queue: Vec<Pending>,
    compiling: HashMap<u32, Vec<Pending>>,
    next_ticket: u64,
    /// Blocks compiled in this frame (see `PER_FRAME`).
    this_frame: u32,
    pub stats: Stats,
}

/// A block compiled, waiting for its batch's module.
struct Pending {
    va: u32,
    pa: u32,
    len: u32,
    ticket: u64,
    compiled: Compiled,
}

impl Jit {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self {
            backend,
            slots: vec![Slot::default(); SLOTS].into_boxed_slice(),
            blocks: Map::default(),
            lines: Map::default(),
            queue: Vec::new(),
            compiling: HashMap::new(),
            next_ticket: 0,
            this_frame: 0,
            stats: Stats::default(),
        }
    }

    /// Puts the modules compiled since in use, then sends the blocks
    /// compiled since to the browser, in one module, compiled in the
    /// background. Called when a batch is full, and at the end of each
    /// frame (`end_frame`).
    pub fn flush(&mut self) {
        self.take_ready();
        if self.queue.is_empty() {
            return;
        }
        let queue = std::mem::take(&mut self.queue);
        let compiled: Vec<&Compiled> = queue.iter().map(|p| &p.compiled).collect();
        let module = batch(&compiled);
        let number = self.backend.submit(&module, queue.len());
        self.compiling.insert(number, queue);
    }

    /// At the end of a frame: the blocks compiled in it go to the browser,
    /// and the next frame may compile `PER_FRAME` more.
    pub fn end_frame(&mut self) {
        self.flush();
        self.this_frame = 0;
    }

    /// The modules compiled since: their blocks run compiled from now on,
    /// unless their code changed meanwhile (`invalidate` dropped them).
    fn take_ready(&mut self) {
        for (number, funcs) in self.backend.ready() {
            let Some(queue) = self.compiling.remove(&number) else {
                for func in funcs.into_iter().flatten() {
                    self.backend.release(func);
                }
                continue;
            };
            let funcs = funcs.filter(|f| f.len() == queue.len());
            if funcs.is_none() {
                self.stats.failed += queue.len() as u64;
            }
            for (k, p) in queue.into_iter().enumerate() {
                let current = matches!(self.blocks.get(&p.va), Some(Entry::Queued(t)) if *t == p.ticket);
                match funcs.as_ref().map(|f| f[k]) {
                    Some(func) if current => {
                        self.stats.compiled += 1;
                        self.blocks.insert(
                            p.va,
                            Entry::Compiled {
                                pa: p.pa,
                                len: p.len,
                                func,
                            },
                        );
                    }
                    Some(func) => self.backend.release(func),
                    None if current => {
                        self.blocks.insert(p.va, Entry::Interpreted);
                    }
                    None => {}
                }
            }
        }
    }

    /// Forgets the blocks of the RAM lines written since they were compiled.
    fn invalidate(&mut self, bus: &mut Bus) {
        for line in bus.take_written_code() {
            for va in self.lines.remove(&line).unwrap_or_default() {
                if let Some(Entry::Compiled { func, .. }) = self.blocks.remove(&va) {
                    self.backend.release(func);
                    self.stats.invalidated += 1;
                }
                let slot = &mut self.slots[slot_of(va)];
                if slot.va == va {
                    *slot = Slot::default();
                }
            }
        }
    }
}

fn slot_of(va: u32) -> usize {
    (va >> 2) as usize & (SLOTS - 1)
}

/// Executable memory: RAM (whose writes are watched) and the BIOS.
fn compilable(pa: u32) -> bool {
    (pa as usize) < crate::bus::RAM_SIZE || (0x1FC0_0000..0x2000_0000).contains(&pa)
}

impl Ee {
    /// Runs `cycles` instructions, compiled where they can be. In the idle
    /// loop, or polling INTC_STAT, the rest of the time passes without
    /// running it.
    pub fn run(&mut self, bus: &mut Bus, cycles: u64) {
        let Some(mut jit) = self.jit.take() else {
            for k in 0..cycles {
                if self.idling() || std::mem::take(&mut self.polling) {
                    self.skip(cycles - k);
                    return;
                }
                self.step(bus);
            }
            return;
        };
        let mut left = cycles;
        while left > 0 {
            if self.idling() || std::mem::take(&mut self.polling) {
                self.skip(left);
                break;
            }
            if bus.code_written() {
                jit.invalidate(bus);
            }
            // The dispatch cache first (func 0 is no function).
            let slot = jit.slots[slot_of(self.pc)];
            if slot.va == self.pc
                && slot.func != 0
                && !self.delay_pending
                && !self.pending_di
                && slot.mapping == self.cop0.mapping()
                && u64::from(slot.len) <= left
            {
                let n = jit
                    .backend
                    .call(slot.func, self, bus, left.min(u64::from(u32::MAX)) as u32);
                self.catch_up(n);
                self.counted = 0;
                jit.stats.runs += 1;
                jit.stats.in_blocks += u64::from(n);
                left -= u64::from(n);
                continue;
            }
            if let Some(n) = self.run_block(&mut jit, bus, left) {
                jit.stats.in_blocks += u64::from(n);
                left -= u64::from(n);
            } else {
                self.step(bus);
                jit.stats.interpreted += 1;
                left -= 1;
            }
        }
        self.jit = Some(jit);
    }

    /// Runs the block at pc if it is compiled and fits in `left` cycles
    /// (compiling it if it is hot): the instructions it ran.
    fn run_block(&mut self, jit: &mut Jit, bus: &mut Bus, left: u64) -> Option<u32> {
        // In a delay slot, or after a DI (whose write lands after the
        // next instruction): the interpreter's.
        if self.delay_pending || self.pending_di {
            jit.stats.delay += 1;
            return None;
        }
        let va = self.pc;
        let pa = self.cop0.translate(va, false).ok()?;
        let entry = jit.blocks.entry(va).or_insert(Entry::Cold(0));
        match *entry {
            Entry::Interpreted => {
                jit.stats.kept += 1;
                return None;
            }
            Entry::Queued(_) => {
                jit.stats.queued += 1;
                return None;
            }
            Entry::Compiled { pa: at, len, func } if at == pa => {
                if u64::from(len) > left {
                    jit.stats.tail += 1;
                    return None;
                }
                jit.slots[slot_of(va)] = Slot {
                    va,
                    mapping: self.cop0.mapping(),
                    func,
                    len,
                };
                let n = jit.backend.call(func, self, bus, left.min(u64::from(u32::MAX)) as u32);
                jit.stats.runs += 1;
                self.catch_up(n);
                self.counted = 0;
                return Some(n);
            }
            // Compiled for another mapping: compile it again.
            Entry::Compiled { func, .. } => {
                *entry = Entry::Cold(HOT);
                jit.backend.release(func);
                let slot = &mut jit.slots[slot_of(va)];
                if slot.va == va {
                    *slot = Slot::default();
                }
            }
            Entry::Cold(n) if n + 1 < HOT => {
                *entry = Entry::Cold(n + 1);
                jit.stats.cold += 1;
                return None;
            }
            // Hot, but enough compiled this frame: the next one.
            Entry::Cold(_) if jit.this_frame >= PER_FRAME => {
                jit.stats.cold += 1;
                return None;
            }
            Entry::Cold(_) => {}
        }
        // Hot: compile it.
        *entry = Entry::Interpreted;
        if !compilable(pa) {
            return None;
        }
        let page_end = (va | 0xFFF).wrapping_add(1);
        let count = (page_end.wrapping_sub(va) / 4).min(MAX_LEN as u32 + 1) as usize;
        let words: Vec<u32> = (0..count as u32).map(|k| bus.read32(pa + 4 * k)).collect();
        // Idle loops: NOPs from the branch's target to its delay slot.
        let mut idle = Vec::new();
        for (k, &w) in words.iter().enumerate() {
            let at = va.wrapping_add(4 * k as u32);
            if let Some(target) = idle_candidate(w, at) {
                let nops = (target..=at + 4)
                    .step_by(4)
                    .all(|a| a == at || matches!(self.cop0.translate(a, false), Ok(p) if bus.read32(p) == 0));
                if nops {
                    idle.push(at);
                }
            }
        }
        let (pages, stale) = self.cop0.pages.addresses();
        let (ram, code_lines, scratchpad) = bus.addresses();
        let base = self as *const Ee as usize;
        let status = &self.cop0.regs[super::cop0::STATUS] as *const u32 as usize;
        let layout = Layout {
            pages: pages as u32,
            ram: ram as u32,
            scratchpad: scratchpad as u32,
            code_lines: code_lines as u32,
            stale_offset: (stale - base) as u32,
            status_offset: (status - base) as u32,
        };
        jit.this_frame += 1;
        let compiled = compile(&Source { va, words, idle }, jit.backend.exec_index(), layout)?;
        let len = compiled.first;
        jit.stats.native += u64::from(compiled.native);
        jit.stats.total += u64::from(compiled.words);
        // Interpreted until its module is ready; dropped if its code
        // changes meanwhile (its lines are watched from now on).
        let ticket = jit.next_ticket;
        jit.next_ticket += 1;
        jit.blocks.insert(va, Entry::Queued(ticket));
        jit.queue.push(Pending {
            va,
            pa,
            len,
            ticket,
            compiled,
        });
        if (pa as usize) < crate::bus::RAM_SIZE {
            let line = |a: u32| a / CODE_LINE as u32;
            for l in line(pa)..=line(pa + 4 * len - 1) {
                jit.lines.entry(l).or_default().push(va);
                bus.watch_code(l);
            }
        }
        if jit.queue.len() >= BATCH {
            jit.flush();
        }
        None
    }

    /// Counts the instructions a block ran up to `index` (from its start)
    /// that are not counted yet: as `step` would have, one cycle each.
    fn catch_up(&mut self, index: u32) {
        let n = index - self.counted;
        self.cycles += u64::from(n);
        self.unticked += n;
        self.counted = index;
    }
}

pub mod helpers {
    use super::super::Ee;
    use crate::bus::Bus;

    /// Runs one instruction of a compiled block through the interpreter:
    /// `instr`, at `pc`; `flags`: bit 0, it is a delay slot; above, how many
    /// of the block's instructions ran before it. The block has set pc and
    /// next_pc as `step` would have. Returns 1 when the block must stop
    /// there: an exception, a change of where the EE goes next, or a poll
    /// of INTC_STAT found (the rest of the slice then passes at once, from
    /// right after it, as with the interpreter).
    pub fn exec(ee: &mut Ee, bus: &mut Bus, instr: u32, pc: u32, flags: u32) -> u32 {
        ee.catch_up(flags >> 1);
        let (p, n) = (ee.pc, ee.next_pc);
        ee.current_pc = pc;
        ee.in_delay = flags & 1 != 0;
        ee.delay_pending = false;
        ee.execute(instr, bus);
        u32::from(ee.pc != p || ee.next_pc != n || ee.delay_pending || ee.polling || ee.pending_di)
    }
}
