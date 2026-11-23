//! Compiles a VU microprogram (VU1's, or VU0's) into a WebAssembly function:
//! `(vu: i32, data: i32, gif: i32, gs: i32) -> i32`, which runs the program
//! from its start on the `Vu` at `vu` (its data memory at `data`), and
//! returns 0 when it ended, or 1 when it stopped for the interpreter to go
//! on (at `vu.pc`, after `vu.run_n` instructions; see `Vu::run`).
//!
//! The program is the blocks reachable from its start: straight runs of
//! instructions ending at a branch and its delay slot, or at an E bit and
//! the instruction after it. A loop dispatches on the PC between them; a
//! PC no block starts at (a jump through a register elsewhere) stops the
//! program there. So do the rare cases the blocks leave to the interpreter:
//! a branch in a delay slot, an E bit on a branch or before one.
//!
//! Each instruction does what the interpreter's does, in the same order:
//! Q's latency, the upper instruction's result (on four lanes at once,
//! with the PS2's float rules and its MAC flags), the lower instruction,
//! the upper's result committed, the flags recorded for the flag
//! instructions four later. Most lower instructions are compiled; the rare
//! ones (the EFU, XGKICK, the random numbers) call the interpreter's.

use std::collections::{BTreeMap, VecDeque};
use std::mem::offset_of;

use crate::ee::jit::wasm::{self, Code, FuncType, I32, op, uleb};
use crate::vu::{Flags, MAX_STEPS, PendingQ, Vu};

const V128: u8 = 0x7B;

/// Opcodes the shared encoder does not name.
const LOOP: u8 = 0x03;
const BR_TABLE: u8 = 0x0E;
const F32_EQ: u8 = 0x5B;
const F32_ABS: u8 = 0x8B;
const F32_SQRT: u8 = 0x91;
const F32_DIV: u8 = 0x95;
const I32_REINTERPRET_F32: u8 = 0xBC;
const F32_REINTERPRET_I32: u8 = 0xBE;

/// SIMD opcodes (after the 0xFD prefix).
mod simd {
    pub const LOAD: u32 = 0x00;
    pub const STORE: u32 = 0x0B;
    pub const CONST: u32 = 0x0C;
    pub const SHUFFLE: u32 = 0x0D;
    pub const I32X4_SPLAT: u32 = 0x11;
    pub const F32X4_SPLAT: u32 = 0x13;
    pub const F32X4_EXTRACT: u32 = 0x1F;
    pub const F32X4_REPLACE: u32 = 0x20;
    pub const I32X4_EQ: u32 = 0x37;
    pub const I32X4_NE: u32 = 0x38;
    pub const F32X4_LT: u32 = 0x43;
    pub const F32X4_GT: u32 = 0x44;
    pub const AND: u32 = 0x4E;
    pub const OR: u32 = 0x50;
    pub const BITSELECT: u32 = 0x52;
    pub const I32X4_BITMASK: u32 = 0xA4;
    pub const F32X4_NEG: u32 = 0xE1;
    pub const F32X4_ADD: u32 = 0xE4;
    pub const F32X4_SUB: u32 = 0xE5;
    pub const F32X4_MUL: u32 = 0xE6;
    pub const F32X4_DIV: u32 = 0xE7;
    pub const I32X4_TRUNC_SAT_F32X4_S: u32 = 0xF8;
    pub const F32X4_CONVERT_I32X4_S: u32 = 0xFA;
}

/// The function's parameters and locals.
const VU: u32 = 0;
const DATA: u32 = 1;
const GIF: u32 = 2;
const GS: u32 = 3;
const PC: u32 = 4;
const N: u32 = 5;
const T0: u32 = 6;
const T1: u32 = 7;
const COND: u32 = 8;
const TARGET: u32 = 9;
const FLAGS: u32 = 10;
/// DIV's operands (not COND or TARGET: a branch's, when it is in a delay
/// slot).
const DIV_S: u32 = 11;
const DIV_T: u32 = 12;
const I32_LOCALS: u32 = 9;
/// v128 locals: operands, temporaries, the upper result to commit.
const VS: u32 = 13;
const VT: u32 = 14;
const VX: u32 = 15;
const VY: u32 = 16;
const VZ: u32 = 17;
const VUP: u32 = 18;
const V128_LOCALS: u32 = 6;

/// The helper's type index (see `VuBackend::helper_index`).
const HELPER_TYPE: u32 = 1;

/// Instructions compiled per program, at most (the rest is interpreted).
const MAX_INSTRUCTIONS: usize = 4096;

const SIGN: i32 = i32::MIN;
const EXP: i32 = 0x7F80_0000;
const MAX: i32 = 0x7F7F_FFFF;

/// A compiled program: its module, and the instructions it was compiled
/// from (PC, the 64-bit word), to check it against micro memory.
pub struct Compiled {
    pub module: Vec<u8>,
    pub words: Vec<(u32, u64)>,
    /// The PCs its blocks start at (where it can be entered, its jumps
    /// through registers included).
    pub starts: Vec<u32>,
}

fn field(offset: usize) -> u32 {
    offset as u32
}

fn vf(r: usize) -> u32 {
    field(offset_of!(Vu, vf)) + 16 * r as u32
}

fn vi(r: usize) -> u32 {
    field(offset_of!(Vu, vi)) + 2 * r as u32
}

/// What a lower instruction is to the program's flow.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    None,
    /// B, BAL: to a fixed target.
    Jump(u32),
    /// JR, JALR: to a register's address.
    Register,
    /// IBEQ...: to a fixed target, or on.
    Cond(u32),
}

/// The instruction at `pc`: (upper, lower).
fn word_at(micro: &[u8], pc: u32) -> u64 {
    let at = pc as usize * 8;
    u64::from_le_bytes(micro[at..at + 8].try_into().unwrap_or_default())
}

fn flow(upper: u32, lower: u32, pc: u32, wrap: u32) -> Flow {
    if upper & (1 << 31) != 0 {
        return Flow::None;
    }
    let imm11 = ((lower & 0x7FF) as i32) << 21 >> 21;
    let target = ((pc.wrapping_add(1) as i32).wrapping_add(imm11) as u32) & wrap;
    match lower >> 25 {
        0x20 | 0x21 => Flow::Jump(target),
        0x24 | 0x25 => Flow::Register,
        0x28 | 0x29 | 0x2C..=0x2F => Flow::Cond(target),
        _ => Flow::None,
    }
}

const E_BIT: u32 = 1 << 30;

/// A block: its first instruction, how many it runs, and where it goes.
struct Block {
    start: u32,
    len: u32,
    end: End,
}

enum End {
    /// To the block (or the interpreter) at a PC: static, or computed
    /// (TARGET), or the condition's (COND: target, else the fall-through).
    Goto(u32),
    Computed,
    Cond {
        target: u32,
        next: u32,
    },
    /// The program ends; pc after it.
    Done(u32),
    /// Stop before the instruction at this PC, for the interpreter.
    Stop(u32),
}

/// Finds the blocks reachable from `start`, and from `entries` (PCs that
/// jumps through registers were found going to).
fn blocks(micro: &[u8], start: u32, entries: &[u32], wrap: u32) -> Vec<Block> {
    let mut out = Vec::new();
    let mut seen = BTreeMap::new();
    let mut todo: VecDeque<u32> = std::iter::once(start).chain(entries.iter().map(|&e| e & wrap)).collect();
    let mut total = 0;
    while let Some(first) = todo.pop_front() {
        if seen.contains_key(&first) || total >= MAX_INSTRUCTIONS {
            continue;
        }
        seen.insert(first, out.len());
        let mut pc = first;
        let mut len = 0;
        let end = loop {
            if total + len as usize >= MAX_INSTRUCTIONS {
                break End::Stop(pc);
            }
            let w = word_at(micro, pc);
            let (upper, lower) = ((w >> 32) as u32, w as u32);
            let next = (pc + 1) & wrap;
            let w2 = word_at(micro, next);
            let (upper2, lower2) = ((w2 >> 32) as u32, w2 as u32);
            let f = flow(upper, lower, pc, wrap);
            let f2 = flow(upper2, lower2, next, wrap);
            if upper & E_BIT != 0 {
                // This one and the next, then the end: unless a branch is
                // involved (the interpreter's).
                if f != Flow::None || f2 != Flow::None {
                    break End::Stop(pc);
                }
                len += 2;
                break End::Done((next + 1) & wrap);
            }
            if f != Flow::None {
                // The branch and its delay slot (not a branch, no E bit).
                if f2 != Flow::None || upper2 & E_BIT != 0 {
                    break End::Stop(pc);
                }
                len += 2;
                let after = (next + 1) & wrap;
                if (lower >> 25) & 1 == 1 && matches!(f, Flow::Jump(_) | Flow::Register) {
                    // BAL, JALR: where the subroutine returns.
                    todo.push_back(after);
                }
                break match f {
                    Flow::Jump(t) => {
                        todo.push_back(t);
                        End::Goto(t)
                    }
                    Flow::Register => End::Computed,
                    Flow::Cond(t) => {
                        todo.push_back(t);
                        todo.push_back(after);
                        End::Cond { target: t, next: after }
                    }
                    Flow::None => unreachable!(),
                };
            }
            len += 1;
            pc = next;
            // A long straight run goes on in another block.
            if len >= 256 {
                todo.push_back(pc);
                break End::Goto(pc);
            }
        };
        total += len as usize;
        out.push(Block { start: first, len, end });
    }
    out
}

/// Compiles the program at `start` (in 64-bit units) of a VU's micro memory,
/// for a data memory of `qwords` quadwords (1024 for VU1, 256 for VU0).
/// `helper`: the function-table index of `helpers::lower`; `entries`: more
/// PCs to start blocks at (see `blocks`).
pub fn compile(micro: &[u8], start: u32, entries: &[u32], qwords: u32, helper: u32) -> Option<Compiled> {
    let wrap = (micro.len() / 8).max(1) as u32 - 1;
    let start = start & wrap;
    let blocks = blocks(micro, start, entries, wrap);
    if blocks.is_empty() {
        return None;
    }
    let mut e = Emitter {
        code: Code::default(),
        vu: VU,
        helper,
        wrap,
        qmask: qwords.max(1) - 1,
    };
    let mut words = Vec::new();
    let n = blocks.len() as u32;
    let index: BTreeMap<u32, u32> = blocks.iter().enumerate().map(|(k, b)| (b.start, k as u32)).collect();

    // PC = start, N = 0; the dispatch loop.
    e.code.i32_const(start as i32).local_set(PC);
    e.code.i32_const(0).local_set(N);
    e.code.op(LOOP).op(0x40);
    e.code.block(); // the stop
    for _ in 0..n {
        e.code.block();
    }
    // The table: a block's index, else the stop.
    e.code.local_get(PC).op(BR_TABLE);
    uleb(&mut e.code.bytes, u64::from(wrap) + 1);
    for pc in 0..=wrap {
        uleb(&mut e.code.bytes, u64::from(index.get(&pc).copied().unwrap_or(n)));
    }
    uleb(&mut e.code.bytes, u64::from(n));
    for (k, b) in blocks.iter().enumerate() {
        e.code.op(op::END);
        let k = k as u32;
        // From here: depth 0 is the next block's label, n-1-k the stop,
        // n-k the dispatch loop.
        let (stop, dispatch) = (n - 1 - k, n - k);
        // Too many instructions for the program's budget: the
        // interpreter's (which ends it where it should).
        e.code
            .local_get(N)
            .i32_const((MAX_STEPS as u32).saturating_sub(b.len) as i32)
            .op(op::I32_GT_U);
        e.code
            .if_()
            .i32_const(b.start as i32)
            .local_set(PC)
            .br(stop + 1)
            .op(op::END);
        let mut pc = b.start;
        let stop_at = match b.end {
            End::Stop(at) => Some(at),
            _ => None,
        };
        for _ in 0..b.len {
            let w = word_at(micro, pc);
            words.push((pc, w));
            e.instruction(pc, w);
            pc = (pc + 1) & wrap;
        }
        match b.end {
            End::Goto(t) => {
                e.code.i32_const(t as i32).local_set(PC).br(dispatch);
            }
            End::Computed => {
                e.code
                    .local_get(TARGET)
                    .i32_const(wrap as i32)
                    .op(op::I32_AND)
                    .local_set(PC)
                    .br(dispatch);
            }
            End::Cond { target, next } => {
                e.code
                    .i32_const(target as i32)
                    .i32_const(next as i32)
                    .local_get(COND)
                    .op(op::SELECT);
                e.code.local_set(PC).br(dispatch);
            }
            End::Done(after) => {
                e.store_i32(field(offset_of!(Vu, pc)), |c| {
                    c.i32_const(after as i32);
                });
                e.store_i32(field(offset_of!(Vu, run_n)), |c| {
                    c.local_get(N);
                });
                e.code.i32_const(0).op(op::RETURN);
            }
            End::Stop(_) => {
                e.code.i32_const(stop_at.unwrap_or(pc) as i32).local_set(PC).br(stop);
            }
        }
    }
    e.code.op(op::END);
    // The stop: the interpreter goes on from PC.
    e.store_i32(field(offset_of!(Vu, pc)), |c| {
        c.local_get(PC);
    });
    e.store_i32(field(offset_of!(Vu, run_n)), |c| {
        c.local_get(N);
    });
    e.code.i32_const(1).op(op::RETURN);
    e.code.op(op::END); // the loop
    e.code.i32_const(1).op(op::END); // unreachable: the loop never falls through

    let types = [
        FuncType {
            params: &[I32, I32, I32, I32],
            results: &[I32],
        },
        FuncType {
            params: &[I32, I32, I32, I32, I32],
            results: &[],
        },
    ];
    let module = wasm::module(&types, &[(I32_LOCALS, I32), (V128_LOCALS, V128)], &e.code);
    let starts = blocks.iter().map(|b| b.start).collect();
    Some(Compiled { module, words, starts })
}

/// The locals `macro_upper`'s code uses, past the function's first three
/// (the EE block's parameters): i32s up to index 12 (`vu` among them, at
/// most 12, and T0, T1 at 6 and 7 as scratch), then the v128s at 13 to 18.
pub(crate) const MACRO_I32_LOCALS: u32 = 10;
pub(crate) const MACRO_V128_LOCALS: u32 = V128_LOCALS;
pub(crate) const V128_TYPE: u8 = V128;
const _: () = assert!(VS == 3 + MACRO_I32_LOCALS && FLAGS < VS && T0 > 5 && T1 > 5);

/// Compiles VU0's upper instruction `instr` as the EE runs it in macro mode
/// (COP2): its result committed at once, with its flags, as `Vu::macro_upper`
/// does; on the `Vu` whose address is in local `vu`. False (and nothing
/// emitted) if it is not one compiled.
pub(crate) fn macro_upper(code: &mut Code, instr: u32, vu: u32) -> bool {
    let funct = instr & 0x3F;
    let compiled = match funct {
        0x00..=0x2F => true,
        // The special table's upper instructions, not its NOP or the
        // undefined one.
        0x3C..=0x3F => {
            let op = (funct & 3) | ((instr >> 4) & 0x7C);
            op < 0x30 && op != 0x2B && op != 0x2F
        }
        _ => false,
    };
    if !compiled {
        return false;
    }
    let mut e = Emitter {
        code: std::mem::take(code),
        vu,
        helper: 0,
        wrap: 0,
        qmask: 0,
    };
    if let Some((reg, mask, flags)) = e.upper(instr) {
        e.commit(reg, mask, flags);
    }
    *code = e.code;
    true
}

struct Emitter {
    code: Code,
    /// The local holding the `Vu`'s address.
    vu: u32,
    helper: u32,
    wrap: u32,
    /// Data addresses wrap at the data memory's size (in quadwords).
    qmask: u32,
}

/// How an upper instruction's result is computed.
#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    /// ACC + a*b, ACC - a*b.
    Madd,
    Msub,
}

/// Its second operand: ft, a lane of ft, Q, I.
#[derive(Clone, Copy)]
enum Src {
    T,
    Lane(u32),
    Q,
    I,
}

impl Emitter {
    fn simd(&mut self, opcode: u32) -> &mut Self {
        self.code.op(0xFD);
        uleb(&mut self.code.bytes, u64::from(opcode));
        self
    }

    /// A SIMD load or store at a static offset from the address on the stack.
    fn simd_mem(&mut self, opcode: u32, offset: u32) {
        self.simd(opcode);
        uleb(&mut self.code.bytes, 2);
        uleb(&mut self.code.bytes, u64::from(offset));
    }

    fn v128_const(&mut self, lanes: [i32; 4]) {
        self.simd(simd::CONST);
        for l in lanes {
            self.code.bytes.extend_from_slice(&l.to_le_bytes());
        }
    }

    fn splat_i32(&mut self, v: i32) {
        self.v128_const([v; 4]);
    }

    /// A shuffle of the 32-bit lanes of two vectors (0-3 the first's, 4-7
    /// the second's).
    fn shuffle(&mut self, lanes: [u8; 4]) {
        self.simd(simd::SHUFFLE);
        for l in lanes {
            for b in 0..4 {
                self.code.bytes.push(l * 4 + b);
            }
        }
    }

    fn lane_op(&mut self, opcode: u32, lane: u32) {
        self.simd(opcode);
        self.code.bytes.push(lane as u8);
    }

    fn load_vu_v128(&mut self, offset: u32) {
        self.code.local_get(self.vu);
        self.simd_mem(simd::LOAD, offset);
    }

    fn load_vu_i32(&mut self, offset: u32) {
        self.code.local_get(self.vu).mem(op::I32_LOAD, 2, offset);
    }

    fn store_i32(&mut self, offset: u32, value: impl FnOnce(&mut Code)) {
        self.code.local_get(self.vu);
        value(&mut self.code);
        self.code.mem(op::I32_STORE, 2, offset);
    }

    fn load_vi(&mut self, r: usize) {
        self.code.local_get(self.vu).mem(op::I32_LOAD16_U, 1, vi(r));
    }

    fn load_vi_signed(&mut self, r: usize) {
        self.code.local_get(self.vu).mem(op::I32_LOAD16_S, 1, vi(r));
    }

    /// VI r = the i32 `value` pushes (its low 16 bits); VI0 stays 0.
    fn set_vi(&mut self, r: usize, value: impl FnOnce(&mut Self)) {
        if r == 0 {
            // Still evaluated for its effects? None have any: skip it.
            return;
        }
        self.code.local_get(self.vu);
        value(self);
        self.code.mem(op::I32_STORE16, 1, vi(r));
    }

    /// The vector on the stack, the PS2's way: the largest exponent is
    /// ±max, denormals are ±0 (`val`, and `res`'s value).
    fn sanitize(&mut self) {
        self.code.local_set(VX);
        // sign
        self.code.local_get(VX);
        self.splat_i32(SIGN);
        self.simd(simd::AND);
        self.code.local_tee(VY);
        self.splat_i32(MAX);
        self.simd(simd::OR);
        self.code.local_get(VX);
        self.code.local_get(VX);
        self.splat_i32(EXP);
        self.simd(simd::AND);
        self.code.local_tee(VZ);
        self.splat_i32(EXP);
        self.simd(simd::I32X4_EQ);
        self.simd(simd::BITSELECT);
        self.code.local_set(VX);
        self.code.local_get(VY);
        self.code.local_get(VX);
        self.code.local_get(VZ);
        self.splat_i32(0);
        self.simd(simd::I32X4_EQ);
        self.simd(simd::BITSELECT);
    }

    /// The flags of the raw result in VX (before `sanitize`): MAC's 16
    /// bits (zero, sign, underflow, overflow; x at the top of each group)
    /// into FLAGS.
    fn mac_flags(&mut self) {
        // Lanes reversed (w z y x): bitmask's bit 3 is x, as MAC has it.
        self.code.local_get(VX).local_get(VX);
        self.shuffle([3, 2, 1, 0]);
        self.code.local_tee(VY);
        // sign
        self.simd(simd::I32X4_BITMASK);
        self.code.i32_const(4).op(op::I32_SHL);
        // zero: exponent 0
        self.code.local_get(VY);
        self.splat_i32(EXP);
        self.simd(simd::AND);
        self.code.local_tee(VZ);
        self.splat_i32(0);
        self.simd(simd::I32X4_EQ);
        self.code.local_tee(VX);
        self.simd(simd::I32X4_BITMASK);
        self.code.op(op::I32_OR);
        // underflow: exponent 0, mantissa not
        self.code.local_get(VX);
        self.code.local_get(VY);
        self.splat_i32(0x007F_FFFF);
        self.simd(simd::AND);
        self.splat_i32(0);
        self.simd(simd::I32X4_NE);
        self.simd(simd::AND);
        self.simd(simd::I32X4_BITMASK);
        self.code.i32_const(8).op(op::I32_SHL).op(op::I32_OR);
        // overflow: the largest exponent
        self.code.local_get(VZ);
        self.splat_i32(EXP);
        self.simd(simd::I32X4_EQ);
        self.simd(simd::I32X4_BITMASK);
        self.code.i32_const(12).op(op::I32_SHL).op(op::I32_OR);
        self.code.local_set(FLAGS);
    }

    /// Pushes a VF register, sanitized.
    fn operand_vf(&mut self, r: usize) {
        self.load_vu_v128(vf(r));
        self.sanitize();
    }

    /// Pushes a 32-bit register (Q, I) on four lanes, sanitized.
    fn operand_scalar(&mut self, offset: u32) {
        self.load_vu_i32(offset);
        self.simd(simd::I32X4_SPLAT);
        self.sanitize();
    }

    fn operand(&mut self, src: Src) {
        match src {
            Src::T => self.code.local_get(VT),
            Src::Lane(k) => {
                self.code.local_get(VT);
                self.lane_op(simd::F32X4_EXTRACT, k);
                self.simd(simd::F32X4_SPLAT);
                &mut self.code
            }
            Src::Q => {
                self.operand_scalar(field(offset_of!(Vu, q)));
                &mut self.code
            }
            Src::I => {
                self.operand_scalar(field(offset_of!(Vu, i)));
                &mut self.code
            }
        };
    }

    /// An arithmetic upper instruction: its result into VUP (sanitized) and
    /// its flags into FLAGS.
    fn arith(&mut self, op: Op, src: Src) {
        match op {
            Op::Add | Op::Sub | Op::Mul => {
                self.code.local_get(VS);
                self.operand(src);
                self.simd(match op {
                    Op::Add => simd::F32X4_ADD,
                    Op::Sub => simd::F32X4_SUB,
                    _ => simd::F32X4_MUL,
                });
            }
            Op::Madd | Op::Msub => {
                self.load_vu_v128(field(offset_of!(Vu, acc)));
                self.sanitize();
                self.code.local_get(VS);
                self.operand(src);
                self.simd(simd::F32X4_MUL);
                self.simd(if matches!(op, Op::Madd) {
                    simd::F32X4_ADD
                } else {
                    simd::F32X4_SUB
                });
            }
        }
        self.result_with_flags();
    }

    /// The raw result on the stack: its flags into FLAGS, its value
    /// (sanitized) into VUP.
    fn result_with_flags(&mut self) {
        self.code.local_tee(VUP);
        self.code.local_set(VX);
        self.mac_flags();
        self.code.local_get(VUP);
        self.sanitize();
        self.code.local_set(VUP);
    }

    /// A lane mask (all ones in the lanes `mask` selects: x is bit 3).
    fn lane_mask(&mut self, mask: u32) {
        let l = |k: u32| if mask & (8 >> k) != 0 { -1 } else { 0 };
        self.v128_const([l(0), l(1), l(2), l(3)]);
    }

    /// Stores the vector `value` pushes into the VU's vector at `offset`,
    /// in the lanes `mask` selects.
    fn store_masked(&mut self, offset: u32, mask: u32, value: impl FnOnce(&mut Self)) {
        if mask == 0 {
            return;
        }
        self.code.local_get(self.vu);
        value(self);
        if mask != 0xF {
            self.load_vu_v128(offset);
            self.lane_mask(mask);
            self.simd(simd::BITSELECT);
        }
        self.simd_mem(simd::STORE, offset);
    }

    /// Commits the upper result in VUP: to a VF register (None: ACC),
    /// with its flags (from FLAGS) when it has them.
    fn commit(&mut self, reg: Option<usize>, mask: u32, flags: bool) {
        match reg {
            Some(0) => {}
            Some(r) => self.store_masked(vf(r), mask, |e| {
                e.code.local_get(VUP);
            }),
            None => self.store_masked(field(offset_of!(Vu, acc)), mask, |e| {
                e.code.local_get(VUP);
            }),
        }
        if flags {
            // MAC: the lanes written only; status: its four bits, sticky
            // ones too.
            self.code
                .local_get(FLAGS)
                .i32_const((mask * 0x1111) as i32)
                .op(op::I32_AND)
                .local_set(T0);
            self.store_i32(field(offset_of!(Vu, mac)), |c| {
                c.local_get(T0);
            });
            // now = any zero | any sign << 1 | any underflow << 2 | any overflow << 3
            self.code
                .local_get(T0)
                .i32_const(0xF)
                .op(op::I32_AND)
                .i32_const(0)
                .op(op::I32_NE);
            for (k, bits) in [(1, 0xF0), (2, 0xF00), (3, 0xF000)] {
                self.code
                    .local_get(T0)
                    .i32_const(bits)
                    .op(op::I32_AND)
                    .i32_const(0)
                    .op(op::I32_NE);
                self.code.i32_const(k).op(op::I32_SHL).op(op::I32_OR);
            }
            self.code.local_set(T1);
            let vu = self.vu;
            self.store_i32(field(offset_of!(Vu, status)), |c| {
                c.local_get(vu).mem(op::I32_LOAD, 2, field(offset_of!(Vu, status)));
                c.i32_const(0xFF0).op(op::I32_AND).local_get(T1).op(op::I32_OR);
                c.local_get(T1).i32_const(6).op(op::I32_SHL).op(op::I32_OR);
            });
        }
    }

    /// One instruction pair, at `pc`.
    fn instruction(&mut self, pc: u32, word: u64) {
        let upper = (word >> 32) as u32;
        let lower = word as u32;
        // Q's latency.
        self.tick_q();
        // The upper instruction's result (committed after the lower one).
        let commit = self.upper(upper);
        if upper & (1 << 31) != 0 {
            // I bit: the lower word is I's new value.
            self.store_i32(field(offset_of!(Vu, i)), |c| {
                c.i32_const(lower as i32);
            });
        } else {
            self.lower(lower, pc);
        }
        if let Some((reg, mask, flags)) = commit {
            self.commit(reg, mask, flags);
        }
        // The flags, for the flag instructions four later: slot n % 4.
        let size = std::mem::size_of::<Flags>() as i32;
        self.code
            .local_get(N)
            .i32_const(3)
            .op(op::I32_AND)
            .i32_const(size)
            .op(op::I32_MUL);
        self.code.local_get(self.vu).op(op::I32_ADD).local_set(T0);
        let history = field(offset_of!(Vu, flag_history));
        for (f, from) in [
            (offset_of!(Flags, mac), offset_of!(Vu, mac)),
            (offset_of!(Flags, status), offset_of!(Vu, status)),
            (offset_of!(Flags, clip), offset_of!(Vu, clip)),
        ] {
            self.code.local_get(T0);
            self.load_vu_i32(field(from));
            self.code.mem(op::I32_STORE, 2, history + field(f));
        }
        self.code.local_get(N).i32_const(1).op(op::I32_ADD).local_set(N);
    }

    fn pending(&self) -> (u32, u32) {
        (
            field(offset_of!(Vu, pending_q) + offset_of!(PendingQ, value)),
            field(offset_of!(Vu, pending_q) + offset_of!(PendingQ, left)),
        )
    }

    /// Q's pending value comes one instruction closer (lands at 1).
    fn tick_q(&mut self) {
        let (value, left) = self.pending();
        let vu = self.vu;
        self.load_vu_i32(left);
        self.code.local_tee(T0).if_();
        self.code.local_get(T0).i32_const(1).op(op::I32_EQ).if_();
        self.store_i32(field(offset_of!(Vu, q)), |c| {
            c.local_get(vu).mem(op::I32_LOAD, 2, value);
        });
        self.store_i32(left, |c| {
            c.i32_const(0);
        });
        self.code.op(op::ELSE);
        self.store_i32(left, |c| {
            c.local_get(T0).i32_const(1).op(op::I32_SUB);
        });
        self.code.op(op::END).op(op::END);
    }

    /// WAITQ: Q's pending value lands now.
    fn wait_q(&mut self) {
        let (value, left) = self.pending();
        let vu = self.vu;
        self.load_vu_i32(left);
        self.code.if_();
        self.store_i32(field(offset_of!(Vu, q)), |c| {
            c.local_get(vu).mem(op::I32_LOAD, 2, value);
        });
        self.store_i32(left, |c| {
            c.i32_const(0);
        });
        self.code.op(op::END);
    }

    /// The upper instruction: its result computed (into VUP, FLAGS), and
    /// how to commit it: (register, None for ACC; mask; with flags).
    fn upper(&mut self, instr: u32) -> Option<(Option<usize>, u32, bool)> {
        if instr & 0x7FF == 0x2FF {
            return None;
        }
        let mask = (instr >> 21) & 0xF;
        let ft = ((instr >> 16) & 31) as usize;
        let fs = ((instr >> 11) & 31) as usize;
        let fd = ((instr >> 6) & 31) as usize;
        let funct = instr & 0x3F;
        let bc = funct & 3;
        let (op_, src, to): (Op, Src, Option<usize>) = match funct {
            0x00..=0x03 => (Op::Add, Src::Lane(bc), Some(fd)),
            0x04..=0x07 => (Op::Sub, Src::Lane(bc), Some(fd)),
            0x08..=0x0B => (Op::Madd, Src::Lane(bc), Some(fd)),
            0x0C..=0x0F => (Op::Msub, Src::Lane(bc), Some(fd)),
            0x10..=0x13 => return self.minmax(fs, ft, Src::Lane(bc), fd, mask, true),
            0x14..=0x17 => return self.minmax(fs, ft, Src::Lane(bc), fd, mask, false),
            0x18..=0x1B => (Op::Mul, Src::Lane(bc), Some(fd)),
            0x1C => (Op::Mul, Src::Q, Some(fd)),
            0x1D => return self.minmax(fs, ft, Src::I, fd, mask, true),
            0x1E => (Op::Mul, Src::I, Some(fd)),
            0x1F => return self.minmax(fs, ft, Src::I, fd, mask, false),
            0x20 => (Op::Add, Src::Q, Some(fd)),
            0x21 => (Op::Madd, Src::Q, Some(fd)),
            0x22 => (Op::Add, Src::I, Some(fd)),
            0x23 => (Op::Madd, Src::I, Some(fd)),
            0x24 => (Op::Sub, Src::Q, Some(fd)),
            0x25 => (Op::Msub, Src::Q, Some(fd)),
            0x26 => (Op::Sub, Src::I, Some(fd)),
            0x27 => (Op::Msub, Src::I, Some(fd)),
            0x28 => (Op::Add, Src::T, Some(fd)),
            0x29 => (Op::Madd, Src::T, Some(fd)),
            0x2A => (Op::Mul, Src::T, Some(fd)),
            0x2B => return self.minmax(fs, ft, Src::T, fd, mask, true),
            0x2C => (Op::Sub, Src::T, Some(fd)),
            0x2D => (Op::Msub, Src::T, Some(fd)),
            0x2E => {
                self.cross(fs, ft, true);
                return Some((Some(fd), mask, true));
            }
            0x2F => return self.minmax(fs, ft, Src::T, fd, mask, false),
            0x3C..=0x3F => {
                let op = (funct & 3) | ((instr >> 4) & 0x7C);
                return self.upper_special(op, mask, ft, fs);
            }
            _ => return None,
        };
        self.operands(fs, ft);
        self.arith(op_, src);
        Some((to, mask, true))
    }

    /// VS and VT: fs and ft, sanitized.
    fn operands(&mut self, fs: usize, ft: usize) {
        self.operand_vf(fs);
        self.code.local_set(VS);
        self.operand_vf(ft);
        self.code.local_set(VT);
    }

    /// MAX, MINI: ties give the second operand (as the interpreter has it);
    /// no flags.
    fn minmax(
        &mut self,
        fs: usize,
        ft: usize,
        src: Src,
        fd: usize,
        mask: u32,
        max: bool,
    ) -> Option<(Option<usize>, u32, bool)> {
        self.operands(fs, ft);
        self.operand(src);
        self.code.local_set(VZ);
        // s where s > t (max) or s < t (min), else t.
        self.code.local_get(VS).local_get(VZ).local_get(VS).local_get(VZ);
        self.simd(if max { simd::F32X4_GT } else { simd::F32X4_LT });
        self.simd(simd::BITSELECT);
        self.code.local_set(VUP);
        Some((Some(fd), mask, false))
    }

    /// OPMSUB (ACC - cross) and OPMULA (the cross product): fs.yzx * ft.zxy,
    /// w 0.
    fn cross(&mut self, fs: usize, ft: usize, sub: bool) {
        self.operands(fs, ft);
        if sub {
            self.load_vu_v128(field(offset_of!(Vu, acc)));
            self.sanitize();
        }
        self.code.local_get(VS).local_get(VS);
        self.shuffle([1, 2, 0, 3]);
        self.code.local_get(VT).local_get(VT);
        self.shuffle([2, 0, 1, 3]);
        self.simd(simd::F32X4_MUL);
        self.code.op(op::I32_CONST);
        wasm::sleb(&mut self.code.bytes, 0);
        self.code.op(F32_REINTERPRET_I32);
        self.lane_op(simd::F32X4_REPLACE, 3);
        if sub {
            self.simd(simd::F32X4_SUB);
        }
        self.result_with_flags();
    }

    fn upper_special(&mut self, op: u32, mask: u32, ft: usize, fs: usize) -> Option<(Option<usize>, u32, bool)> {
        let bc = op & 3;
        let (op_, src) = match op {
            0x00..=0x03 => (Op::Add, Src::Lane(bc)),
            0x04..=0x07 => (Op::Sub, Src::Lane(bc)),
            0x08..=0x0B => (Op::Madd, Src::Lane(bc)),
            0x0C..=0x0F => (Op::Msub, Src::Lane(bc)),
            // ITOF0/4/12/15: fs's raw bits, as integers.
            0x10..=0x13 => {
                let scale = [1.0f32, 16.0, 4096.0, 32768.0][bc as usize];
                self.load_vu_v128(vf(fs));
                self.simd(simd::F32X4_CONVERT_I32X4_S);
                self.splat_i32(scale.to_bits() as i32);
                self.simd(simd::F32X4_DIV);
                self.sanitize();
                self.code.local_set(VUP);
                return Some((Some(ft), mask, false));
            }
            // FTOI0/4/12/15: truncating, saturating.
            0x14..=0x17 => {
                let scale = [1.0f32, 16.0, 4096.0, 32768.0][bc as usize];
                self.operand_vf(fs);
                self.splat_i32(scale.to_bits() as i32);
                self.simd(simd::F32X4_MUL);
                self.simd(simd::I32X4_TRUNC_SAT_F32X4_S);
                self.code.local_set(VUP);
                return Some((Some(ft), mask, false));
            }
            0x18..=0x1B => (Op::Mul, Src::Lane(bc)),
            0x1C => (Op::Mul, Src::Q),
            // ABS: fs's raw bits, without the sign.
            0x1D => {
                self.load_vu_v128(vf(fs));
                self.splat_i32(0x7FFF_FFFF);
                self.simd(simd::AND);
                self.code.local_set(VUP);
                return Some((Some(ft), mask, false));
            }
            0x1E => (Op::Mul, Src::I),
            0x1F => {
                self.clip(fs, ft);
                return None;
            }
            0x20 => (Op::Add, Src::Q),
            0x21 => (Op::Madd, Src::Q),
            0x22 => (Op::Add, Src::I),
            0x23 => (Op::Madd, Src::I),
            0x24 => (Op::Sub, Src::Q),
            0x25 => (Op::Msub, Src::Q),
            0x26 => (Op::Sub, Src::I),
            0x27 => (Op::Msub, Src::I),
            0x28 => (Op::Add, Src::T),
            0x29 => (Op::Madd, Src::T),
            0x2A => (Op::Mul, Src::T),
            0x2C => (Op::Sub, Src::T),
            0x2D => (Op::Msub, Src::T),
            0x2E => {
                self.cross(fs, ft, false);
                return Some((None, mask, true));
            }
            _ => return None,
        };
        self.operands(fs, ft);
        self.arith(op_, src);
        Some((None, mask, true))
    }

    /// CLIP: fs.xyz against ±|ft.w|; six new bits under the old ones, at
    /// once (the lower instruction sees them).
    fn clip(&mut self, fs: usize, ft: usize) {
        self.operands(fs, ft);
        // W = |ft.w| on four lanes.
        self.code.local_get(VT);
        self.lane_op(simd::F32X4_EXTRACT, 3);
        self.code.op(F32_ABS);
        self.simd(simd::F32X4_SPLAT);
        self.code.local_set(VZ);
        // g: x>w y>w z>w (bits 0-2), l: x<-w ... (bits 0-2)
        self.code.local_get(VS).local_get(VZ);
        self.simd(simd::F32X4_GT);
        self.simd(simd::I32X4_BITMASK);
        self.code.i32_const(7).op(op::I32_AND).local_set(T0);
        self.code.local_get(VS).local_get(VZ);
        self.simd(simd::F32X4_NEG);
        self.simd(simd::F32X4_LT);
        self.simd(simd::I32X4_BITMASK);
        self.code.i32_const(7).op(op::I32_AND).local_set(T1);
        // Interleaved: g0 l0 g1 l1 g2 l2.
        let clip = field(offset_of!(Vu, clip));
        let vu = self.vu;
        self.store_i32(clip, |c| {
            c.local_get(vu).mem(op::I32_LOAD, 2, clip).i32_const(6).op(op::I32_SHL);
            for (src, bit, shift) in [(T0, 1, 0), (T1, 1, 1), (T0, 2, 1), (T1, 2, 2), (T0, 4, 2), (T1, 4, 3)] {
                c.local_get(src)
                    .i32_const(bit)
                    .op(op::I32_AND)
                    .i32_const(shift)
                    .op(op::I32_SHL)
                    .op(op::I32_OR);
            }
            c.i32_const(0xFF_FFFF).op(op::I32_AND);
        });
    }

    // --- Lower instructions -------------------------------------------------------

    /// The address of a quadword of data memory, from the i32 (a quadword
    /// index) on the stack: wrapped at the memory's size.
    fn qaddr(&mut self) {
        self.code
            .i32_const(self.qmask as i32)
            .op(op::I32_AND)
            .i32_const(4)
            .op(op::I32_SHL);
        self.code.local_get(DATA).op(op::I32_ADD);
    }

    /// VI r + imm, as a data address.
    fn vi_plus(&mut self, r: usize, imm: i32) {
        self.load_vi(r);
        if imm != 0 {
            self.code.i32_const(imm).op(op::I32_ADD);
        }
        self.qaddr();
    }

    /// VF ft = the quadword at the address on the stack, in the lanes masked.
    fn load_q_to(&mut self, ft: usize, mask: u32) {
        self.code.local_set(T0);
        if ft == 0 {
            return;
        }
        self.store_masked(vf(ft), mask, |e| {
            e.code.local_get(T0);
            e.simd_mem(simd::LOAD, 0);
        });
    }

    /// The quadword at the address on the stack = the vector `value`
    /// pushes, in the lanes masked.
    fn store_q(&mut self, mask: u32, value: impl FnOnce(&mut Self)) {
        self.code.local_set(T0);
        if mask == 0 {
            return;
        }
        self.code.local_get(T0);
        value(self);
        if mask != 0xF {
            self.code.local_get(T0);
            self.simd_mem(simd::LOAD, 0);
            self.lane_mask(mask);
            self.simd(simd::BITSELECT);
        }
        self.simd_mem(simd::STORE, 0);
    }

    /// The first lane a mask selects (x first), or x.
    fn first_lane(mask: u32) -> u32 {
        (0..4).find(|k| mask & (8 >> k) != 0).unwrap_or(0)
    }

    fn lower(&mut self, instr: u32, pc: u32) {
        let op_ = instr >> 25;
        let mask = (instr >> 21) & 0xF;
        let it = ((instr >> 16) & 15) as usize;
        let is = ((instr >> 11) & 15) as usize;
        let imm11 = ((instr & 0x7FF) as i32) << 21 >> 21;
        let next = pc.wrapping_add(1) & self.wrap;
        match op_ {
            // LQ, SQ
            0x00 => {
                let ft = ((instr >> 16) & 31) as usize;
                self.vi_plus(is, imm11);
                self.load_q_to(ft, mask);
            }
            0x01 => {
                let fs = ((instr >> 11) & 31) as usize;
                self.vi_plus(it, imm11);
                self.store_q(mask, |e| e.load_vu_v128(vf(fs)));
            }
            // ILW: the lane's low halfword; ISW: VI it on the lanes.
            0x04 => {
                let k = Self::first_lane(mask);
                self.vi_plus(is, imm11);
                self.code.local_set(T0);
                self.set_vi(it, |e| {
                    e.code.local_get(T0).mem(op::I32_LOAD16_U, 1, 4 * k);
                });
            }
            0x05 => {
                self.vi_plus(is, imm11);
                self.store_q(mask, |e| {
                    e.load_vi(it);
                    e.simd(simd::I32X4_SPLAT);
                });
            }
            // IADDIU, ISUBIU
            0x08 | 0x09 => {
                let imm15 = ((instr & 0x7FF) | ((instr >> 10) & 0x7800)) as i32;
                self.set_vi(it, |e| {
                    e.load_vi(is);
                    e.code
                        .i32_const(imm15)
                        .op(if op_ == 0x08 { op::I32_ADD } else { op::I32_SUB });
                });
            }
            // Clip flag tests (on what this instruction sees), FCSET.
            0x10 => self.set_vi(1, |e| {
                e.seen(offset_of!(Flags, clip));
                e.code
                    .i32_const(0xFF_FFFF)
                    .op(op::I32_AND)
                    .i32_const((instr & 0xFF_FFFF) as i32)
                    .op(op::I32_EQ);
            }),
            0x11 => self.store_i32(field(offset_of!(Vu, clip)), |c| {
                c.i32_const((instr & 0xFF_FFFF) as i32);
            }),
            0x12 => self.set_vi(1, |e| {
                e.seen(offset_of!(Flags, clip));
                e.code
                    .i32_const((instr & 0xFF_FFFF) as i32)
                    .op(op::I32_AND)
                    .i32_const(0)
                    .op(op::I32_NE);
            }),
            0x13 => self.set_vi(1, |e| {
                e.seen(offset_of!(Flags, clip));
                e.code
                    .i32_const(instr as i32)
                    .op(op::I32_OR)
                    .i32_const(0xFF_FFFF)
                    .op(op::I32_AND);
                e.code.i32_const(0xFF_FFFF).op(op::I32_EQ);
            }),
            0x1C => self.set_vi(it, |e| {
                e.seen(offset_of!(Flags, clip));
                e.code.i32_const(0xFFF).op(op::I32_AND);
            }),
            // Status flag tests, FSSET.
            0x14..=0x17 => {
                let imm12 = ((instr & 0x7FF) | ((instr >> 10) & 0x800)) as i32;
                let st = |e: &mut Self| {
                    e.seen(offset_of!(Flags, status));
                    e.code.i32_const(0xFFF).op(op::I32_AND);
                };
                match op_ {
                    0x14 => self.set_vi(it, |e| {
                        st(e);
                        e.code.i32_const(imm12).op(op::I32_EQ);
                    }),
                    0x15 => {
                        let status = field(offset_of!(Vu, status));
                        let vu = self.vu;
                        self.store_i32(status, |c| {
                            c.local_get(vu)
                                .mem(op::I32_LOAD, 2, status)
                                .i32_const(0x3F)
                                .op(op::I32_AND);
                            c.i32_const(imm12 & 0xFC0).op(op::I32_OR);
                        });
                    }
                    0x16 => self.set_vi(it, |e| {
                        st(e);
                        e.code.i32_const(imm12).op(op::I32_AND).i32_const(0).op(op::I32_NE);
                    }),
                    _ => self.set_vi(it, |e| {
                        st(e);
                        e.code.i32_const(imm12).op(op::I32_OR);
                    }),
                }
            }
            // MAC flag tests.
            0x18 => self.set_vi(it, |e| {
                e.seen(offset_of!(Flags, mac));
                e.code.i32_const(0xFFFF).op(op::I32_AND);
                e.load_vi(is);
                e.code.op(op::I32_EQ);
            }),
            0x1A => self.set_vi(it, |e| {
                e.seen(offset_of!(Flags, mac));
                e.load_vi(is);
                e.code.op(op::I32_AND);
            }),
            0x1B => self.set_vi(it, |e| {
                e.seen(offset_of!(Flags, mac));
                e.load_vi(is);
                e.code.op(op::I32_OR);
            }),
            // Branches: their target or condition, for the block's end.
            0x20 => {}
            0x21 => self.set_vi(it, |e| {
                e.code.i32_const(next.wrapping_add(1) as i32);
            }),
            0x24 => {
                self.load_vi(is);
                self.code.local_set(TARGET);
            }
            0x25 => {
                self.load_vi(is);
                self.code.local_set(TARGET);
                self.set_vi(it, |e| {
                    e.code.i32_const(next.wrapping_add(1) as i32);
                });
            }
            0x28 | 0x29 => {
                self.load_vi_signed(it);
                self.load_vi_signed(is);
                self.code
                    .op(if op_ == 0x28 { op::I32_EQ } else { op::I32_NE })
                    .local_set(COND);
            }
            0x2C..=0x2F => {
                self.load_vi_signed(is);
                self.code.i32_const(0);
                self.code.op(match op_ {
                    0x2C => op::I32_LT_S,
                    0x2D => op::I32_GT_S,
                    0x2E => op::I32_LE_S,
                    _ => op::I32_GE_S,
                });
                self.code.local_set(COND);
            }
            0x40 => self.lower_special(instr, pc),
            _ => {}
        }
    }

    /// The flag word (at offset `f` in `Flags`) this instruction sees: slot
    /// n % 4 of the history.
    fn seen(&mut self, f: usize) {
        let size = std::mem::size_of::<Flags>() as i32;
        self.code
            .local_get(N)
            .i32_const(3)
            .op(op::I32_AND)
            .i32_const(size)
            .op(op::I32_MUL);
        self.code.local_get(self.vu).op(op::I32_ADD);
        self.code.mem(op::I32_LOAD, 2, field(offset_of!(Vu, flag_history) + f));
    }

    fn lower_special(&mut self, instr: u32, pc: u32) {
        let funct = instr & 0x3F;
        let it = ((instr >> 16) & 15) as usize;
        let is = ((instr >> 11) & 15) as usize;
        let id = ((instr >> 6) & 15) as usize;
        match funct {
            0x30 | 0x31 | 0x34 | 0x35 => self.set_vi(id, |e| {
                e.load_vi(is);
                e.load_vi(it);
                e.code.op(match funct {
                    0x30 => op::I32_ADD,
                    0x31 => op::I32_SUB,
                    0x34 => op::I32_AND,
                    _ => op::I32_OR,
                });
            }),
            0x32 => {
                let imm5 = ((((instr >> 6) & 0x1F) as i32) << 27) >> 27;
                self.set_vi(it, |e| {
                    e.load_vi(is);
                    e.code.i32_const(imm5).op(op::I32_ADD);
                });
            }
            0x3C..=0x3F => {
                let op = (funct & 3) | ((instr >> 4) & 0x7C);
                self.special2(op, instr, pc);
            }
            _ => {}
        }
    }

    fn special2(&mut self, op_: u32, instr: u32, pc: u32) {
        let mask = (instr >> 21) & 0xF;
        let ft = ((instr >> 16) & 31) as usize;
        let fs = ((instr >> 11) & 31) as usize;
        let it = ft & 15;
        let is = fs & 15;
        let fsf = (instr >> 21) & 3;
        let ftf = (instr >> 23) & 3;
        match op_ {
            // MOVE, MR32
            0x30 if ft != 0 => self.store_masked(vf(ft), mask, |e| e.load_vu_v128(vf(fs))),
            0x31 if ft != 0 => self.store_masked(vf(ft), mask, |e| {
                e.load_vu_v128(vf(fs));
                e.code.local_tee(VX).local_get(VX);
                e.shuffle([1, 2, 3, 0]);
            }),
            0x30 | 0x31 => {}
            // LQI, SQI, LQD, SQD
            0x34 => {
                self.load_vi(is);
                self.qaddr();
                self.load_q_to(ft, mask);
                self.inc_vi(is, 1);
            }
            0x35 => {
                self.load_vi(it);
                self.qaddr();
                self.store_q(mask, |e| e.load_vu_v128(vf(fs)));
                self.inc_vi(it, 1);
            }
            0x36 => {
                self.inc_vi(is, -1);
                self.load_vi(is);
                self.qaddr();
                self.load_q_to(ft, mask);
            }
            0x37 => {
                self.inc_vi(it, -1);
                self.load_vi(it);
                self.qaddr();
                self.store_q(mask, |e| e.load_vu_v128(vf(fs)));
            }
            0x38..=0x3A => self.divide(op_, fs, ft, fsf, ftf),
            0x3B => self.wait_q(),
            // MTIR: a lane's low 16 bits; MFIR: VI sign-extended.
            0x3C => self.set_vi(it, |e| {
                e.load_vu_i32(vf(fs) + 4 * fsf);
            }),
            0x3D if ft != 0 => self.store_masked(vf(ft), mask, |e| {
                e.load_vi_signed(is);
                e.simd(simd::I32X4_SPLAT);
            }),
            0x3D => {}
            // ILWR, ISWR
            0x3E => {
                let k = Self::first_lane(mask);
                self.load_vi(is);
                self.qaddr();
                self.code.local_set(T0);
                self.set_vi(it, |e| {
                    e.code.local_get(T0).mem(op::I32_LOAD16_U, 1, 4 * k);
                });
            }
            0x3F => {
                self.load_vi(is);
                self.qaddr();
                self.store_q(mask, |e| {
                    e.load_vi(it);
                    e.simd(simd::I32X4_SPLAT);
                });
            }
            // MFP, XTOP, XITOP
            0x64 if ft != 0 => self.store_masked(vf(ft), mask, |e| {
                e.load_vu_i32(field(offset_of!(Vu, p)));
                e.simd(simd::I32X4_SPLAT);
            }),
            0x68 => self.set_vi(it, |e| {
                e.code
                    .local_get(e.vu)
                    .mem(op::I32_LOAD16_U, 1, field(offset_of!(Vu, top)));
            }),
            0x69 => self.set_vi(it, |e| {
                e.code
                    .local_get(e.vu)
                    .mem(op::I32_LOAD16_U, 1, field(offset_of!(Vu, itop)));
            }),
            // The random numbers, XGKICK, the EFU: the interpreter's.
            0x40..=0x43 | 0x6C | 0x70..=0x7E => self.helper(instr, pc),
            _ => {}
        }
    }

    /// VI r += delta (VI0 stays 0).
    fn inc_vi(&mut self, r: usize, delta: i32) {
        self.set_vi(r, |e| {
            e.load_vi(r);
            e.code.i32_const(delta).op(op::I32_ADD);
        });
    }

    /// The interpreter runs the lower instruction (its pc as it has it).
    fn helper(&mut self, instr: u32, pc: u32) {
        let next = pc.wrapping_add(1) & self.wrap;
        self.code.local_get(self.vu).local_get(GIF).local_get(GS);
        self.code.i32_const(instr as i32).i32_const(next as i32);
        self.code.i32_const(self.helper as i32).call_indirect(HELPER_TYPE);
    }

    /// The scalar (i32 bits) on the stack, sanitized.
    fn sanitize_scalar(&mut self) {
        self.code.local_tee(T0).i32_const(EXP).op(op::I32_AND).local_set(T1);
        // exponent max: sign | max
        self.code
            .local_get(T0)
            .i32_const(SIGN)
            .op(op::I32_AND)
            .i32_const(MAX)
            .op(op::I32_OR);
        // exponent 0: sign
        self.code.local_get(T0).i32_const(SIGN).op(op::I32_AND);
        self.code.local_get(T0);
        self.code.local_get(T1).op(op::I32_EQZ).op(op::SELECT);
        self.code.local_get(T1).i32_const(EXP).op(op::I32_EQ).op(op::SELECT);
    }

    /// DIV, SQRT, RSQRT: Q's next value, after its latency (7, 7, 13).
    fn divide(&mut self, op_: u32, fs: usize, ft: usize, fsf: u32, ftf: u32) {
        self.wait_q();
        let s_at = vf(fs) + 4 * fsf;
        let t_at = vf(ft) + 4 * ftf;
        let (value, left) = self.pending();
        // s, t sanitized, as f32 bits.
        self.load_vu_i32(s_at);
        self.sanitize_scalar();
        self.code.local_set(DIV_S);
        self.load_vu_i32(t_at);
        self.sanitize_scalar();
        self.code.local_set(DIV_T);
        let f32_of = |c: &mut Code, l: u32| {
            c.local_get(l).op(F32_REINTERPRET_I32);
        };
        match op_ {
            0x38 => {
                // t == 0: ±max, with s and t's raw signs; else s / t.
                self.code.local_get(self.vu);
                self.load_vu_i32(s_at);
                self.load_vu_i32(t_at);
                self.code
                    .op(op::I32_XOR)
                    .i32_const(SIGN)
                    .op(op::I32_AND)
                    .i32_const(MAX)
                    .op(op::I32_OR);
                f32_of(&mut self.code, DIV_S);
                f32_of(&mut self.code, DIV_T);
                self.code.op(F32_DIV).op(I32_REINTERPRET_F32);
                self.sanitize_scalar();
                f32_of(&mut self.code, DIV_T);
                self.code.i32_const(0).op(F32_REINTERPRET_I32).op(F32_EQ);
                self.code.op(op::SELECT);
                self.code.mem(op::I32_STORE, 2, value);
            }
            0x39 => {
                self.code.local_get(self.vu);
                f32_of(&mut self.code, DIV_T);
                self.code.op(F32_ABS).op(F32_SQRT).op(I32_REINTERPRET_F32);
                self.sanitize_scalar();
                self.code.mem(op::I32_STORE, 2, value);
            }
            _ => {
                // r = sqrt(|t|): r == 0 gives max; else s / r.
                f32_of(&mut self.code, DIV_T);
                self.code
                    .op(F32_ABS)
                    .op(F32_SQRT)
                    .op(I32_REINTERPRET_F32)
                    .local_set(DIV_T);
                self.code.local_get(self.vu);
                self.code.i32_const(MAX);
                f32_of(&mut self.code, DIV_S);
                f32_of(&mut self.code, DIV_T);
                self.code.op(F32_DIV).op(I32_REINTERPRET_F32);
                self.sanitize_scalar();
                f32_of(&mut self.code, DIV_T);
                self.code.i32_const(0).op(F32_REINTERPRET_I32).op(F32_EQ);
                self.code.op(op::SELECT);
                self.code.mem(op::I32_STORE, 2, value);
            }
        }
        let latency = if op_ == 0x3A { 13 } else { 7 };
        self.store_i32(left, |c| {
            c.i32_const(latency);
        });
    }
}
