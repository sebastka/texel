//! Compiles a block of EE code into a WebAssembly function:
//! `(ee: i32, bus: i32, left: i32) -> i32`, which runs the block on the
//! `Ee` and `Bus` at those addresses (of the emulator's own memory), for at
//! most `left` instructions, and returns how many it ran.
//!
//! A block runs from its first instruction to a branch and its delay slot,
//! or to an instruction after which the interpreter must look again (COP0,
//! SYSCALL, BREAK), or to the end of its 4 KiB page, or `MAX_LEN`
//! instructions. The integer instructions and the branches are compiled;
//! the others call `exec` (the interpreter, for that one instruction), and
//! the block stops early when one of those takes an exception or changes
//! where the EE goes next.

use std::mem::offset_of;

use super::super::Ee;
use super::EXEC_TYPE;
use super::wasm::{self, Code, FuncType, I32, op};

/// Instructions in a block, at most (its delay slot not counted).
pub const MAX_LEN: usize = 64;

/// The function's parameters and locals.
const EE: u32 = 0;
const BUS: u32 = 1;
const LEFT: u32 = 2;
const COND: u32 = 3;
const TARGET: u32 = 4;
const DEST: u32 = 5;
const VA: u32 = 6;
const PA: u32 = 7;
const TMP: u32 = 8;

/// Where the state compiled blocks read directly is, in the emulator's
/// memory (addresses, which only mean something in WebAssembly): the TLB's
/// page table, RAM, the scratchpad, and the bitmap of RAM lines holding
/// compiled code (all on the heap, where they stay); and, from the `Ee` (which may move),
/// whether the page table must be rebuilt, and COP0's Status.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    pub pages: u32,
    pub ram: u32,
    pub scratchpad: u32,
    pub code_lines: u32,
    pub stale_offset: u32,
    pub status_offset: u32,
}

/// The page table's flags (tlb.rs), and Status.ERL.
const VALID: i32 = 2;
const DIRTY: i32 = 4;
const STATUS_ERL: i32 = 1 << 2;
const RAM_SIZE: i32 = crate::bus::RAM_SIZE as i32;
/// The scratchpad: its physical address here (bus.rs), its size.
const SCRATCHPAD: u32 = crate::bus::SCRATCHPAD;
const SCRATCHPAD_SIZE: i32 = 16 * 1024;
const _: () = assert!(crate::bus::CODE_LINE == 64);

/// A block's code, read from memory: its instructions from `va`; and the
/// idle loops' branches among them (`beq $0, $0` back over NOPs only),
/// which the interpreter keeps, to spot them.
pub struct Source {
    pub va: u32,
    pub words: Vec<u32>,
    pub idle: Vec<u32>,
}

/// How a block ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Branch {
    /// Jumps to a fixed address (J, JAL): with a link, rd.
    Jump { target: u32, link: Option<u32> },
    /// Jumps to a register's address (JR, JALR).
    Register { rs: u32, link: Option<u32> },
    /// Branches when a condition holds; `likely`: the delay slot runs only
    /// then. `link`: r31 gets the return address, taken or not.
    If {
        cond: Cond,
        target: u32,
        likely: bool,
        link: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cond {
    Eq(u32, u32),
    Ne(u32, u32),
    /// rs <= 0, rs > 0, rs < 0, rs >= 0 (signed).
    Lez(u32),
    Gtz(u32),
    Ltz(u32),
    Gez(u32),
}

fn sext16(v: u32) -> u32 {
    v as u16 as i16 as i32 as u32
}

/// Whether `i` at `pc` is `beq $0, $0` back by at most 16 instructions:
/// maybe an idle loop's branch.
pub fn idle_candidate(i: u32, pc: u32) -> Option<u32> {
    let rel = pc.wrapping_add(4).wrapping_add(sext16(i & 0xFFFF) << 2);
    (i >> 16 == 0x1000 && rel < pc && pc - rel <= 64).then_some(rel)
}

/// The branch an instruction at `pc` is, if any; `Err(())` for one the
/// compiler leaves to the interpreter (idle loops', in `idle`).
fn branch(i: u32, pc: u32, idle: &[u32]) -> Result<Option<Branch>, ()> {
    let rs = (i >> 21) & 31;
    let rt = (i >> 16) & 31;
    let rd = (i >> 11) & 31;
    let rel = pc.wrapping_add(4).wrapping_add(sext16(i & 0xFFFF) << 2);
    let jump = (pc.wrapping_add(4) & 0xF000_0000) | ((i & 0x03FF_FFFF) << 2);
    let b = match i >> 26 {
        0x00 => match i & 0x3F {
            0x08 => Branch::Register { rs, link: None },
            0x09 => Branch::Register { rs, link: Some(rd) },
            _ => return Ok(None),
        },
        0x01 => {
            let cond = if rt & 1 == 0 { Cond::Ltz(rs) } else { Cond::Gez(rs) };
            match rt {
                0x00..=0x03 | 0x10..=0x13 => Branch::If {
                    cond,
                    target: rel,
                    likely: rt & 2 != 0,
                    link: rt & 0x10 != 0,
                },
                _ => return Ok(None),
            }
        }
        0x02 => Branch::Jump {
            target: jump,
            link: None,
        },
        0x03 => Branch::Jump {
            target: jump,
            link: Some(31),
        },
        op @ (0x04..=0x07 | 0x14..=0x17) => {
            // An idle loop's branch: the interpreter looks for it.
            if idle.contains(&pc) {
                return Err(());
            }
            let cond = match op & 3 {
                0 => Cond::Eq(rs, rt),
                1 => Cond::Ne(rs, rt),
                2 => Cond::Lez(rs),
                _ => Cond::Gtz(rs),
            };
            Branch::If {
                cond,
                target: rel,
                likely: op >= 0x14,
                link: false,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(b))
}

/// Whether the interpreter must look again after this instruction: COP0
/// (TLB, Status, ERET), SYSCALL and BREAK.
fn stops(i: u32) -> bool {
    i >> 26 == 0x10 || (i >> 26 == 0 && matches!(i & 0x3F, 0x0C | 0x0D))
}

fn gpr(r: u32) -> u32 {
    (offset_of!(Ee, gpr) + 16 * r as usize) as u32
}

const fn field(offset: usize) -> u32 {
    offset as u32
}

/// Emits the compiled block.
struct Gen {
    code: Code,
    exec: u32,
    layout: Layout,
    /// Instructions compiled (not left to the interpreter).
    natives: u32,
}

impl Gen {
    /// Pushes register `r`'s lower 64 bits.
    fn get(&mut self, r: u32) {
        if r == 0 {
            self.code.i64_const(0);
        } else {
            self.code.local_get(EE).mem(op::I64_LOAD, 3, gpr(r));
        }
    }

    /// Writes the lower 64 bits of `r` (if not r0) with what `value` pushes.
    fn set(&mut self, r: u32, value: impl FnOnce(&mut Self)) {
        if r == 0 {
            return;
        }
        self.code.local_get(EE);
        value(self);
        self.code.mem(op::I64_STORE, 3, gpr(r));
    }

    fn set_const(&mut self, r: u32, v: u64) {
        self.set(r, |g| {
            g.code.i64_const(v as i64);
        });
    }

    /// Stores `pc` and `next_pc` as constants.
    fn set_pc(&mut self, pc: u32) {
        self.code
            .local_get(EE)
            .i32_const(pc as i32)
            .mem(op::I32_STORE, 2, field(offset_of!(Ee, pc)));
        self.code.local_get(EE).i32_const(pc.wrapping_add(4) as i32).mem(
            op::I32_STORE,
            2,
            field(offset_of!(Ee, next_pc)),
        );
    }

    /// Stores `pc` and `next_pc` from the DEST local.
    fn set_pc_dest(&mut self) {
        self.code
            .local_get(EE)
            .local_get(DEST)
            .mem(op::I32_STORE, 2, field(offset_of!(Ee, pc)));
        self.code
            .local_get(EE)
            .local_get(DEST)
            .i32_const(4)
            .op(0x6A) // i32.add
            .mem(op::I32_STORE, 2, field(offset_of!(Ee, next_pc)));
    }

    /// Runs instruction `i` at `pc` through the interpreter: `index`
    /// instructions of the block ran before it; `slot`: it is a delay slot
    /// (pc and next_pc then come from DEST). Returns from the block when it
    /// says so.
    fn exec(&mut self, i: u32, pc: u32, index: u32, slot: bool) {
        if slot {
            self.set_pc_dest();
        } else {
            self.set_pc(pc.wrapping_add(4));
        }
        self.code
            .local_get(EE)
            .local_get(BUS)
            .i32_const(i as i32)
            .i32_const(pc as i32)
            .i32_const(((index << 1) | u32::from(slot)) as i32)
            .i32_const(self.exec as i32)
            .call_indirect(EXEC_TYPE);
        self.code.if_().i32_const(index as i32 + 1).op(op::RETURN).op(op::END);
    }

    /// Compiles `i` if it is an integer instruction, else runs it through
    /// the interpreter.
    fn instruction(&mut self, i: u32, pc: u32, index: u32, slot: bool) {
        if self.memory(i, pc, index, slot) || self.native(i) {
            self.natives += 1;
        } else {
            self.exec(i, pc, index, slot);
        }
    }

    /// Leaves in PA the physical address of VA, or -1 where the interpreter
    /// must translate it (the TLB fault, a table to rebuild, ERL set).
    fn translate(&mut self, write: bool) {
        let l = self.layout;
        let need = if write { VALID | DIRTY } else { VALID };
        // kseg0 and kseg1: fixed windows.
        self.code
            .local_get(VA)
            .i32_const(0xC000_0000u32 as i32)
            .op(op::I32_AND)
            .i32_const(0x8000_0000u32 as i32)
            .op(op::I32_EQ)
            .if_i32();
        self.code.local_get(VA).i32_const(0x1FFF_FFFF).op(op::I32_AND);
        self.code.op(op::ELSE);
        // The page table, if current and ERL is clear.
        self.code
            .local_get(EE)
            .mem(op::I32_LOAD8_U, 0, l.stale_offset)
            .local_get(EE)
            .mem(op::I32_LOAD, 2, l.status_offset)
            .i32_const(STATUS_ERL)
            .op(op::I32_AND)
            .op(op::I32_OR)
            .if_i32();
        self.code.i32_const(-1);
        self.code.op(op::ELSE);
        self.code
            .local_get(VA)
            .i32_const(12)
            .op(op::I32_SHR_U)
            .i32_const(2)
            .op(op::I32_SHL)
            .mem(op::I32_LOAD, 2, l.pages)
            .local_tee(TMP)
            .i32_const(need)
            .op(op::I32_AND)
            .i32_const(need)
            .op(op::I32_EQ)
            .if_i32();
        self.code
            .local_get(TMP)
            .i32_const(!0xFFF)
            .op(op::I32_AND)
            .local_get(VA)
            .i32_const(0xFFF)
            .op(op::I32_AND)
            .op(op::I32_OR);
        self.code.op(op::ELSE).i32_const(-1).op(op::END);
        self.code.op(op::END);
        self.code.op(op::END).local_set(PA);
    }

    /// Pushes whether PA is RAM (and, for writes, not compiled code): the
    /// fast path.
    fn fast(&mut self, write: bool) {
        self.code.local_get(PA).i32_const(RAM_SIZE).op(op::I32_LT_U);
        if write {
            // Its line's bit in the bitmap of compiled code is clear.
            self.code
                .local_get(PA)
                .i32_const(6)
                .op(op::I32_SHR_U)
                .local_tee(TMP)
                .i32_const(6)
                .op(op::I32_SHR_U)
                .i32_const(3)
                .op(op::I32_SHL)
                .mem(op::I64_LOAD, 3, self.layout.code_lines)
                .local_get(TMP)
                .op(op::I64_EXTEND_I32_U)
                .op(op::I64_SHR_U)
                .i64_const(1)
                .op(op::I64_AND)
                .op(op::I64_EQZ)
                .op(op::I32_AND);
        }
    }

    /// Compiles loads and stores to and from RAM, with the interpreter for
    /// other memory and for faults; false if `i` is not one.
    fn memory(&mut self, i: u32, pc: u32, index: u32, slot: bool) -> bool {
        let rs = (i >> 21) & 31;
        let rt = (i >> 16) & 31;
        let simm = sext16(i & 0xFFFF);
        // Loads: size, wasm load; stores: size, wasm store.
        let (size, load, store) = match i >> 26 {
            0x20 => (1, op::I64_LOAD8_S, 0),
            0x21 => (2, op::I64_LOAD16_S, 0),
            0x23 => (4, op::I64_LOAD32_S, 0),
            0x24 => (1, op::I64_LOAD8_U, 0),
            0x25 => (2, op::I64_LOAD16_U, 0),
            0x27 => (4, op::I64_LOAD32_U, 0),
            0x37 => (8, op::I64_LOAD, 0),
            0x1E => (16, op::I64_LOAD, 0),
            0x28 => (1, 0, op::I64_STORE8),
            0x29 => (2, 0, op::I64_STORE16),
            0x2B => (4, 0, op::I64_STORE32),
            0x3F => (8, 0, op::I64_STORE),
            0x1F => (16, 0, op::I64_STORE),
            _ => return false,
        };
        let write = store != 0;
        if !write && rt == 0 {
            return false;
        }
        // VA = rs + offset (LQ and SQ: aligned to 16 bytes).
        self.get(rs);
        self.code.op(op::I32_WRAP_I64).i32_const(simm as i32).op(op::I32_ADD);
        if size == 16 {
            self.code.i32_const(!15).op(op::I32_AND);
        }
        self.code.local_set(VA);
        self.translate(write);
        // RAM, then the scratchpad (from its TLB entry), else the
        // interpreter.
        self.fast(write);
        self.code.if_();
        self.access(write, size, load, store, rt, self.layout.ram, !(size as i32 - 1));
        self.code.op(op::ELSE);
        self.code
            .local_get(PA)
            .i32_const(!(SCRATCHPAD_SIZE - 1))
            .op(op::I32_AND)
            .i32_const(SCRATCHPAD as i32)
            .op(op::I32_EQ)
            .if_();
        let mask = (SCRATCHPAD_SIZE - 1) & !(size as i32 - 1);
        self.access(write, size, load, store, rt, self.layout.scratchpad, mask);
        self.code.op(op::ELSE);
        self.exec(i, pc, index, slot);
        self.code.op(op::END);
        self.code.op(op::END);
        true
    }

    /// The access itself, at `base` + (PA & `mask`).
    #[allow(clippy::too_many_arguments)]
    fn access(&mut self, write: bool, size: u32, load: u8, store: u8, rt: u32, base: u32, mask: i32) {
        let at = |g: &mut Self| {
            g.code.local_get(PA).i32_const(mask).op(op::I32_AND);
        };
        if write {
            at(self);
            self.get(rt);
            self.code.mem(store, size.min(8).trailing_zeros(), base);
            if size == 16 {
                at(self);
                self.get_high(rt);
                self.code.mem(store, 3, base + 8);
            }
        } else {
            self.code.local_get(EE);
            at(self);
            self.code.mem(load, size.min(8).trailing_zeros(), base);
            self.code.mem(op::I64_STORE, 3, gpr(rt));
            if size == 16 {
                self.code.local_get(EE);
                at(self);
                self.code.mem(op::I64_LOAD, 3, base + 8);
                self.code.mem(op::I64_STORE, 3, gpr(rt) + 8);
            }
        }
    }

    /// Pushes register `r`'s upper 64 bits.
    fn get_high(&mut self, r: u32) {
        if r == 0 {
            self.code.i64_const(0);
        } else {
            self.code.local_get(EE).mem(op::I64_LOAD, 3, gpr(r) + 8);
        }
    }

    /// Emits a 64-bit binary operation of rs and rt into rd.
    fn binary(&mut self, rd: u32, rs: u32, rt: u32, ops: &[u8]) {
        self.set(rd, |g| {
            g.get(rs);
            g.get(rt);
            for &o in ops {
                g.code.op(o);
            }
        });
    }

    /// Compiles `i`; false if it is not one the compiler knows.
    fn native(&mut self, i: u32) -> bool {
        let rs = (i >> 21) & 31;
        let rt = (i >> 16) & 31;
        let rd = (i >> 11) & 31;
        let sa = (i >> 6) & 31;
        let imm = i & 0xFFFF;
        let simm = i64::from(imm as u16 as i16);
        match i >> 26 {
            0x00 => match i & 0x3F {
                // SLL, SRL, SRA: on the lower 32 bits, sign-extended.
                0x00 => self.set(rd, |g| {
                    g.get(rt);
                    g.code.i64_const(i64::from(sa)).op(op::I64_SHL).op(op::I64_EXTEND32_S);
                }),
                0x02 => self.set(rd, |g| {
                    g.get(rt);
                    g.code
                        .i64_const(0xFFFF_FFFF)
                        .op(op::I64_AND)
                        .i64_const(i64::from(sa))
                        .op(op::I64_SHR_U)
                        .op(op::I64_EXTEND32_S);
                }),
                0x03 => self.set(rd, |g| {
                    g.get(rt);
                    g.code.op(op::I64_EXTEND32_S).i64_const(i64::from(sa)).op(op::I64_SHR_S);
                }),
                // SLLV, SRLV, SRAV.
                0x04 => self.set(rd, |g| {
                    g.get(rt);
                    g.get(rs);
                    g.code
                        .i64_const(31)
                        .op(op::I64_AND)
                        .op(op::I64_SHL)
                        .op(op::I64_EXTEND32_S);
                }),
                0x06 => self.set(rd, |g| {
                    g.get(rt);
                    g.code.i64_const(0xFFFF_FFFF).op(op::I64_AND);
                    g.get(rs);
                    g.code
                        .i64_const(31)
                        .op(op::I64_AND)
                        .op(op::I64_SHR_U)
                        .op(op::I64_EXTEND32_S);
                }),
                0x07 => self.set(rd, |g| {
                    g.get(rt);
                    g.code.op(op::I64_EXTEND32_S);
                    g.get(rs);
                    g.code.i64_const(31).op(op::I64_AND).op(op::I64_SHR_S);
                }),
                // MOVZ, MOVN.
                0x0A | 0x0B => {
                    if rd != 0 {
                        self.get(rt);
                        self.code.op(op::I64_EQZ);
                        if i & 1 == 1 {
                            self.code.op(op::I32_EQZ);
                        }
                        self.code.if_();
                        self.set(rd, |g| g.get(rs));
                        self.code.op(op::END);
                    }
                }
                // SYNC.
                0x0F => {}
                // MFHI, MTHI, MFLO, MTLO: the lower 64 bits.
                0x10 | 0x12 => {
                    let at = if i & 2 == 0 {
                        offset_of!(Ee, hi)
                    } else {
                        offset_of!(Ee, lo)
                    };
                    self.set(rd, |g| {
                        g.code.local_get(EE).mem(op::I64_LOAD, 3, field(at));
                    });
                }
                0x11 | 0x13 => {
                    let at = if i & 2 == 0 {
                        offset_of!(Ee, hi)
                    } else {
                        offset_of!(Ee, lo)
                    };
                    self.code.local_get(EE);
                    self.get(rs);
                    self.code.mem(op::I64_STORE, 3, field(at));
                }
                // DSLLV, DSRLV, DSRAV (wasm masks the amount by 63 too).
                0x14 => self.binary(rd, rt, rs, &[op::I64_SHL]),
                0x16 => self.binary(rd, rt, rs, &[op::I64_SHR_U]),
                0x17 => self.binary(rd, rt, rs, &[op::I64_SHR_S]),
                // ADD (no overflow trap), ADDU, SUB, SUBU.
                0x20 | 0x21 => self.binary(rd, rs, rt, &[op::I64_ADD, op::I64_EXTEND32_S]),
                0x22 | 0x23 => self.binary(rd, rs, rt, &[op::I64_SUB, op::I64_EXTEND32_S]),
                0x24 => self.binary(rd, rs, rt, &[op::I64_AND]),
                0x25 => self.binary(rd, rs, rt, &[op::I64_OR]),
                0x26 => self.binary(rd, rs, rt, &[op::I64_XOR]),
                0x27 => self.set(rd, |g| {
                    g.get(rs);
                    g.get(rt);
                    g.code.op(op::I64_OR).i64_const(-1).op(op::I64_XOR);
                }),
                0x2A => self.binary(rd, rs, rt, &[op::I64_LT_S, op::I64_EXTEND_I32_U]),
                0x2B => self.binary(rd, rs, rt, &[op::I64_LT_U, op::I64_EXTEND_I32_U]),
                // DADD (no overflow trap), DADDU, DSUB, DSUBU.
                0x2C | 0x2D => self.binary(rd, rs, rt, &[op::I64_ADD]),
                0x2E | 0x2F => self.binary(rd, rs, rt, &[op::I64_SUB]),
                // DSLL, DSRL, DSRA, and their +32 forms.
                f @ (0x38 | 0x3A | 0x3B | 0x3C | 0x3E | 0x3F) => {
                    let shift = i64::from(sa) + if f >= 0x3C { 32 } else { 0 };
                    let o = match f & 3 {
                        0 => op::I64_SHL,
                        2 => op::I64_SHR_U,
                        _ => op::I64_SHR_S,
                    };
                    self.set(rd, |g| {
                        g.get(rt);
                        g.code.i64_const(shift).op(o);
                    });
                }
                _ => return false,
            },
            // ADDI (no overflow trap), ADDIU.
            0x08 | 0x09 => self.set(rt, |g| {
                g.get(rs);
                g.code.i64_const(simm).op(op::I64_ADD).op(op::I64_EXTEND32_S);
            }),
            0x0A => self.set(rt, |g| {
                g.get(rs);
                g.code.i64_const(simm).op(op::I64_LT_S).op(op::I64_EXTEND_I32_U);
            }),
            0x0B => self.set(rt, |g| {
                g.get(rs);
                g.code.i64_const(simm).op(op::I64_LT_U).op(op::I64_EXTEND_I32_U);
            }),
            0x0C..=0x0E => {
                let o = [op::I64_AND, op::I64_OR, op::I64_XOR][(i >> 26) as usize - 0x0C];
                self.set(rt, |g| {
                    g.get(rs);
                    g.code.i64_const(i64::from(imm)).op(o);
                });
            }
            0x0F => self.set_const(rt, i64::from((imm << 16) as i32) as u64),
            // DADDI (no overflow trap), DADDIU.
            0x18 | 0x19 => self.set(rt, |g| {
                g.get(rs);
                g.code.i64_const(simm).op(op::I64_ADD);
            }),
            // CACHE, PREF.
            0x2F | 0x33 => {}
            _ => return false,
        }
        true
    }

    /// Pushes the branch condition (i32).
    fn cond(&mut self, c: Cond) {
        match c {
            Cond::Eq(s, t) | Cond::Ne(s, t) => {
                self.get(s);
                self.get(t);
                self.code.op(if matches!(c, Cond::Eq(..)) {
                    op::I64_EQ
                } else {
                    op::I64_NE
                });
            }
            Cond::Lez(s) | Cond::Gtz(s) | Cond::Ltz(s) | Cond::Gez(s) => {
                self.get(s);
                self.code.i64_const(0).op(match c {
                    Cond::Lez(_) => op::I64_LE_S,
                    Cond::Gtz(_) => op::I64_GT_S,
                    Cond::Ltz(_) => op::I64_LT_S,
                    _ => op::I64_GE_S,
                });
            }
        }
    }
}

/// The words from `k` to the end of the segment starting there: up to and
/// including the next branch's delay slot, as `compile` goes.
fn segment(src: &Source, k: usize) -> usize {
    let mut n = k;
    while n < src.words.len() && n < MAX_LEN {
        let i = src.words[n];
        let pc = src.va.wrapping_add(4 * n as u32);
        match branch(i, pc, &src.idle) {
            Err(()) => break,
            Ok(None) => {
                n += 1;
                if stops(i) {
                    break;
                }
            }
            Ok(Some(_)) => {
                let plain = src
                    .words
                    .get(n + 1)
                    .is_some_and(|&s| matches!(branch(s, pc + 4, &src.idle), Ok(None)) && !stops(s));
                if plain {
                    n += 2;
                }
                break;
            }
        }
    }
    n - k
}

/// A compiled block.
pub struct Compiled {
    pub module: Vec<u8>,
    /// The instructions it must have left to start: its first segment.
    pub first: u32,
    /// Its instructions, and those compiled (not left to the interpreter;
    /// branches included).
    pub words: u32,
    pub native: u32,
}

/// Compiles the block at `src`; `None` when there is nothing to compile
/// (its first instruction is one the interpreter keeps).
///
/// Conditional branches leave the block when taken, and it goes on when
/// not (a superblock); jumps end it. Each segment after the first starts by
/// leaving the block if it has fewer instructions left than the segment
/// needs. Every way out stores pc and next_pc and returns how many
/// instructions ran.
///
/// `exec`: the function-table index of the interpreter's entry point (see
/// `Backend::exec_index`); `layout`: where the EE's memory is.
pub fn compile(src: &Source, exec: u32, layout: Layout) -> Option<Compiled> {
    let mut g = Gen {
        code: Code::default(),
        exec,
        layout,
        natives: 0,
    };
    // k: the next word; ran: the instructions run so far on the path
    // compiled (a likely branch not taken skips its delay slot).
    let mut k = 0;
    let mut ran = 0u32;
    let mut ended = false;
    let first = segment(src, 0);
    let mut segment_start = true;
    while k < src.words.len() && k < MAX_LEN {
        let i = src.words[k];
        let pc = src.va.wrapping_add(4 * k as u32);
        if segment_start && k > 0 {
            // Out here unless the next segment fits in what is left.
            let need = segment(src, k) as i32;
            g.code
                .local_get(LEFT)
                .i32_const(ran as i32 + need)
                .op(op::I32_LT_U)
                .if_();
            g.set_pc(pc);
            g.code.i32_const(ran as i32).op(op::RETURN).op(op::END);
        }
        segment_start = false;
        let b = match branch(i, pc, &src.idle) {
            Err(()) => break,
            Ok(None) => {
                g.instruction(i, pc, ran, false);
                ran += 1;
                k += 1;
                if stops(i) {
                    break;
                }
                continue;
            }
            Ok(Some(b)) => b,
        };
        // The delay slot must be in the block's page, and plain.
        let Some(&slot) = src.words.get(k + 1) else { break };
        if !matches!(branch(slot, pc + 4, &src.idle), Ok(None)) || stops(slot) {
            break;
        }
        let after = pc.wrapping_add(8);
        g.natives += 1;
        // DEST: where the EE goes after the delay slot. Registers are read
        // before the delay slot runs, and links written before it too.
        match b {
            Branch::Jump { target, link } => {
                if let Some(r) = link {
                    g.set_const(r, u64::from(after));
                }
                g.code.i32_const(target as i32).local_set(DEST);
            }
            Branch::Register { rs, link } => {
                g.get(rs);
                g.code.op(op::I32_WRAP_I64).local_set(TARGET);
                if let Some(r) = link {
                    g.set_const(r, u64::from(after));
                }
                g.code.local_get(TARGET).local_set(DEST);
            }
            Branch::If { cond, target, link, .. } => {
                g.cond(cond);
                g.code.local_set(COND);
                if link {
                    g.set_const(31, u64::from(after));
                }
                g.code
                    .i32_const(target as i32)
                    .i32_const(after as i32)
                    .local_get(COND)
                    .op(op::SELECT)
                    .local_set(DEST);
            }
        }
        match b {
            Branch::If { likely: true, .. } => {
                // Taken: the delay slot, then out. Not taken: the slot is
                // skipped (and takes no time), and the block goes on.
                g.code.local_get(COND).if_();
                g.instruction(slot, pc + 4, ran + 1, true);
                g.set_pc_dest();
                g.code.i32_const(ran as i32 + 2).op(op::RETURN).op(op::END);
                ran += 1;
            }
            Branch::If { .. } => {
                g.instruction(slot, pc + 4, ran + 1, true);
                g.code.local_get(COND).if_();
                g.set_pc_dest();
                g.code.i32_const(ran as i32 + 2).op(op::RETURN).op(op::END);
                ran += 2;
            }
            _ => {
                g.instruction(slot, pc + 4, ran + 1, true);
                g.set_pc_dest();
                g.code.i32_const(ran as i32 + 2);
                ran += 2;
                ended = true;
            }
        }
        k += 2;
        segment_start = true;
        if ended {
            break;
        }
    }
    if k == 0 {
        return None;
    }
    if !ended {
        g.set_pc(src.va.wrapping_add(4 * k as u32));
        g.code.i32_const(ran as i32);
    }
    g.code.op(op::END);
    let module = wasm::module(
        &[
            FuncType {
                params: &[I32, I32, I32],
                results: &[I32],
            },
            FuncType {
                params: &[I32, I32, I32, I32, I32],
                results: &[I32],
            },
        ],
        &[(6, I32)],
        &g.code,
    );
    Some(Compiled {
        module,
        first: first as u32,
        words: k as u32,
        native: g.natives,
    })
}
