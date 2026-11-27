//! A block of CPU code compiled to one WebAssembly function: from its first
//! instruction to the delay slot of its first jump or branch back to its
//! start (or `MAX_LEN` instructions), leaving at the conditional branches
//! before taken. The function takes the CPU and the bus (their addresses
//! in this module's memory) and returns the instructions it ran.
//!
//! Each instruction: its fetch's cycles (through the instruction cache, as
//! `Cpu::fetch_cycles` counts them); the instruction, in WebAssembly, or
//! through the interpreter (`helpers::exec`); the load the previous one
//! issued lands (the load delay slot, resolved here: the next instruction
//! is known); its cycles counted, into the CPU's and the bus's pending
//! ones. The block stops where `step`'s caller would do anything but run
//! the next instruction: at the bus's deadline (the peripherals catch up),
//! after the interpreter ran something beyond the CPU; and leaves the CPU
//! as `step` would have.
//!
//! In WebAssembly: ALU instructions, branches and jumps, loads and stores
//! of RAM and the scratchpad (aligned; stores with the cache not isolated).
//! The rest goes through the interpreter: multiplications and divisions,
//! ADD, ADDI and SUB (they trap on overflow), coprocessors, LWL and
//! friends, and accesses elsewhere.

use super::JitBus;
use super::wasm::{self, Code, FuncType, I32, I64, op};
use crate::bus::RAM_PAGE_BITS;
use crate::cpu::{BIOS_ACCESS, Cpu, ICACHE_FILL, IO_ACCESS, RAM_ACCESS};

/// Instructions per block, at most.
pub const MAX_LEN: usize = 64;

/// The type index of the interpreter's function (see `Backend`).
const EXEC_TYPE: u32 = 1;

/// Where compiled code finds things: offsets in the CPU and the bus (from
/// the addresses the function is given), and the addresses of RAM and the
/// instruction cache (valid until the machine is reset or restored, when
/// blocks are dropped).
pub struct Layout {
    regs: u32,
    out_regs: u32,
    pc: u32,
    next_pc: u32,
    branch: u32,
    load_reg: u32,
    load_val: u32,
    cycles: u32,
    sr: u32,
    pending: u32,
    deadline: u32,
    scratchpad: u32,
    ram: u32,
    /// RAM's addresses' mask (its size less one: see `Bus::ram_mask`).
    ram_mask: u32,
    ram_writes: u32,
    icache: u32,
    /// The pending cycles count instructions (one each), not their cycles
    /// (the PS2's IOP, see `JitBus::FLAT`); its loops never skip ahead.
    flat: bool,
}

/// Where a bus keeps what compiled code reads and writes directly (see
/// `JitBus::places`): addresses in the module's memory.
pub struct BusPlaces {
    pub pending: usize,
    pub deadline: usize,
    pub scratchpad: usize,
    pub ram: usize,
    pub ram_mask: u32,
    pub ram_writes: usize,
}

impl Layout {
    pub fn new<B: JitBus>(cpu: &Cpu, bus: &B) -> Self {
        let c = cpu as *const Cpu as usize;
        let b = bus as *const B as usize;
        let at = |p: usize, base: usize| (p - base) as u32;
        let places = bus.places();
        Self {
            regs: at(cpu.regs.as_ptr() as usize, c),
            out_regs: at(cpu.out_regs.as_ptr() as usize, c),
            pc: at(&cpu.pc as *const u32 as usize, c),
            next_pc: at(&cpu.next_pc as *const u32 as usize, c),
            branch: at(&cpu.branch as *const bool as usize, c),
            load_reg: at(&cpu.load.0 as *const usize as usize, c),
            load_val: at(&cpu.load.1 as *const u32 as usize, c),
            cycles: at(&cpu.cycles as *const u64 as usize, c),
            sr: at(&cpu.sr as *const u32 as usize, c),
            pending: at(places.pending, b),
            deadline: at(places.deadline, b),
            scratchpad: at(places.scratchpad, b),
            ram: places.ram as u32,
            ram_mask: places.ram_mask,
            ram_writes: places.ram_writes as u32,
            icache: cpu.icache.as_ptr() as usize as u32,
            flat: B::FLAT,
        }
    }
}

pub struct Compiled {
    /// The function's body (see `batch`).
    pub code: Code,
    /// Instructions in the block, and those compiled to WebAssembly.
    pub len: u32,
    pub native: u32,
}

// Locals: the parameters, then ours.
const CPU: u32 = 0;
const BUS: u32 = 1;
const N: u32 = 2;
const PEND: u32 = 3;
const CYC: u32 = 4;
const EXTRA: u32 = 5;
const ADDR: u32 = 6;
const KIND: u32 = 7;
const NEXT: u32 = 8;
const TAKEN: u32 = 9;
const RET: u32 = 10;
const SMC: u32 = 11;
/// Two locals for loaded values, alternating: one landing, one in flight.
const LOADED: [u32; 2] = [12, 13];
/// In a loop: the pending cycles when the round began, and whether one
/// round was done (see `idle_loop`).
const START: u32 = 14;
const ROUNDS: u32 = 15;

/// Where the CPU stands after an instruction, for leaving the block there.
#[derive(Clone, Copy)]
enum After {
    /// A plain instruction: the next one at `pc`.
    Plain { pc: u32 },
    /// A branch: its delay slot at `pc`, then NEXT (taken: TAKEN).
    Branch { pc: u32 },
    /// A delay slot: NEXT.
    Delay,
}

struct Emitter<'a> {
    c: Code,
    l: &'a Layout,
    /// Blocks and ifs open inside the block the function's exit is.
    depth: u32,
}

impl Emitter<'_> {
    fn reg(&mut self, r: usize) {
        if r == 0 {
            self.c.i32_const(0);
        } else {
            self.c.local_get(CPU).mem(op::I32_LOAD, 2, self.l.regs + 4 * r as u32);
        }
    }

    /// Stores the value in local `from` into register `r` (both copies).
    fn set_reg_from(&mut self, r: usize, from: u32) {
        if r == 0 {
            return;
        }
        for base in [self.l.regs, self.l.out_regs] {
            self.c
                .local_get(CPU)
                .local_get(from)
                .mem(op::I32_STORE, 2, base + 4 * r as u32);
        }
    }

    /// Computes a value (pushed by `value`) into register `r`, through RET.
    fn set_reg(&mut self, r: usize, value: impl FnOnce(&mut Self)) {
        if r == 0 {
            return;
        }
        value(self);
        self.c.local_set(RET);
        self.set_reg_from(r, RET);
    }

    fn store_cpu(&mut self, offset: u32, value: impl FnOnce(&mut Code)) {
        self.c.local_get(CPU);
        value(&mut self.c);
        self.c.mem(op::I32_STORE, 2, offset);
    }

    /// Leaves the CPU as `step` would after the instruction (`load`: the
    /// load it issued, still to land), and exits.
    fn leave(&mut self, after: After, load: Option<(usize, u32)>) {
        let l = self.l;
        match after {
            After::Plain { pc } => {
                self.store_cpu(l.pc, |c| {
                    c.i32_const(pc as i32);
                });
                self.store_cpu(l.next_pc, |c| {
                    c.i32_const(pc.wrapping_add(4) as i32);
                });
            }
            After::Branch { pc } => {
                self.store_cpu(l.pc, |c| {
                    c.i32_const(pc as i32);
                });
                self.store_cpu(l.next_pc, |c| {
                    c.local_get(NEXT);
                });
                self.c.local_get(CPU).local_get(TAKEN).mem(op::I32_STORE8, 0, l.branch);
            }
            After::Delay => {
                self.store_cpu(l.pc, |c| {
                    c.local_get(NEXT);
                });
                self.store_cpu(l.next_pc, |c| {
                    c.local_get(NEXT).i32_const(4).op(op::I32_ADD);
                });
            }
        }
        if let Some((reg, local)) = load {
            self.store_cpu(l.load_reg, |c| {
                c.i32_const(reg as i32);
            });
            self.store_cpu(l.load_val, |c| {
                c.local_get(local);
            });
        }
        self.c.br(self.depth);
    }

    /// The fetch's extra cycles, into EXTRA (as `Cpu::fetch_cycles`, less
    /// the instruction's own cycle).
    fn fetch(&mut self, pc: u32, icache: bool) {
        let p = pc & 0x1fff_ffff;
        let uncached = match p {
            0x0000_0000..=0x007f_ffff => RAM_ACCESS,
            0x1fc0_0000..=0x1fc7_ffff => BIOS_ACCESS,
            _ => IO_ACCESS,
        };
        if pc >> 29 == 5 || !icache {
            self.c.i32_const(uncached.saturating_sub(1) as i32).local_set(EXTRA);
            return;
        }
        let line = self.l.icache + 4 * ((pc >> 4) & 0xff);
        let tag = (p >> 12) + 1;
        let fill = if uncached == RAM_ACCESS {
            ICACHE_FILL
        } else {
            uncached * 4
        };
        self.c
            .i32_const(line as i32)
            .mem(op::I32_LOAD, 2, 0)
            .i32_const(tag as i32)
            .op(op::I32_EQ);
        self.c.if_i32().i32_const(0).op(op::ELSE);
        self.c
            .i32_const(line as i32)
            .i32_const(tag as i32)
            .mem(op::I32_STORE, 2, 0);
        self.c
            .i32_const(fill.saturating_sub(1) as i32)
            .op(op::END)
            .local_set(EXTRA);
    }

    /// Counts the instruction's cycles (1 + EXTRA), then leaves when the bus's
    /// deadline is reached (and, for a store, when it wrote the block's own code).
    fn commit(&mut self, after: After, load: Option<(usize, u32)>, smc: bool) {
        self.c.local_get(EXTRA).i32_const(1).op(op::I32_ADD).local_set(RET);
        if self.l.flat {
            self.c.local_get(PEND).i32_const(1).op(op::I32_ADD).local_set(PEND);
        } else {
            self.c.local_get(PEND).local_get(RET).op(op::I32_ADD).local_set(PEND);
        }
        self.c
            .local_get(CYC)
            .local_get(RET)
            .op(op::I64_EXTEND_I32_U)
            .op(op::I64_ADD)
            .local_set(CYC);
        self.c.local_get(N).i32_const(1).op(op::I32_ADD).local_set(N);
        self.c
            .local_get(PEND)
            .local_get(BUS)
            .mem(op::I32_LOAD, 2, self.l.deadline)
            .op(op::I32_GE_U);
        if smc {
            self.c.local_get(SMC).op(op::I32_OR);
        }
        self.c.if_();
        self.depth += 1;
        self.leave(after, load);
        self.depth -= 1;
        self.c.op(op::END);
    }

    /// Runs the instruction through the interpreter (`in_delay`: in a delay
    /// slot, taken when TAKEN is set), with the load landing after it;
    /// leaves the block when it says to, or at the bus's deadline.
    /// `stop`: it always says to.
    fn interpret(&mut self, instr: u32, pc: u32, in_delay: bool, landing: Option<(usize, u32)>, exec: u32, stop: bool) {
        let l = self.l;
        self.store_cpu(l.next_pc, |c| {
            if in_delay {
                c.local_get(NEXT);
            } else {
                c.i32_const(pc.wrapping_add(4) as i32);
            }
        });
        if let Some((reg, local)) = landing {
            self.store_cpu(l.load_reg, |c| {
                c.i32_const(reg as i32);
            });
            self.store_cpu(l.load_val, |c| {
                c.local_get(local);
            });
        }
        // The interpreter sees the cycles and pending cycles so far; it may
        // sync the bus (an I/O access), so the pending cycles are the bus's
        // afterwards, plus the instruction's.
        self.c.local_get(CPU).local_get(CYC).mem(op::I64_STORE, 3, l.cycles);
        self.c.local_get(BUS).local_get(PEND).mem(op::I32_STORE, 2, l.pending);
        self.c
            .local_get(CPU)
            .local_get(BUS)
            .i32_const(instr as i32)
            .i32_const(pc as i32);
        if in_delay {
            self.c.local_get(TAKEN);
        } else {
            self.c.i32_const(0);
        }
        self.c.local_get(EXTRA).i32_const(1).op(op::I32_SHL).op(op::I32_OR);
        self.c.i32_const(exec as i32).call_indirect(EXEC_TYPE).local_set(RET);
        self.c.local_get(CPU).mem(op::I64_LOAD, 3, l.cycles).local_set(CYC);
        self.c.local_get(BUS).mem(op::I32_LOAD, 2, l.pending);
        self.c
            .local_get(RET)
            .i32_const(0x7fff_ffff)
            .op(op::I32_AND)
            .op(op::I32_ADD)
            .local_set(PEND);
        self.c.local_get(N).i32_const(1).op(op::I32_ADD).local_set(N);
        // The interpreter left the CPU as `step` would.
        if stop {
            self.c.br(self.depth);
            return;
        }
        self.c.local_get(RET).i32_const(31).op(op::I32_SHR_U);
        self.c
            .local_get(PEND)
            .local_get(BUS)
            .mem(op::I32_LOAD, 2, l.deadline)
            .op(op::I32_GE_U)
            .op(op::I32_OR)
            .br_if(self.depth);
    }
}

/// Whether a loop (`words`: its instructions, the branch back to the first
/// and its delay slot last) does the same every round from its second, as
/// long as memory does not change: only instructions compiled to
/// WebAssembly, no stores, no link register written, no load landing after
/// it, and every register it reads either not written in it, or written
/// before in the same round (a load's from the second instruction after
/// it). A wait for a vertical blank's counter, for instance.
fn idle_loop(words: &[u32]) -> bool {
    /// Registers an instruction reads, and the one it writes (and whether
    /// by a load).
    type Regs = ([usize; 2], Option<(usize, bool)>);
    // `None` when the instruction disqualifies the loop.
    fn regs(i: u32) -> Option<Regs> {
        let rs = ((i >> 21) & 0x1f) as usize;
        let rt = ((i >> 16) & 0x1f) as usize;
        let rd = ((i >> 11) & 0x1f) as usize;
        Some(match i >> 26 {
            0x00 => match i & 0x3f {
                0x00 | 0x02 | 0x03 => ([rt, 0], Some((rd, false))),
                0x04 | 0x06 | 0x07 | 0x21 | 0x23..=0x27 | 0x2a | 0x2b => ([rs, rt], Some((rd, false))),
                0x08 => ([rs, 0], None),
                _ => return None,
            },
            0x01 if (i >> 17) & 0xf != 0x8 => ([rs, 0], None),
            0x02 => ([0, 0], None),
            0x04 | 0x05 => ([rs, rt], None),
            0x06 | 0x07 => ([rs, 0], None),
            0x09..=0x0e => ([rs, 0], Some((rt, false))),
            0x0f => ([0, 0], Some((rt, false))),
            0x20 | 0x21 | 0x23 | 0x24 | 0x25 => ([rs, 0], Some((rt, true))),
            _ => return None,
        })
    }
    let mut written = 0u32;
    for &i in words {
        match regs(i) {
            Some((_, Some((r, _)))) => written |= 1u32 << r,
            Some(_) => {}
            None => return false,
        }
    }
    written &= !1;
    let mut ready = 0u32;
    let mut landing = None;
    for &i in words {
        let Some((reads, write)) = regs(i) else {
            return false;
        };
        if reads.iter().any(|&r| written & !ready & (1u32 << r) != 0) {
            return false;
        }
        if let Some(r) = landing.take() {
            ready |= 1u32 << r;
        }
        match write {
            Some((r, true)) => landing = Some(r),
            Some((r, false)) => ready |= 1 << r,
            None => {}
        }
    }
    landing.is_none()
}

/// Where a branch or jump at `at` goes, when that is known when compiling
/// (not for register jumps).
fn branch_target(i: u32, at: u32) -> Option<u32> {
    let delay = at.wrapping_add(4);
    match i >> 26 {
        0x02 | 0x03 => Some((delay & 0xf000_0000) | ((i & 0x3ff_ffff) << 2)),
        0x01 | 0x04..=0x07 => Some(delay.wrapping_add(((i & 0xffff) as i16 as i32 as u32) << 2)),
        _ => None,
    }
}

/// Whether a branch is conditional (not a jump: J, JAL, JR, JALR).
fn conditional(i: u32) -> bool {
    matches!(i >> 26, 0x01 | 0x04..=0x07)
}

/// What an instruction is, for compiling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// In WebAssembly.
    Native,
    /// A branch or jump (in WebAssembly).
    Branch,
    /// Through the interpreter; the block may go on after it.
    Pure,
    /// Through the interpreter; the block stops after it.
    Stop,
}

fn class(i: u32) -> Class {
    let op = i >> 26;
    match op {
        0x00 => match i & 0x3f {
            0x00 | 0x02 | 0x03 | 0x04 | 0x06 | 0x07 | 0x21 | 0x23..=0x27 | 0x2a | 0x2b => Class::Native,
            0x08 | 0x09 => Class::Branch,
            0x10..=0x1b | 0x20 | 0x22 => Class::Pure,
            _ => Class::Stop,
        },
        0x01..=0x07 => Class::Branch,
        0x08 => Class::Pure,
        0x09..=0x0f => Class::Native,
        0x12 => {
            let rs = (i >> 21) & 0x1f;
            if rs & 0x10 != 0 || rs == 0x04 || rs == 0x06 {
                Class::Pure
            } else {
                Class::Stop
            }
        }
        // Loads and stores: in WebAssembly when they hit RAM or the
        // scratchpad, else they stop the block.
        0x20 | 0x21 | 0x23 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2b => Class::Native,
        _ => Class::Stop,
    }
}

/// Compiles the block at `pc` from its words (`words[0]` is its first
/// instruction; the rest may be more than it uses), assuming the
/// instruction cache on or off (`icache`); `ram`: where its code is in RAM
/// (stores there stop it), if it is; `exec`: the interpreter's function's
/// table index.
pub fn compile(
    pc: u32,
    words: &[u32],
    icache: bool,
    ram: Option<(u32, u32)>,
    exec: u32,
    layout: &Layout,
) -> Option<Compiled> {
    // How many instructions: to the delay slot of the first jump, or of
    // the first branch back to the block's start (a loop, see `back`). A
    // block goes on after other conditional branches, as when they are
    // not taken, and leaves for their destination when they are: fewer,
    // longer blocks (their dispatch cost what they ran, in profiles). A
    // branch with no delay slot here, or a branch in it, ends the block
    // before it.
    let mut len = 0;
    while len < words.len().min(MAX_LEN) {
        match class(words[len]) {
            Class::Branch => {
                let slot = words.get(len + 1).map(|&w| class(w));
                if slot.is_none_or(|s| s == Class::Branch) {
                    break;
                }
                let (branch, at) = (words[len], pc.wrapping_add(4 * len as u32));
                len += 2;
                let goes_on = conditional(branch) && branch_target(branch, at) != Some(pc) && slot != Some(Class::Stop);
                if !goes_on {
                    break;
                }
            }
            Class::Stop => {
                len += 1;
                break;
            }
            _ => len += 1,
        }
    }
    if len == 0 {
        return None;
    }
    // A block ending with a branch back to its start (a loop) runs again
    // while it is taken, without returning: as `step`'s caller would, as
    // nothing outside the CPU could have changed (the block leaves for
    // that: at the bus's deadline, an I/O access, a store to its code).
    let back = len >= 2 && class(words[len - 2]) == Class::Branch && {
        let at = pc.wrapping_add(4 * (len as u32 - 2));
        branch_target(words[len - 2], at) == Some(pc)
    };
    let idle = back && !layout.flat && idle_loop(&words[..len]);
    let mut e = Emitter {
        c: Code::default(),
        l: layout,
        // Inside the function's block and the loop.
        depth: 1,
    };
    let mut native = 0;
    // The function: a block its exits branch out of, then the pending
    // cycles and the CPU's cycles written back, and the count returned.
    e.c.local_get(BUS).mem(op::I32_LOAD, 2, layout.pending).local_set(PEND);
    e.c.local_get(CPU).mem(op::I64_LOAD, 3, layout.cycles).local_set(CYC);
    e.c.local_get(PEND).local_set(START);
    e.c.block();
    e.c.loop_();
    // The load landing after the current instruction: register, local.
    let mut landing: Option<(usize, u32)> = None;
    let mut in_delay = false;
    for k in 0..len {
        let i = words[k];
        let at = pc.wrapping_add(4 * k as u32);
        let next = at.wrapping_add(4);
        let after = if in_delay {
            After::Delay
        } else {
            After::Plain { pc: next }
        };
        let ops = (
            i >> 26,
            ((i >> 21) & 0x1f) as usize,
            ((i >> 16) & 0x1f) as usize,
            ((i >> 11) & 0x1f) as usize,
        );
        let (opcode, rs, rt, rd) = ops;
        let shamt = ((i >> 6) & 0x1f) as i32;
        let imm = i & 0xffff;
        let imm_se = imm as i16 as i32;
        let class = class(i);
        e.fetch(at, icache);
        match class {
            Class::Pure | Class::Stop => {
                e.interpret(i, at, in_delay, landing, exec, class == Class::Stop);
                landing = None;
            }
            Class::Native => {
                native += 1;
                let mine = LOADED[k % 2];
                let mut issued: Option<(usize, u32)> = None;
                let mut smc = false;
                let mut wrote = 0;
                match opcode {
                    0x00 => {
                        wrote = rd;
                        let f = i & 0x3f;
                        e.set_reg(rd, |e| {
                            match f {
                                0x00 | 0x02 | 0x03 => {
                                    e.reg(rt);
                                    e.c.i32_const(shamt);
                                }
                                0x04 | 0x06 | 0x07 => {
                                    e.reg(rt);
                                    e.reg(rs);
                                }
                                _ => {
                                    e.reg(rs);
                                    e.reg(rt);
                                }
                            }
                            match f {
                                0x00 | 0x04 => e.c.op(op::I32_SHL),
                                0x02 | 0x06 => e.c.op(op::I32_SHR_U),
                                0x03 | 0x07 => e.c.op(op::I32_SHR_S),
                                0x21 => e.c.op(op::I32_ADD),
                                0x23 => e.c.op(op::I32_SUB),
                                0x24 => e.c.op(op::I32_AND),
                                0x25 => e.c.op(op::I32_OR),
                                0x26 => e.c.op(op::I32_XOR),
                                0x27 => e.c.op(op::I32_OR).i32_const(-1).op(op::I32_XOR),
                                0x2a => e.c.op(op::I32_LT_S),
                                _ => e.c.op(op::I32_LT_U),
                            };
                        });
                    }
                    0x09..=0x0f => {
                        wrote = rt;
                        e.set_reg(rt, |e| {
                            if opcode == 0x0f {
                                e.c.i32_const((imm << 16) as i32);
                                return;
                            }
                            e.reg(rs);
                            match opcode {
                                0x09 => e.c.i32_const(imm_se).op(op::I32_ADD),
                                0x0a => e.c.i32_const(imm_se).op(op::I32_LT_S),
                                0x0b => e.c.i32_const(imm_se).op(op::I32_LT_U),
                                0x0c => e.c.i32_const(imm as i32).op(op::I32_AND),
                                0x0d => e.c.i32_const(imm as i32).op(op::I32_OR),
                                _ => e.c.i32_const(imm as i32).op(op::I32_XOR),
                            };
                        });
                    }
                    _ => {
                        // Loads and stores: RAM or the scratchpad, aligned
                        // (stores: the cache not isolated), else the
                        // interpreter (and the block stops).
                        let store = opcode >= 0x28;
                        let align: i32 = match opcode & 7 {
                            1 | 5 => 1,
                            3 => 3,
                            _ => 0,
                        };
                        e.reg(rs);
                        e.c.i32_const(imm_se).op(op::I32_ADD).local_set(ADDR);
                        // KIND: 1 RAM, 2 the scratchpad, 0 elsewhere (or
                        // misaligned, or a store with the cache isolated).
                        e.c.local_get(ADDR)
                            .i32_const(0x1fff_ffff)
                            .op(op::I32_AND)
                            .local_tee(KIND);
                        e.c.i32_const(0x0080_0000)
                            .op(op::I32_LT_U)
                            .if_i32()
                            .i32_const(1)
                            .op(op::ELSE);
                        e.c.local_get(KIND)
                            .i32_const(!0x3ff)
                            .op(op::I32_AND)
                            .i32_const(0x1f80_0000)
                            .op(op::I32_EQ);
                        e.c.i32_const(1).op(op::I32_SHL).op(op::END);
                        e.c.local_get(ADDR)
                            .i32_const(align)
                            .op(op::I32_AND)
                            .op(op::I32_EQZ)
                            .op(op::I32_MUL);
                        if store {
                            e.c.local_get(CPU)
                                .mem(op::I32_LOAD, 2, layout.sr)
                                .i32_const(0x1_0000)
                                .op(op::I32_AND)
                                .op(op::I32_EQZ);
                            e.c.op(op::I32_MUL);
                        }
                        e.c.local_set(KIND);
                        e.c.local_get(KIND).op(op::I32_EQZ).if_();
                        e.depth += 1;
                        e.interpret(i, at, in_delay, landing, exec, true);
                        e.depth -= 1;
                        e.c.op(op::END);
                        // The offset in RAM or the scratchpad, and its base.
                        e.c.local_get(KIND).i32_const(1).op(op::I32_EQ).if_i32();
                        e.c.local_get(ADDR).i32_const(layout.ram_mask as i32).op(op::I32_AND);
                        e.c.i32_const(layout.ram as i32).op(op::I32_ADD).op(op::ELSE);
                        e.c.local_get(ADDR)
                            .i32_const(0x3ff)
                            .op(op::I32_AND)
                            .local_get(BUS)
                            .op(op::I32_ADD);
                        e.c.i32_const(layout.scratchpad as i32).op(op::I32_ADD).op(op::END);
                        if store {
                            // The data, stored.
                            e.reg(rt);
                            let (o, a) = match opcode {
                                0x28 => (op::I32_STORE8, 0),
                                0x29 => (op::I32_STORE16, 1),
                                _ => (op::I32_STORE, 2),
                            };
                            e.c.mem(o, a, 0);
                            if let Some((lo, hi)) = ram {
                                // Its own code written: stop after it.
                                smc = true;
                                e.c.local_get(KIND).i32_const(1).op(op::I32_EQ);
                                e.c.local_get(ADDR)
                                    .i32_const(layout.ram_mask as i32)
                                    .op(op::I32_AND)
                                    .local_set(ADDR);
                                e.c.local_get(ADDR)
                                    .i32_const(lo as i32)
                                    .op(op::I32_GE_U)
                                    .op(op::I32_AND);
                                e.c.local_get(ADDR)
                                    .i32_const(hi as i32)
                                    .op(op::I32_LT_U)
                                    .op(op::I32_AND);
                                e.c.local_set(SMC);
                            }
                            // Counted, as the bus counts RAM writes (see
                            // `Bus::ram_writes`).
                            e.c.local_get(KIND).i32_const(1).op(op::I32_EQ).if_();
                            e.c.local_get(ADDR)
                                .i32_const(layout.ram_mask as i32)
                                .op(op::I32_AND)
                                .i32_const(RAM_PAGE_BITS as i32)
                                .op(op::I32_SHR_U)
                                .i32_const(2)
                                .op(op::I32_SHL)
                                .i32_const(layout.ram_writes as i32)
                                .op(op::I32_ADD)
                                .local_set(ADDR);
                            e.c.local_get(ADDR)
                                .local_get(ADDR)
                                .mem(op::I32_LOAD, 2, 0)
                                .i32_const(1)
                                .op(op::I32_ADD)
                                .mem(op::I32_STORE, 2, 0);
                            e.c.op(op::END);
                        } else {
                            let (o, a) = match opcode {
                                0x20 => (op::I32_LOAD8_S, 0),
                                0x21 => (op::I32_LOAD16_S, 1),
                                0x24 => (op::I32_LOAD8_U, 0),
                                0x25 => (op::I32_LOAD16_U, 1),
                                _ => (op::I32_LOAD, 2),
                            };
                            e.c.mem(o, a, 0).local_set(mine);
                            // RAM costs its access time; the scratchpad none.
                            e.c.local_get(EXTRA).local_get(KIND).i32_const(1).op(op::I32_EQ);
                            e.c.i32_const(RAM_ACCESS as i32)
                                .op(op::I32_MUL)
                                .op(op::I32_ADD)
                                .local_set(EXTRA);
                            if rt != 0 {
                                issued = Some((rt, mine));
                            }
                        }
                    }
                }
                // The previous load lands, unless this instruction wrote
                // the register.
                if let Some((reg, local)) = landing
                    && reg != wrote
                {
                    e.set_reg_from(reg, local);
                }
                e.commit(after, issued, smc);
                landing = issued;
            }
            Class::Branch => {
                native += 1;
                let delay = next;
                let mut link = None;
                // TAKEN and NEXT.
                match opcode {
                    0x00 => {
                        // JR, JALR: always taken.
                        e.reg(rs);
                        e.c.local_set(NEXT);
                        e.c.i32_const(1).local_set(TAKEN);
                        if i & 0x3f == 0x09 {
                            link = Some(rd);
                        }
                    }
                    0x02 | 0x03 => {
                        let target = (at.wrapping_add(4) & 0xf000_0000) | ((i & 0x3ff_ffff) << 2);
                        e.c.i32_const(target as i32).local_set(NEXT);
                        e.c.i32_const(1).local_set(TAKEN);
                        if opcode == 0x03 {
                            link = Some(31);
                        }
                    }
                    _ => {
                        let target = delay.wrapping_add((imm_se << 2) as u32);
                        match opcode {
                            0x01 => {
                                let ge = (i >> 16) & 1 != 0;
                                e.reg(rs);
                                e.c.i32_const(0).op(if ge { op::I32_GE_S } else { op::I32_LT_S });
                                if (i >> 17) & 0xf == 0x8 {
                                    link = Some(31);
                                }
                            }
                            0x04 | 0x05 => {
                                e.reg(rs);
                                e.reg(rt);
                                e.c.op(if opcode == 0x04 { op::I32_EQ } else { op::I32_NE });
                            }
                            0x06 => {
                                e.reg(rs);
                                e.c.i32_const(0).op(op::I32_LE_S);
                            }
                            _ => {
                                e.reg(rs);
                                e.c.i32_const(0).op(op::I32_GT_S);
                            }
                        }
                        e.c.local_tee(TAKEN);
                        e.c.if_i32().i32_const(target as i32).op(op::ELSE);
                        e.c.i32_const(delay.wrapping_add(4) as i32).op(op::END).local_set(NEXT);
                    }
                }
                let wrote = link.unwrap_or(0);
                if let Some(r) = link {
                    e.set_reg(r, |e| {
                        e.c.i32_const(delay.wrapping_add(4) as i32);
                    });
                }
                if let Some((reg, local)) = landing
                    && reg != wrote
                {
                    e.set_reg_from(reg, local);
                }
                landing = None;
                e.commit(After::Branch { pc: delay }, None, false);
                in_delay = true;
                continue;
            }
        }
        if in_delay {
            if k + 1 == len {
                break;
            }
            // A conditional branch: taken, the block leaves for its
            // destination; not, it goes on.
            e.c.local_get(TAKEN).if_();
            e.depth += 1;
            e.leave(After::Delay, landing);
            e.depth -= 1;
            e.c.op(op::END);
        }
        in_delay = false;
    }
    // The end: the next instruction, or the branch's destination.
    let last = pc.wrapping_add(4 * len as u32);
    let end = if in_delay {
        After::Delay
    } else {
        After::Plain { pc: last }
    };
    // Back to the start, the branch taken (and no load in flight: a block
    // starts with none).
    if back && in_delay && landing.is_none() {
        if idle {
            // A loop that does the same every round (waiting for memory
            // to change: only something outside the CPU can, at the bus's
            // deadline): the whole rounds left before the deadline at
            // once, as long as the last (the second or a later one: the
            // first may have filled the instruction cache).
            e.c.local_get(TAKEN).if_();
            e.c.local_get(ROUNDS).if_();
            // RET: the round's cycles; ADDR: how many more fit.
            e.c.local_get(PEND).local_get(START).op(op::I32_SUB).local_set(RET);
            e.c.local_get(BUS)
                .mem(op::I32_LOAD, 2, layout.deadline)
                .local_get(PEND)
                .op(op::I32_SUB)
                .local_get(RET)
                .op(op::I32_DIV_U)
                .local_set(ADDR);
            e.c.local_get(ADDR).local_get(RET).op(op::I32_MUL).local_set(RET);
            e.c.local_get(PEND).local_get(RET).op(op::I32_ADD).local_set(PEND);
            e.c.local_get(CYC)
                .local_get(RET)
                .op(op::I64_EXTEND_I32_U)
                .op(op::I64_ADD)
                .local_set(CYC);
            e.c.local_get(N)
                .local_get(ADDR)
                .i32_const(len as i32)
                .op(op::I32_MUL)
                .op(op::I32_ADD)
                .local_set(N);
            e.c.op(op::END);
            e.c.i32_const(1).local_set(ROUNDS);
            e.c.local_get(PEND).local_set(START);
            e.c.op(op::END);
        }
        e.c.local_get(TAKEN).br_if(0);
    }
    e.leave(end, landing);
    // The loop's end, then the block's.
    e.c.op(op::END);
    e.c.op(op::END);
    e.c.local_get(BUS).local_get(PEND).mem(op::I32_STORE, 2, layout.pending);
    e.c.local_get(CPU).local_get(CYC).mem(op::I64_STORE, 3, layout.cycles);
    e.c.local_get(N).op(op::END);
    Some(Compiled {
        code: e.c,
        len: len as u32,
        native,
    })
}

/// A module of compiled blocks, exporting them as `b0`, `b1`... in order:
/// `(cpu, bus) -> instructions run`, calling the interpreter's function
/// (type 1) through the table.
pub fn batch(blocks: &[&Code]) -> Vec<u8> {
    let types = [
        FuncType {
            params: &[I32, I32],
            results: &[I32],
        },
        FuncType {
            params: &[I32, I32, I32, I32, I32],
            results: &[I32],
        },
    ];
    let locals: &[(u32, u8)] = &[(2, I32), (1, I64), (11, I32)];
    let funcs: Vec<_> = blocks.iter().map(|&c| (locals, c)).collect();
    wasm::module_of(&types, &funcs)
}

#[cfg(test)]
mod idle_tests {
    use super::idle_loop;

    const NOP: u32 = 0;

    fn lw(rt: u32, base: u32) -> u32 {
        (0x23 << 26) | (base << 21) | (rt << 16)
    }

    fn slt(rd: u32, rs: u32, rt: u32) -> u32 {
        (rs << 21) | (rt << 16) | (rd << 11) | 0x2a
    }

    fn bne(rs: u32, rt: u32) -> u32 {
        (0x05 << 26) | (rs << 21) | (rt << 16) | 0xfffc
    }

    fn addiu(rt: u32, rs: u32, imm: i16) -> u32 {
        (0x09 << 26) | (rs << 21) | (rt << 16) | u32::from(imm as u16)
    }

    fn sw(rt: u32, base: u32) -> u32 {
        (0x2b << 26) | (base << 21) | (rt << 16)
    }

    #[test]
    fn a_wait_for_memory_is_idle() {
        // lw v0, 0(gp); nop; slt v0, v0, a0; bne v0, zero, start; nop
        assert!(idle_loop(&[lw(2, 28), NOP, slt(2, 2, 4), bne(2, 0), NOP]));
    }

    #[test]
    fn a_countdown_is_not() {
        assert!(!idle_loop(&[addiu(8, 8, -1), bne(8, 0), NOP]));
    }

    #[test]
    fn a_store_is_not() {
        assert!(!idle_loop(&[lw(2, 28), sw(2, 29), bne(2, 0), NOP]));
    }

    #[test]
    fn a_read_in_the_load_delay_is_not() {
        // The branch sees the last round's value.
        assert!(!idle_loop(&[lw(2, 28), bne(2, 0), NOP]));
    }
}
