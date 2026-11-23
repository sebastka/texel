//! Compiles a block of EE code into a WebAssembly function:
//! `(ee: i32, bus: i32, left: i32) -> i32`, which runs the block on the
//! `Ee` and `Bus` at those addresses (of the emulator's own memory), for at
//! most `left` instructions, and returns how many it ran.
//!
//! A block runs from its first instruction to a branch and its delay slot,
//! or to an instruction after which the interpreter must look again (COP0,
//! SYSCALL, BREAK), or to the end of its 4 KiB page, or `MAX_LEN`
//! instructions. The integer instructions, the branches, the moves to and
//! from the FPU and VU0, and VU0's upper instructions in macro mode (with
//! the VU compiler's code) are compiled; the others call `exec` (the
//! interpreter, for that one instruction), and the block stops early when
//! one of those takes an exception or changes where the EE goes next.

use std::mem::offset_of;

use super::super::Ee;
use super::EXEC_TYPE;
use super::wasm::{self, Code, FuncType, I32, op};
use crate::vu_jit::{MACRO_I32_LOCALS, MACRO_V128_LOCALS, V128_TYPE, macro_upper};

/// Instructions in a block, at most (its delay slot not counted).
pub const MAX_LEN: usize = 64;

/// Float opcodes the shared encoder does not name.
const F32_CONST: u8 = 0x43;
const F32_EQ: u8 = 0x5B;
const F32_LT: u8 = 0x5D;
const F32_LE: u8 = 0x5F;
const F32_ABS: u8 = 0x8B;
const F32_SQRT: u8 = 0x91;
const F32_ADD: u8 = 0x92;
const F32_SUB: u8 = 0x93;
const F32_MUL: u8 = 0x94;
const F32_DIV: u8 = 0x95;
const F32_CONVERT_I32_S: u8 = 0xB2;
const I32_REINTERPRET_F32: u8 = 0xBC;
const F32_REINTERPRET_I32: u8 = 0xBE;

/// SIMD opcodes (after the 0xFD prefix).
mod simd {
    pub const LOAD: u32 = 0x00;
    pub const STORE: u32 = 0x0B;
    pub const SHUFFLE: u32 = 0x0D;
    pub const I8X16_EQ: u32 = 0x23;
    pub const I8X16_GT_S: u32 = 0x27;
    pub const I16X8_EQ: u32 = 0x2D;
    pub const I16X8_GT_S: u32 = 0x31;
    pub const I32X4_EQ: u32 = 0x37;
    pub const I32X4_GT_S: u32 = 0x3B;
    pub const NOT: u32 = 0x4D;
    pub const AND: u32 = 0x4E;
    pub const OR: u32 = 0x50;
    pub const XOR: u32 = 0x51;
    pub const I8X16_ADD: u32 = 0x6E;
    pub const I8X16_ADD_SAT_S: u32 = 0x6F;
    pub const I8X16_ADD_SAT_U: u32 = 0x70;
    pub const I8X16_SUB: u32 = 0x71;
    pub const I8X16_SUB_SAT_S: u32 = 0x72;
    pub const I8X16_SUB_SAT_U: u32 = 0x73;
    pub const I16X8_SHL: u32 = 0x8B;
    pub const I16X8_SHR_S: u32 = 0x8C;
    pub const I16X8_SHR_U: u32 = 0x8D;
    pub const I16X8_ADD: u32 = 0x8E;
    pub const I16X8_ADD_SAT_S: u32 = 0x8F;
    pub const I16X8_ADD_SAT_U: u32 = 0x90;
    pub const I16X8_SUB: u32 = 0x91;
    pub const I16X8_SUB_SAT_S: u32 = 0x92;
    pub const I16X8_SUB_SAT_U: u32 = 0x93;
    pub const I16X8_MIN_S: u32 = 0x96;
    pub const I16X8_MAX_S: u32 = 0x98;
    pub const I32X4_SHL: u32 = 0xAB;
    pub const I32X4_SHR_S: u32 = 0xAC;
    pub const I32X4_SHR_U: u32 = 0xAD;
    pub const I32X4_ADD: u32 = 0xAE;
    pub const I32X4_SUB: u32 = 0xB1;
    pub const I32X4_MIN_S: u32 = 0xB6;
    pub const I32X4_MAX_S: u32 = 0xB8;
}

/// An `i8x16.shuffle` of rt (first) and rs: lane k of the result (of
/// `size` bytes) is lane `lane` of rt (`src` 0) or rs (1).
fn lanes<const N: usize>(size: usize, map: [(u8, u8); N]) -> [u8; 16] {
    lanes_of(size, &map)
}

/// PEXTL*, PEXTU*: the lower (or upper) lanes of rt and rs, alternating,
/// rt's first.
fn interleave(size: usize, upper: bool) -> [u8; 16] {
    let n = 16 / size;
    let base = if upper { n / 2 } else { 0 };
    let map: Vec<(u8, u8)> = (0..n).map(|k| ((k % 2) as u8, (base + k / 2) as u8)).collect();
    lanes_of(size, &map)
}

/// PPAC*: rt's even lanes, then rs's.
fn pack(size: usize) -> [u8; 16] {
    let n = 16 / size;
    let map: Vec<(u8, u8)> = (0..n)
        .map(|k| if k < n / 2 { (0, (2 * k) as u8) } else { (1, (2 * (k - n / 2)) as u8) })
        .collect();
    lanes_of(size, &map)
}

fn lanes_of(size: usize, map: &[(u8, u8)]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (k, &(src, lane)) in map.iter().enumerate() {
        for b in 0..size {
            out[k * size + b] = src * 16 + lane * size as u8 + b as u8;
        }
    }
    out
}

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
/// VU0's address, for its macro instructions (whose code uses locals up to
/// 18: see `vu_jit::macro_upper`).
const VU0: u32 = 9;
/// An f32: the FPU's divisor (after the VU's locals, see `Gen::locals`).
const COND_F: u32 = 19;
const F32: u8 = 0x7D;
const _: () = assert!(COND_F == 3 + MACRO_I32_LOCALS + MACRO_V128_LOCALS);

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

/// FPU register `f`, in the `Ee`.
fn fpr(f: u32) -> u32 {
    (offset_of!(Ee, fpu) + offset_of!(super::super::fpu::Fpu, fpr) + 4 * f as usize) as u32
}

/// VU0's VF register `v`, in the `Bus`.
fn vf(v: u32) -> u32 {
    (offset_of!(crate::bus::Bus, vu0) + offset_of!(crate::vu::Vu, vf) + 16 * v as usize) as u32
}

/// The register a load or store reads or writes: the EE's own, the FPU's
/// (LWC1, SWC1), VU0's (LQC2, SQC2).
#[derive(Clone, Copy)]
enum Reg {
    Gpr(u32),
    Fpr(u32),
    Vf(u32),
}

/// Emits the compiled block.
struct Gen {
    code: Code,
    exec: u32,
    layout: Layout,
    /// Instructions compiled (not left to the interpreter).
    natives: u32,
    /// Whether it has VU0 macro or FPU instructions (which need their
    /// locals).
    vu0: bool,
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

    /// The bits on the stack (i32) as the PS2 has them: the largest
    /// exponent is ±max, denormals are ±0 (`fpu::value`, and `fpu::result`
    /// on a result's bits).
    fn ps2_float(&mut self) {
        self.code
            .local_tee(TMP)
            .i32_const(0x7F80_0000)
            .op(op::I32_AND)
            .local_set(PA);
        // ±max
        self.code
            .local_get(TMP)
            .i32_const(i32::MIN)
            .op(op::I32_AND)
            .i32_const(0x7F7F_FFFF)
            .op(op::I32_OR);
        // ±0 if the exponent is 0, else the bits
        self.code
            .local_get(TMP)
            .i32_const(i32::MIN)
            .op(op::I32_AND)
            .local_get(TMP)
            .local_get(PA)
            .op(op::I32_EQZ)
            .op(op::SELECT);
        self.code
            .local_get(PA)
            .i32_const(0x7F80_0000)
            .op(op::I32_EQ)
            .op(op::SELECT);
    }

    /// Pushes an FPU register (or ACC, at `offset`) as an f32, the PS2's way.
    fn fpu_operand(&mut self, offset: u32) {
        self.code.local_get(EE).mem(op::I32_LOAD, 2, offset);
        self.ps2_float();
        self.code.op(F32_REINTERPRET_I32);
    }

    /// Stores the f32 on the stack, clamped the PS2's way, at `offset`.
    fn fpu_result(&mut self, offset: u32) {
        self.code.op(I32_REINTERPRET_F32);
        self.ps2_float();
        self.code.local_set(VA);
        self.code.local_get(EE).local_get(VA).mem(op::I32_STORE, 2, offset);
    }

    /// COP1's single-precision instructions, as `Ee::fpu_single` does them
    /// (its f32 arithmetic is these WebAssembly instructions); false for
    /// MAX and MIN (whose ±0 Rust's may order otherwise) and the rest.
    fn fpu_single(&mut self, i: u32) -> bool {
        let ft = (i >> 16) & 31;
        let fs = (i >> 11) & 31;
        let fd = (i >> 6) & 31;
        let acc = (offset_of!(Ee, fpu) + offset_of!(super::super::fpu::Fpu, acc)) as u32;
        let fcr31 = (offset_of!(Ee, fpu) + offset_of!(super::super::fpu::Fpu, fcr31)) as u32;
        const C: i32 = 1 << 23;
        let funct = i & 0x3F;
        match funct {
            // ADD, SUB, MUL; ADDA, SUBA, MULA.
            0x00..=0x02 | 0x18..=0x1A => {
                self.fpu_operand(fpr(fs));
                self.fpu_operand(fpr(ft));
                self.code.op([F32_ADD, F32_SUB, F32_MUL][(funct & 3) as usize]);
                self.fpu_result(if funct < 0x18 { fpr(fd) } else { acc });
            }
            // DIV: by zero, ±max with the operands' signs.
            0x03 => {
                self.fpu_operand(fpr(ft));
                self.code.local_set(COND_F);
                self.code.local_get(COND_F).op(F32_CONST).bytes.extend_from_slice(&0f32.to_le_bytes());
                self.code.op(F32_EQ).if_();
                self.code
                    .local_get(EE)
                    .local_get(EE)
                    .mem(op::I32_LOAD, 2, fpr(fs))
                    .local_get(EE)
                    .mem(op::I32_LOAD, 2, fpr(ft))
                    .op(op::I32_XOR)
                    .i32_const(i32::MIN)
                    .op(op::I32_AND)
                    .i32_const(0x7F7F_FFFF)
                    .op(op::I32_OR)
                    .mem(op::I32_STORE, 2, fpr(fd));
                self.code.op(op::ELSE);
                self.fpu_operand(fpr(fs));
                self.code.local_get(COND_F).op(F32_DIV);
                self.fpu_result(fpr(fd));
                self.code.op(op::END);
            }
            // SQRT: of |ft|.
            0x04 => {
                self.fpu_operand(fpr(ft));
                self.code.op(F32_ABS).op(F32_SQRT);
                self.fpu_result(fpr(fd));
            }
            // ABS, MOV, NEG: the bits.
            0x05..=0x07 => {
                self.code.local_get(EE).local_get(EE).mem(op::I32_LOAD, 2, fpr(fs));
                match funct {
                    0x05 => {
                        self.code.i32_const(0x7FFF_FFFF).op(op::I32_AND);
                    }
                    0x07 => {
                        self.code.i32_const(i32::MIN).op(op::I32_XOR);
                    }
                    _ => {}
                }
                self.code.mem(op::I32_STORE, 2, fpr(fd));
            }
            // RSQRT: fs / sqrt(|ft|), max if that is 0.
            0x16 => {
                self.fpu_operand(fpr(ft));
                self.code.op(F32_ABS).op(F32_SQRT).local_set(COND_F);
                self.code.local_get(COND_F).op(F32_CONST).bytes.extend_from_slice(&0f32.to_le_bytes());
                self.code.op(F32_EQ).if_();
                self.code
                    .local_get(EE)
                    .i32_const(0x7F7F_FFFF)
                    .mem(op::I32_STORE, 2, fpr(fd));
                self.code.op(op::ELSE);
                self.fpu_operand(fpr(fs));
                self.code.local_get(COND_F).op(F32_DIV);
                self.fpu_result(fpr(fd));
                self.code.op(op::END);
            }
            // MADD, MSUB (to fd); MADDA, MSUBA (to ACC): ACC ± fs×ft.
            0x1C..=0x1F => {
                self.fpu_operand(acc);
                self.fpu_operand(fpr(fs));
                self.fpu_operand(fpr(ft));
                self.code.op(F32_MUL);
                self.code.op(if funct & 1 == 0 { F32_ADD } else { F32_SUB });
                self.fpu_result(if funct < 0x1E { fpr(fd) } else { acc });
            }
            // CVT.W.S: truncating, saturating.
            0x24 => {
                self.code.local_get(EE);
                self.fpu_operand(fpr(fs));
                self.code.op(0xFC).op(0x00); // i32.trunc_sat_f32_s
                self.code.mem(op::I32_STORE, 2, fpr(fd));
            }
            // C.F; C.EQ, C.LT, C.LE.
            0x30 | 0x32 | 0x34 | 0x36 => {
                self.code.local_get(EE).local_get(EE).mem(op::I32_LOAD, 2, fcr31);
                self.code.i32_const(!C).op(op::I32_AND);
                if funct != 0x30 {
                    self.fpu_operand(fpr(fs));
                    self.fpu_operand(fpr(ft));
                    self.code.op(match funct {
                        0x32 => F32_EQ,
                        0x34 => F32_LT,
                        _ => F32_LE,
                    });
                    self.code.i32_const(23).op(op::I32_SHL).op(op::I32_OR);
                }
                self.code.mem(op::I32_STORE, 2, fcr31);
            }
            _ => return false,
        }
        true
    }

    /// CVT.S.W: fs's integer, as a float.
    fn cvt_s_w(&mut self, i: u32) {
        let fs = (i >> 11) & 31;
        let fd = (i >> 6) & 31;
        self.code.local_get(EE).mem(op::I32_LOAD, 2, fpr(fs)).op(F32_CONVERT_I32_S);
        self.fpu_result(fpr(fd));
    }

    /// A SIMD instruction (after the 0xFD prefix).
    fn simd(&mut self, opcode: u32) {
        self.code.op(0xFD);
        wasm::uleb(&mut self.code.bytes, u64::from(opcode));
    }

    /// Pushes register `r`'s 128 bits.
    fn get128(&mut self, r: u32) {
        self.code.local_get(EE);
        self.simd(simd::LOAD);
        wasm::uleb(&mut self.code.bytes, 4);
        wasm::uleb(&mut self.code.bytes, u64::from(gpr(r)));
    }

    /// Compiles the MMI instructions with an exact SIMD equivalent (lanes
    /// added, compared, interleaved, packed, shifted...); false for the
    /// others (the 32-bit saturating ones, PABS, HI and LO's).
    fn mmi(&mut self, i: u32) -> bool {
        let rs = (i >> 21) & 31;
        let rt = (i >> 16) & 31;
        let rd = (i >> 11) & 31;
        let sa = (i >> 6) & 31;
        // What rd gets: from rs and rt (an operation, or a shuffle of rt's
        // and rs's lanes), or rt shifted by sa.
        enum Op {
            Binary(u32),
            Nor,
            Shuffle([u8; 16]),
            Shift(u32),
        }
        use Op::{Binary, Shift, Shuffle};
        let op = match (i & 0x3F, sa) {
            (0x08, 0x00) => Binary(simd::I32X4_ADD),
            (0x08, 0x01) => Binary(simd::I32X4_SUB),
            (0x08, 0x02) => Binary(simd::I32X4_GT_S),
            (0x08, 0x03) => Binary(simd::I32X4_MAX_S),
            (0x08, 0x04) => Binary(simd::I16X8_ADD),
            (0x08, 0x05) => Binary(simd::I16X8_SUB),
            (0x08, 0x06) => Binary(simd::I16X8_GT_S),
            (0x08, 0x07) => Binary(simd::I16X8_MAX_S),
            (0x08, 0x08) => Binary(simd::I8X16_ADD),
            (0x08, 0x09) => Binary(simd::I8X16_SUB),
            (0x08, 0x0A) => Binary(simd::I8X16_GT_S),
            (0x08, 0x12) => Shuffle(interleave(4, false)),
            (0x08, 0x13) => Shuffle(pack(4)),
            (0x08, 0x14) => Binary(simd::I16X8_ADD_SAT_S),
            (0x08, 0x15) => Binary(simd::I16X8_SUB_SAT_S),
            (0x08, 0x16) => Shuffle(interleave(2, false)),
            (0x08, 0x17) => Shuffle(pack(2)),
            (0x08, 0x18) => Binary(simd::I8X16_ADD_SAT_S),
            (0x08, 0x19) => Binary(simd::I8X16_SUB_SAT_S),
            (0x08, 0x1A) => Shuffle(interleave(1, false)),
            (0x08, 0x1B) => Shuffle(pack(1)),
            (0x28, 0x02) => Binary(simd::I32X4_EQ),
            (0x28, 0x03) => Binary(simd::I32X4_MIN_S),
            (0x28, 0x06) => Binary(simd::I16X8_EQ),
            (0x28, 0x07) => Binary(simd::I16X8_MIN_S),
            (0x28, 0x0A) => Binary(simd::I8X16_EQ),
            (0x28, 0x12) => Shuffle(interleave(4, true)),
            (0x28, 0x14) => Binary(simd::I16X8_ADD_SAT_U),
            (0x28, 0x15) => Binary(simd::I16X8_SUB_SAT_U),
            (0x28, 0x16) => Shuffle(interleave(2, true)),
            (0x28, 0x18) => Binary(simd::I8X16_ADD_SAT_U),
            (0x28, 0x19) => Binary(simd::I8X16_SUB_SAT_U),
            (0x28, 0x1A) => Shuffle(interleave(1, true)),
            // PCPYLD: rt's lower doubleword, then rs's.
            (0x09, 0x0E) => Shuffle(lanes(8, [(0, 0), (1, 0)])),
            (0x09, 0x12) => Binary(simd::AND),
            (0x09, 0x13) => Binary(simd::XOR),
            // PCPYUD: rs's upper doubleword, then rt's.
            (0x29, 0x0E) => Shuffle(lanes(8, [(1, 1), (0, 1)])),
            (0x29, 0x12) => Binary(simd::OR),
            (0x29, 0x13) => Op::Nor,
            (0x34, _) => Shift(simd::I16X8_SHL),
            (0x36, _) => Shift(simd::I16X8_SHR_U),
            (0x37, _) => Shift(simd::I16X8_SHR_S),
            (0x3C, _) => Shift(simd::I32X4_SHL),
            (0x3E, _) => Shift(simd::I32X4_SHR_U),
            (0x3F, _) => Shift(simd::I32X4_SHR_S),
            _ => return false,
        };
        if rd == 0 {
            return true;
        }
        self.code.local_get(EE);
        match op {
            Binary(o) => {
                self.get128(rs);
                self.get128(rt);
                self.simd(o);
            }
            Op::Nor => {
                self.get128(rs);
                self.get128(rt);
                self.simd(simd::OR);
                self.simd(simd::NOT);
            }
            Shuffle(bytes) => {
                self.get128(rt);
                self.get128(rs);
                self.simd(simd::SHUFFLE);
                self.code.bytes.extend_from_slice(&bytes);
            }
            Shift(o) => {
                self.get128(rt);
                self.code.i32_const(sa as i32);
                self.simd(o);
            }
        }
        self.simd(simd::STORE);
        wasm::uleb(&mut self.code.bytes, 4);
        wasm::uleb(&mut self.code.bytes, u64::from(gpr(rd)));
        true
    }

    /// Compiles `i` if it is an integer instruction, else runs it through
    /// the interpreter.
    fn instruction(&mut self, i: u32, pc: u32, index: u32, slot: bool) {
        if self.memory(i, pc, index, slot) || self.native(i) || (i >> 26 == 0x1C && self.mmi(i)) {
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
            // LWC1, SWC1, LQC2, SQC2.
            0x31 => (4, op::I32_LOAD, 0),
            0x39 => (4, 0, op::I32_STORE),
            0x36 => (16, op::I64_LOAD, 0),
            0x3E => (16, 0, op::I64_STORE),
            _ => return false,
        };
        let reg = match i >> 26 {
            0x31 | 0x39 => Reg::Fpr(rt),
            0x36 | 0x3E => Reg::Vf(rt),
            _ => Reg::Gpr(rt),
        };
        let write = store != 0;
        // r0 and VF0 keep their values: the interpreter loads into them.
        if !write && rt == 0 && !matches!(reg, Reg::Fpr(_)) {
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
        self.access(write, size, load, store, reg, self.layout.ram, !(size as i32 - 1));
        self.code.op(op::ELSE);
        self.code
            .local_get(PA)
            .i32_const(!(SCRATCHPAD_SIZE - 1))
            .op(op::I32_AND)
            .i32_const(SCRATCHPAD as i32)
            .op(op::I32_EQ)
            .if_();
        let mask = (SCRATCHPAD_SIZE - 1) & !(size as i32 - 1);
        self.access(write, size, load, store, reg, self.layout.scratchpad, mask);
        self.code.op(op::ELSE);
        self.exec(i, pc, index, slot);
        self.code.op(op::END);
        self.code.op(op::END);
        true
    }

    /// The access itself, at `base` + (PA & `mask`), of `size` bytes with
    /// the wasm `load` or `store`: a register's (a 16-byte access in two
    /// halves).
    #[allow(clippy::too_many_arguments)]
    fn access(&mut self, write: bool, size: u32, load: u8, store: u8, reg: Reg, base: u32, mask: i32) {
        let align = size.min(8).trailing_zeros();
        let halves: &[u32] = if size == 16 { &[0, 8] } else { &[0] };
        for &h in halves {
            if write {
                self.code.local_get(PA).i32_const(mask).op(op::I32_AND);
                match reg {
                    Reg::Gpr(r) if h == 0 => self.get(r),
                    Reg::Gpr(r) => self.get_high(r),
                    Reg::Fpr(f) => {
                        self.code.local_get(EE).mem(op::I32_LOAD, 2, fpr(f));
                    }
                    Reg::Vf(v) => {
                        self.code.local_get(BUS).mem(op::I64_LOAD, 3, vf(v) + h);
                    }
                }
                self.code.mem(store, align, base + h);
            } else {
                let (local, at, reg_store, reg_align) = match reg {
                    Reg::Gpr(r) => (EE, gpr(r), op::I64_STORE, 3),
                    Reg::Fpr(f) => (EE, fpr(f), op::I32_STORE, 2),
                    Reg::Vf(v) => (BUS, vf(v), op::I64_STORE, 3),
                };
                self.code.local_get(local);
                self.code.local_get(PA).i32_const(mask).op(op::I32_AND);
                self.code.mem(load, align, base + h);
                self.code.mem(reg_store, reg_align, at + h);
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
            0x11 if rs == 0x10 => {
                self.vu0 = true;
                return self.fpu_single(i);
            }
            0x11 if rs == 0x14 && i & 0x3F == 0x20 => {
                self.vu0 = true;
                self.cvt_s_w(i);
            }
            // MFC1, MTC1.
            0x11 if rs == 0x00 => self.set(rt, |g| {
                g.code.local_get(EE).mem(op::I64_LOAD32_S, 2, fpr(rd));
            }),
            0x11 if rs == 0x04 => {
                self.code.local_get(EE);
                self.get(rt);
                self.code.op(op::I32_WRAP_I64).mem(op::I32_STORE, 2, fpr(rd));
            }
            // QMFC2, QMTC2 (VF0 keeps its value).
            0x12 if rs == 0x01 => {
                if rt != 0 {
                    for h in [0, 8] {
                        self.code
                            .local_get(EE)
                            .local_get(BUS)
                            .mem(op::I64_LOAD, 3, vf(rd) + h)
                            .mem(op::I64_STORE, 3, gpr(rt) + h);
                    }
                }
            }
            // VU0's upper instructions in macro mode.
            0x12 if rs & 0x10 != 0 => {
                let mut c = Code::default();
                if !macro_upper(&mut c, i, VU0) {
                    return false;
                }
                self.code
                    .local_get(BUS)
                    .i32_const((offset_of!(crate::bus::Bus, vu0)) as i32)
                    .op(op::I32_ADD)
                    .local_set(VU0);
                self.code.bytes.extend_from_slice(&c.bytes);
                self.vu0 = true;
            }
            0x12 if rs == 0x05 => {
                if rd != 0 {
                    self.code.local_get(BUS);
                    self.get(rt);
                    self.code.mem(op::I64_STORE, 3, vf(rd));
                    self.code.local_get(BUS);
                    self.get_high(rt);
                    self.code.mem(op::I64_STORE, 3, vf(rd) + 8);
                }
            }
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
    /// The instructions it must have left to start: its first, with its
    /// delay slot when it is a branch (it can stop after any other).
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
/// not (a superblock); jumps end it. Each instruction after the first
/// starts by leaving the block if none are left to run (two for a branch,
/// which runs with its delay slot): the slices of time the EE runs in are
/// short (`Ps2::run`), and the interpreter takes the rest of one where a
/// block stops, as it would have. Every way out stores pc and next_pc and
/// returns how many instructions ran.
///
/// `exec`: the function-table index of the interpreter's entry point (see
/// `Backend::exec_index`); `layout`: where the EE's memory is.
pub fn compile(src: &Source, exec: u32, layout: Layout) -> Option<Compiled> {
    let mut g = Gen {
        code: Code::default(),
        exec,
        layout,
        natives: 0,
        vu0: false,
    };
    // k: the next word; ran: the instructions run so far on the path
    // compiled (a likely branch not taken skips its delay slot).
    let mut k = 0;
    let mut ran = 0u32;
    let mut ended = false;
    // To start: the first instruction, with its delay slot if a branch.
    let is_branch = |k: usize| {
        let pc = src.va.wrapping_add(4 * k as u32);
        matches!(branch(src.words[k], pc, &src.idle), Ok(Some(_)))
    };
    let first = if is_branch(0) { 2 } else { 1 }.min(segment(src, 0));
    while k < src.words.len() && k < MAX_LEN {
        let i = src.words[k];
        let pc = src.va.wrapping_add(4 * k as u32);
        if k > 0 {
            // Out here unless this instruction (and its delay slot) fits.
            let need = if is_branch(k) { 2 } else { 1 };
            g.code
                .local_get(LEFT)
                .i32_const(ran as i32 + need)
                .op(op::I32_LT_U)
                .if_();
            g.set_pc(pc);
            g.code.i32_const(ran as i32).op(op::RETURN).op(op::END);
        }
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
        &if g.vu0 {
            vec![(MACRO_I32_LOCALS, I32), (MACRO_V128_LOCALS, V128_TYPE), (1, F32)]
        } else {
            vec![(6, I32)]
        },
        &g.code,
    );
    Some(Compiled {
        module,
        first: first as u32,
        words: k as u32,
        native: g.natives,
    })
}
