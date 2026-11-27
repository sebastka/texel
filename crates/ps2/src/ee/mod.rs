//! The Emotion Engine's CPU core, the R5900: a MIPS III CPU (with parts
//! of MIPS IV) with 128-bit registers and multimedia instructions (MMI),
//! COP0 (exceptions, interrupts, timer), COP1 (the FPU) and COP2 (VU0, in
//! macro mode: register transfers only so far).
//!
//! An interpreter, one instruction at a time, one cycle each; in browsers,
//! blocks of code are also compiled to WebAssembly (`jit`).

mod cop0;
mod fpu;
pub mod jit;
mod mmi;
mod tlb;

use std::collections::BTreeMap;

use crate::bus::Bus;

pub use cop0::Cop0;
use tlb::Fault;

/// Exception codes (Cause.ExcCode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    Interrupt = 0,
    Modified = 1,
    TlbLoad = 2,
    TlbStore = 3,
    Syscall = 8,
    Break = 9,
    Reserved = 10,
    Overflow = 12,
    Trap = 13,
}

pub const RESET_VECTOR: u32 = 0xBFC0_0000;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Ee {
    pub gpr: [u128; 32],
    /// HI and LO: the lower 64 bits for MULT/DIV, the upper for MULT1/DIV1.
    pub hi: u128,
    pub lo: u128,
    /// The funnel shift amount, in bytes (QFSRV).
    pub sa: u32,
    pub pc: u32,
    next_pc: u32,
    /// The address of the instruction being executed.
    current_pc: u32,
    /// The next instruction is in a branch delay slot.
    delay_pending: bool,
    /// The instruction being executed is in a delay slot.
    in_delay: bool,
    pub cop0: Cop0,
    pub fpu: fpu::Fpu,
    pub cycles: u64,
    /// The last INTC_STAT read: the instruction and the value; and whether
    /// the EE is found polling it (see `note_poll`).
    poll: (u32, u32),
    pub(crate) polling: bool,
    /// A DI ran: Status.EIE is cleared after the next instruction (the
    /// DI's write lands an instruction late, so that instruction still
    /// reads EIE set: the kernel's return to a new program reads Status
    /// right after a DI and writes it back, leaving interrupts enabled,
    /// which Jak X's start-up code requires).
    #[serde(default)]
    pub(crate) pending_di: bool,
    /// Diagnostics: an instruction to stop at, and the registers the first
    /// time the interpreter reached it.
    #[serde(skip)]
    pub break_at: Option<u32>,
    /// ...when this register's low word holds this value, if set.
    #[serde(skip)]
    pub break_if: Option<(usize, u32)>,
    #[serde(skip)]
    pub break_regs: Option<Box<[u128; 32]>>,
    /// Diagnostics, when on: at this instruction, how many times each
    /// (ra, a0, a1, a2, a3) came by.
    #[serde(skip)]
    pub arg_log: Option<(u32, Box<ArgLog>)>,
    /// Diagnostics, when on: how many times the interpreter ran each
    /// instruction, by address.
    #[serde(skip)]
    pub pc_counts: Option<Box<std::collections::HashMap<u32, u64>>>,
    /// Diagnostics, when on: each change of Status.EIE by a COP0
    /// instruction (MTC0, EI, DI): the instruction's address, the new
    /// Status.
    #[serde(skip)]
    pub eie_log: Option<Vec<(u32, u32)>>,
    /// An interrupt waits for the next read of INTC_STAT: the EE stops
    /// right after it (as when polling), and the interrupt is taken there.
    #[serde(skip)]
    pub(crate) intc_watch: bool,
    /// Compiles code to WebAssembly (in browsers), when set.
    #[serde(skip)]
    pub jit: Option<Box<jit::Jit>>,
    /// Instructions of the running compiled block already counted.
    counted: u32,
    /// Cycles not yet counted in COP0's Count: it is ticked in bulk, before
    /// anything reads COP0 (its instructions, interrupts).
    unticked: u32,
    /// An idle loop found: NOPs, and an unconditional branch back to them
    /// (first and last address, the delay slot included). Running it
    /// changes nothing but time.
    pub idle: Option<(u32, u32)>,
    /// After a fast boot: the system configuration (language, screen) the
    /// browser would have given the kernel (`SetOsdConfigParam`), which a
    /// fast boot skips; the game's `GetOsdConfigParam` gets it rather than
    /// the kernel's zeros (Japanese). Saved after the machine (see
    /// `Ps2::save_core`).
    #[serde(skip)]
    pub osd_config: Option<u32>,
    /// Instructions not emulated (executed as no-ops): name → count.
    #[serde(skip)]
    pub unimplemented: BTreeMap<&'static str, u64>,
    /// The last exceptions, for diagnostics: (code, EPC, v1, a0), at most
    /// `TRACE_LEN`; syscalls carry their number in v1.
    #[serde(skip)]
    pub trace: std::collections::VecDeque<(u32, u32, u32, u32)>,
    /// Diagnostics, when on: every syscall by number (v1) and address:
    /// how many, the last a0, the cycle of the last, the last a1.
    #[serde(skip)]
    pub syscalls: Option<SyscallLog>,
    /// Diagnostics, when on: an address, and its 32-bit loads by EE
    /// instruction: how many, the last value, the values' OR.
    #[serde(skip)]
    pub read_watch: Option<ReadWatch>,
}

/// See `Ee::read_watch`.
pub type ReadWatch = (u32, BTreeMap<u32, (u64, u32, u32)>);

/// See `Ee::syscalls`.
pub type SyscallLog = BTreeMap<(u32, u32), (u64, u32, u64, u32)>;

const TRACE_LEN: usize = 64;

impl Default for Ee {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn sext32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

#[inline]
fn sext16(v: u16) -> u64 {
    v as i16 as i64 as u64
}

/// See `Ee::arg_log`: (ra, a0, a1, a2, a3), and how many times.
pub type ArgLog = std::collections::BTreeMap<[u32; 5], u64>;

impl Ee {
    pub fn new() -> Self {
        Self {
            gpr: [0; 32],
            hi: 0,
            lo: 0,
            sa: 0,
            pc: RESET_VECTOR,
            next_pc: RESET_VECTOR + 4,
            current_pc: RESET_VECTOR,
            break_at: None,
            break_if: None,
            break_regs: None,
            arg_log: None,
            pc_counts: None,
            eie_log: None,
            delay_pending: false,
            pending_di: false,
            in_delay: false,
            cop0: Cop0::new(),
            fpu: fpu::Fpu::default(),
            cycles: 0,
            unticked: 0,
            jit: None,
            counted: 0,
            poll: (0, 0),
            polling: false,
            intc_watch: false,
            idle: None,
            osd_config: None,
            unimplemented: BTreeMap::new(),
            trace: std::collections::VecDeque::new(),
            syscalls: None,
            read_watch: None,
        }
    }

    // --- Registers -------------------------------------------------------------

    #[inline]
    fn r64(&self, r: u32) -> u64 {
        self.gpr[r as usize] as u64
    }

    #[inline]
    fn r32(&self, r: u32) -> u32 {
        self.gpr[r as usize] as u32
    }

    /// Writes the lower 64 bits; the upper 64 are kept.
    #[inline]
    fn set64(&mut self, r: u32, v: u64) {
        if r != 0 {
            let g = &mut self.gpr[r as usize];
            *g = (*g & !u128::from(u64::MAX)) | u128::from(v);
        }
    }

    #[inline]
    fn set128(&mut self, r: u32, v: u128) {
        if r != 0 {
            self.gpr[r as usize] = v;
        }
    }

    fn lo64(&self) -> u64 {
        self.lo as u64
    }
    fn hi64(&self) -> u64 {
        self.hi as u64
    }
    fn set_lo64(&mut self, v: u64) {
        self.lo = (self.lo & !u128::from(u64::MAX)) | u128::from(v);
    }
    fn set_hi64(&mut self, v: u64) {
        self.hi = (self.hi & !u128::from(u64::MAX)) | u128::from(v);
    }
    fn set_lo1(&mut self, v: u64) {
        self.lo = (self.lo & u128::from(u64::MAX)) | (u128::from(v) << 64);
    }
    fn set_hi1(&mut self, v: u64) {
        self.hi = (self.hi & u128::from(u64::MAX)) | (u128::from(v) << 64);
    }

    fn unimplemented(&mut self, name: &'static str) {
        *self.unimplemented.entry(name).or_insert(0) += 1;
    }

    // --- Control flow ----------------------------------------------------------

    fn branch(&mut self, target: u32) {
        self.next_pc = target;
        self.delay_pending = true;
    }

    /// A conditional branch: `likely` branches skip their delay slot when
    /// not taken.
    fn cond_branch(&mut self, taken: bool, offset: u32, likely: bool) {
        let target = self.pc.wrapping_add(offset << 2);
        if taken {
            self.branch(target);
        } else if likely {
            self.pc = self.next_pc;
            self.next_pc = self.pc.wrapping_add(4);
        }
    }

    /// Takes an exception (level 1): to the common vector, or the
    /// interrupt one.
    pub fn exception(&mut self, e: Exception) {
        self.exception_at(e, if e == Exception::Interrupt { 0x200 } else { 0x180 });
    }

    /// Takes an exception to the vector at `offset` from the base.
    fn exception_at(&mut self, e: Exception, offset: u32) {
        if self.trace.len() == TRACE_LEN {
            self.trace.pop_front();
        }
        self.trace
            .push_back((e as u32, self.current_pc, self.r32(3), self.r32(4)));
        if e == Exception::Syscall {
            let (n, a0, a1, pc, now) = (self.r32(3), self.r32(4), self.r32(5), self.current_pc, self.cycles);
            if let Some(log) = &mut self.syscalls {
                let entry = log.entry((n, pc)).or_default();
                *entry = (entry.0 + 1, a0, now, a1);
            }
        }
        let c = &mut self.cop0;
        if !c.exl() {
            let epc = if self.in_delay {
                self.current_pc.wrapping_sub(4)
            } else {
                self.current_pc
            };
            c.regs[cop0::EPC] = epc;
            c.set_bd(self.in_delay);
        }
        c.regs[cop0::CAUSE] = (c.regs[cop0::CAUSE] & !0x7C) | ((e as u32) << 2);
        c.regs[cop0::STATUS] |= cop0::STATUS_EXL;
        let base = if c.regs[cop0::STATUS] & cop0::STATUS_BEV != 0 {
            0xBFC0_0200
        } else {
            0x8000_0000
        };
        let vector = base + offset;
        self.pc = vector;
        self.next_pc = vector + 4;
        self.delay_pending = false;
    }

    /// Updates the interrupt lines and takes an interrupt if one is due.
    pub fn check_interrupts(&mut self, int0: bool, int1: bool) {
        self.flush_ticks();
        self.cop0.set_ip(2, int0);
        self.cop0.set_ip(3, int1);
        if self.cop0.interrupt_pending() {
            // The interrupted instruction runs again on return.
            self.current_pc = self.pc;
            self.in_delay = self.delay_pending;
            if self.in_delay {
                // pc is the delay slot; EPC must be the branch.
                self.current_pc = self.pc;
            }
            self.exception(Exception::Interrupt);
        }
    }

    // --- Execution --------------------------------------------------------------

    /// Whether the EE spins in its idle loop: then only time passes.
    pub fn idling(&self) -> bool {
        matches!(self.idle, Some((a, b)) if self.pc >= a && self.pc <= b && !self.delay_pending && !self.pending_di)
    }

    /// Time passes in the idle loop, without running it.
    pub fn skip(&mut self, cycles: u64) {
        self.cycles += cycles;
        self.unticked += cycles as u32;
    }

    /// Counts the cycles run so far in COP0's Count (and its timer
    /// interrupt).
    #[inline]
    pub fn flush_ticks(&mut self) {
        if self.unticked != 0 {
            self.cop0.tick(self.unticked);
            self.unticked = 0;
        }
    }

    /// `beq $0, $0` backwards, at `current_pc`: an idle loop if everything
    /// from its target to its delay slot is a NOP.
    fn detect_idle(&mut self, offset: u32, bus: &mut Bus) {
        let target = self.pc.wrapping_add(offset << 2);
        let slot = self.pc;
        let mut a = target;
        while a <= slot {
            match self.cop0.translate(a, false) {
                Ok(pa) if a == self.current_pc || bus.read32(pa) == 0 => {}
                _ => return,
            }
            a += 4;
        }
        self.idle = Some((target, slot));
    }

    /// Executes one instruction.
    pub fn step(&mut self, bus: &mut Bus) {
        self.current_pc = self.pc;
        if self.break_at == Some(self.pc)
            && self.break_regs.is_none()
            && self.break_if.is_none_or(|(r, v)| self.gpr[r] as u32 == v)
        {
            self.break_regs = Some(Box::new(self.gpr));
        }
        if let Some(counts) = &mut self.pc_counts {
            *counts.entry(self.pc).or_insert(0) += 1;
        }
        if let Some((pc, log)) = &mut self.arg_log
            && *pc == self.pc
        {
            let g = |r: usize| self.gpr[r] as u32;
            *log.entry([g(31), g(4), g(5), g(6), g(7)]).or_insert(0) += 1;
        }
        self.in_delay = self.delay_pending;
        self.delay_pending = false;
        let di = std::mem::take(&mut self.pending_di);
        let instr = match self.cop0.translate(self.pc, false) {
            Ok(pa) => bus.read32(pa),
            Err(fault) => {
                self.tlb_fault(self.pc, false, fault);
                self.cycles += 1;
                self.unticked += 1;
                self.land_di(di);
                return;
            }
        };
        self.pc = self.next_pc;
        self.next_pc = self.pc.wrapping_add(4);
        self.execute(instr, bus);
        self.cycles += 1;
        self.unticked += 1;
        self.land_di(di);
    }

    /// The write of a DI run before the instruction just run (see
    /// `pending_di`).
    fn land_di(&mut self, di: bool) {
        if di {
            self.cop0.regs[cop0::STATUS] &= !cop0::STATUS_EIE;
            if let Some(log) = &mut self.eie_log
                && log.len() < 1000
            {
                log.push((self.current_pc, self.cop0.regs[cop0::STATUS]));
            }
        }
    }

    fn execute(&mut self, i: u32, bus: &mut Bus) {
        // None: a memory access faulted, and the exception is taken.
        let _ = self.execute_op(i, bus);
    }

    fn execute_op(&mut self, i: u32, bus: &mut Bus) -> Option<()> {
        let op = i >> 26;
        let rs = (i >> 21) & 31;
        let rt = (i >> 16) & 31;
        let rd = (i >> 11) & 31;
        let sa = (i >> 6) & 31;
        let imm = i & 0xFFFF;
        let simm = sext16(imm as u16);
        match op {
            // SYSCALL, after a fast boot: the system configuration.
            0x00 if i & 0x3F == 0x0C && self.osd_config.is_some() => {
                match self.r32(3) {
                    // GetOsdConfigParam(a0): answered here, as the kernel
                    // would once the browser set it.
                    0x4B => {
                        let config = self.osd_config.unwrap_or(0);
                        return self.store32(bus, self.r32(4), config);
                    }
                    // SetOsdConfigParam: the kernel's from now on.
                    0x4A => self.osd_config = None,
                    _ => {}
                }
                self.special(i, rs, rt, rd, sa);
            }
            0x00 => self.special(i, rs, rt, rd, sa),
            0x01 => self.regimm(rs, rt, imm),
            0x02 => self.branch((self.pc & 0xF000_0000) | ((i & 0x03FF_FFFF) << 2)),
            0x03 => {
                self.set64(31, u64::from(self.pc.wrapping_add(4)));
                self.branch((self.pc & 0xF000_0000) | ((i & 0x03FF_FFFF) << 2));
            }
            0x04 => {
                if rs == 0 && rt == 0 && (simm as i64) < 0 && (simm as i64) >= -16 && self.idle.is_none() {
                    self.detect_idle(simm as u32, bus);
                }
                self.cond_branch(self.r64(rs) == self.r64(rt), simm as u32, false);
            }
            0x05 => self.cond_branch(self.r64(rs) != self.r64(rt), simm as u32, false),
            0x06 => self.cond_branch(self.r64(rs) as i64 <= 0, simm as u32, false),
            0x07 => self.cond_branch(self.r64(rs) as i64 > 0, simm as u32, false),
            // ADDI (no overflow trap), ADDIU.
            0x08 | 0x09 => self.set64(rt, sext32(self.r32(rs).wrapping_add(simm as u32))),
            0x0A => self.set64(rt, u64::from((self.r64(rs) as i64) < simm as i64)),
            0x0B => self.set64(rt, u64::from(self.r64(rs) < simm)),
            0x0C => self.set64(rt, self.r64(rs) & u64::from(imm)),
            0x0D => self.set64(rt, self.r64(rs) | u64::from(imm)),
            0x0E => self.set64(rt, self.r64(rs) ^ u64::from(imm)),
            0x0F => self.set64(rt, sext32(imm << 16)),
            0x10 => self.cop0_op(i, rs, rt, rd, bus),
            0x11 => self.fpu_op(i, rs, rt, rd, sa),
            0x12 => self.cop2_op(i, rs, rt, rd, bus),
            0x14 => self.cond_branch(self.r64(rs) == self.r64(rt), simm as u32, true),
            0x15 => self.cond_branch(self.r64(rs) != self.r64(rt), simm as u32, true),
            0x16 => self.cond_branch(self.r64(rs) as i64 <= 0, simm as u32, true),
            0x17 => self.cond_branch(self.r64(rs) as i64 > 0, simm as u32, true),
            // DADDI (no overflow trap), DADDIU.
            0x18 | 0x19 => self.set64(rt, self.r64(rs).wrapping_add(simm)),
            0x1A => self.ldl(rs, rt, simm, bus)?,
            0x1B => self.ldr(rs, rt, simm, bus)?,
            0x1C => self.mmi(i, rs, rt, rd, sa),
            0x1E => {
                let addr = self.addr(rs, simm) & !15;
                let v = self.load128(bus, addr)?;
                self.set128(rt, v);
            }
            0x1F => {
                let addr = self.addr(rs, simm) & !15;
                self.store128(bus, addr, self.gpr[rt as usize])?;
            }
            0x20 => {
                let v = self.load8(bus, self.addr(rs, simm))?;
                self.set64(rt, v as i8 as i64 as u64);
            }
            0x21 => {
                let v = self.load16(bus, self.addr(rs, simm))?;
                self.set64(rt, sext16(v));
            }
            0x22 => self.lwl(rs, rt, simm, bus)?,
            0x23 => {
                let v = self.load32(bus, self.addr(rs, simm))?;
                self.set64(rt, sext32(v));
            }
            0x24 => {
                let v = self.load8(bus, self.addr(rs, simm))?;
                self.set64(rt, u64::from(v));
            }
            0x25 => {
                let v = self.load16(bus, self.addr(rs, simm))?;
                self.set64(rt, u64::from(v));
            }
            0x26 => self.lwr(rs, rt, simm, bus)?,
            0x27 => {
                let v = self.load32(bus, self.addr(rs, simm))?;
                self.set64(rt, u64::from(v));
            }
            0x28 => self.store8(bus, self.addr(rs, simm), self.r32(rt) as u8)?,
            0x29 => self.store16(bus, self.addr(rs, simm), self.r32(rt) as u16)?,
            0x2A => self.swl(rs, rt, simm, bus)?,
            0x2B => self.store32(bus, self.addr(rs, simm), self.r32(rt))?,
            0x2C => self.sdl(rs, rt, simm, bus)?,
            0x2D => self.sdr(rs, rt, simm, bus)?,
            0x2E => self.swr(rs, rt, simm, bus)?,
            // CACHE, PREF: no caches emulated.
            0x2F | 0x33 => {}
            0x31 => {
                let v = self.load32(bus, self.addr(rs, simm))?;
                self.fpu.fpr[rt as usize] = v;
            }
            0x36 => {
                let addr = self.addr(rs, simm) & !15;
                let v = self.load128(bus, addr)?;
                set_vf(&mut bus.vu0, rt as usize, v);
            }
            0x37 => {
                let v = self.load64(bus, self.addr(rs, simm))?;
                self.set64(rt, v);
            }
            0x39 => self.store32(bus, self.addr(rs, simm), self.fpu.fpr[rt as usize])?,
            0x3E => {
                let addr = self.addr(rs, simm) & !15;
                let v = get_vf(&bus.vu0, rt as usize);
                self.store128(bus, addr, v)?;
            }
            0x3F => self.store64(bus, self.addr(rs, simm), self.r64(rt))?,
            _ => {
                self.unimplemented("reserved opcode");
                self.exception(Exception::Reserved);
            }
        }
        Some(())
    }

    #[inline]
    fn addr(&self, rs: u32, simm: u64) -> u32 {
        self.r32(rs).wrapping_add(simm as u32)
    }

    fn special(&mut self, i: u32, rs: u32, rt: u32, rd: u32, sa: u32) {
        let s = self.r64(rs);
        let t = self.r64(rt);
        match i & 0x3F {
            0x00 => self.set64(rd, sext32(self.r32(rt) << sa)),
            0x02 => self.set64(rd, sext32(self.r32(rt) >> sa)),
            0x03 => self.set64(rd, sext32(((self.r32(rt) as i32) >> sa) as u32)),
            0x04 => self.set64(rd, sext32(self.r32(rt) << (s & 31))),
            0x06 => self.set64(rd, sext32(self.r32(rt) >> (s & 31))),
            0x07 => self.set64(rd, sext32(((self.r32(rt) as i32) >> (s & 31)) as u32)),
            0x08 => self.branch(s as u32),
            0x09 => {
                let target = s as u32;
                self.set64(rd, u64::from(self.pc.wrapping_add(4)));
                self.branch(target);
            }
            0x0A => {
                if t == 0 {
                    self.set64(rd, s);
                }
            }
            0x0B => {
                if t != 0 {
                    self.set64(rd, s);
                }
            }
            0x0C => self.exception(Exception::Syscall),
            0x0D => self.exception(Exception::Break),
            0x0F => {}
            0x10 => self.set64(rd, self.hi64()),
            0x11 => self.set_hi64(s),
            0x12 => self.set64(rd, self.lo64()),
            0x13 => self.set_lo64(s),
            0x14 => self.set64(rd, t << (s & 63)),
            0x16 => self.set64(rd, t >> (s & 63)),
            0x17 => self.set64(rd, ((t as i64) >> (s & 63)) as u64),
            0x18 => {
                let p = i64::from(s as i32) * i64::from(t as i32);
                self.set_mult(rd, p as u64, false);
            }
            0x19 => {
                let p = u64::from(s as u32) * u64::from(t as u32);
                self.set_mult(rd, p, false);
            }
            0x1A => {
                let (q, r) = div_signed(s as u32, t as u32);
                self.set_lo64(sext32(q));
                self.set_hi64(sext32(r));
            }
            0x1B => {
                let (q, r) = div_unsigned(s as u32, t as u32);
                self.set_lo64(sext32(q));
                self.set_hi64(sext32(r));
            }
            // ADD (no overflow trap), ADDU, SUB, SUBU.
            0x20 | 0x21 => self.set64(rd, sext32((s as u32).wrapping_add(t as u32))),
            0x22 | 0x23 => self.set64(rd, sext32((s as u32).wrapping_sub(t as u32))),
            0x24 => self.set64(rd, s & t),
            0x25 => self.set64(rd, s | t),
            0x26 => self.set64(rd, s ^ t),
            0x27 => self.set64(rd, !(s | t)),
            0x28 => self.set64(rd, u64::from(self.sa)),
            0x29 => self.sa = s as u32 & 15,
            0x2A => self.set64(rd, u64::from((s as i64) < (t as i64))),
            0x2B => self.set64(rd, u64::from(s < t)),
            // DADD (no overflow trap), DADDU, DSUB, DSUBU.
            0x2C | 0x2D => self.set64(rd, s.wrapping_add(t)),
            0x2E | 0x2F => self.set64(rd, s.wrapping_sub(t)),
            // Traps: TGE, TGEU, TLT, TLTU, TEQ, TNE.
            0x30..=0x36 => {
                let trap = match i & 0x3F {
                    0x30 => (s as i64) >= (t as i64),
                    0x31 => s >= t,
                    0x32 => (s as i64) < (t as i64),
                    0x33 => s < t,
                    0x34 => s == t,
                    0x36 => s != t,
                    _ => false,
                };
                if trap {
                    self.exception(Exception::Trap);
                }
            }
            0x38 => self.set64(rd, t << sa),
            0x3A => self.set64(rd, t >> sa),
            0x3B => self.set64(rd, ((t as i64) >> sa) as u64),
            0x3C => self.set64(rd, t << (sa + 32)),
            0x3E => self.set64(rd, t >> (sa + 32)),
            0x3F => self.set64(rd, ((t as i64) >> (sa + 32)) as u64),
            _ => {
                self.unimplemented("SPECIAL ?");
                self.exception(Exception::Reserved);
            }
        }
    }

    /// MULT/MULTU: LO and HI get the halves of the product, sign-extended,
    /// and so does rd (the R5900's three-operand form). `pipe1`: MULT1.
    fn set_mult(&mut self, rd: u32, p: u64, pipe1: bool) {
        let lo = sext32(p as u32);
        let hi = sext32((p >> 32) as u32);
        if pipe1 {
            self.set_lo1(lo);
            self.set_hi1(hi);
        } else {
            self.set_lo64(lo);
            self.set_hi64(hi);
        }
        self.set64(rd, lo);
    }

    fn regimm(&mut self, rs: u32, rt: u32, imm: u32) {
        let s = self.r64(rs) as i64;
        let offset = sext16(imm as u16) as u32;
        match rt {
            0x00 => self.cond_branch(s < 0, offset, false),
            0x01 => self.cond_branch(s >= 0, offset, false),
            0x02 => self.cond_branch(s < 0, offset, true),
            0x03 => self.cond_branch(s >= 0, offset, true),
            // Traps with an immediate: TGEI, TGEIU, TLTI, TLTIU, TEQI, TNEI.
            0x08..=0x0E => {
                let t = sext16(imm as u16);
                let u = s as u64;
                let trap = match rt {
                    0x08 => s >= t as i64,
                    0x09 => u >= t,
                    0x0A => s < t as i64,
                    0x0B => u < t,
                    0x0C => u == t,
                    0x0E => u != t,
                    _ => false,
                };
                if trap {
                    self.exception(Exception::Trap);
                }
            }
            0x10..=0x13 => {
                self.set64(31, u64::from(self.pc.wrapping_add(4)));
                let taken = if rt & 1 == 0 { s < 0 } else { s >= 0 };
                self.cond_branch(taken, offset, rt & 2 != 0);
            }
            // MTSAB, MTSAH: the funnel shift amount, in bytes.
            0x18 => self.sa = (self.r32(rs) ^ imm) & 15,
            0x19 => self.sa = ((self.r32(rs) ^ imm) & 7) * 2,
            _ => {
                self.unimplemented("REGIMM ?");
                self.exception(Exception::Reserved);
            }
        }
    }

    // --- Memory accesses, through the TLB -------------------------------------

    /// Takes the exception for a failed translation.
    fn tlb_fault(&mut self, va: u32, write: bool, fault: Fault) {
        self.cop0.record_fault(va);
        let e = match fault {
            Fault::Modified => Exception::Modified,
            _ if write => Exception::TlbStore,
            _ => Exception::TlbLoad,
        };
        // Refills have a vector of their own, outside exception handlers.
        let refill = fault == Fault::Refill && !self.cop0.exl();
        self.exception_at(e, if refill { 0x000 } else { 0x180 });
    }

    #[inline]
    fn data_addr(&mut self, va: u32, write: bool) -> Option<u32> {
        match self.cop0.translate(va, write) {
            Ok(pa) => Some(pa),
            Err(fault) => {
                self.tlb_fault(va, write, fault);
                None
            }
        }
    }

    fn load8(&mut self, bus: &mut Bus, va: u32) -> Option<u8> {
        let pa = self.data_addr(va, false)?;
        Some(bus.read8(pa))
    }
    fn load16(&mut self, bus: &mut Bus, va: u32) -> Option<u16> {
        let pa = self.data_addr(va, false)?;
        Some(bus.read16(pa))
    }
    fn load32(&mut self, bus: &mut Bus, va: u32) -> Option<u32> {
        let pa = self.data_addr(va, false)?;
        let v = bus.read32(pa);
        if let Some((at, reads)) = &mut self.read_watch
            && *at == pa
        {
            let r = reads.entry(self.current_pc).or_default();
            *r = (r.0 + 1, v, r.2 | v);
        }
        if pa == crate::bus::INTC_STAT {
            self.note_poll(v);
            if std::mem::take(&mut self.intc_watch) {
                self.polling = true;
            }
        }
        Some(v)
    }

    /// INTC_STAT, read by the instruction at `current_pc`: the same value
    /// twice in a row from there, and the EE is waiting for an interrupt.
    /// INTC_STAT only changes between slices (or by the EE's own stores,
    /// which polling does not make), so the rest of the slice can pass
    /// without running the loop.
    fn note_poll(&mut self, v: u32) {
        let (pc, last) = self.poll;
        self.poll = (self.current_pc, v);
        if pc == self.current_pc && last == v {
            self.polling = true;
        }
    }
    fn load64(&mut self, bus: &mut Bus, va: u32) -> Option<u64> {
        let pa = self.data_addr(va, false)?;
        Some(bus.read64(pa))
    }
    fn load128(&mut self, bus: &mut Bus, va: u32) -> Option<u128> {
        let pa = self.data_addr(va, false)?;
        Some(bus.read128(pa))
    }
    fn store8(&mut self, bus: &mut Bus, va: u32, v: u8) -> Option<()> {
        let pa = self.data_addr(va, true)?;
        bus.store_pc = self.current_pc;
        bus.write8(pa, v);
        Some(())
    }
    fn store16(&mut self, bus: &mut Bus, va: u32, v: u16) -> Option<()> {
        let pa = self.data_addr(va, true)?;
        bus.store_pc = self.current_pc;
        bus.write16(pa, v);
        Some(())
    }
    fn store32(&mut self, bus: &mut Bus, va: u32, v: u32) -> Option<()> {
        let pa = self.data_addr(va, true)?;
        bus.store_pc = self.current_pc;
        bus.write32(pa, v);
        Some(())
    }
    fn store64(&mut self, bus: &mut Bus, va: u32, v: u64) -> Option<()> {
        let pa = self.data_addr(va, true)?;
        bus.store_pc = self.current_pc;
        bus.write64(pa, v);
        Some(())
    }
    fn store128(&mut self, bus: &mut Bus, va: u32, v: u128) -> Option<()> {
        let pa = self.data_addr(va, true)?;
        bus.store_pc = self.current_pc;
        bus.write128(pa, v);
        Some(())
    }

    // --- Unaligned loads and stores (little-endian) ---------------------------

    fn lwl(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = (addr & 3) * 8;
        let mem = self.load32(bus, addr & !3)?;
        let mask = 0x00FF_FFFFu32 >> shift;
        let v = (self.r32(rt) & mask) | (mem << (24 - shift));
        self.set64(rt, sext32(v));
        Some(())
    }

    fn lwr(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = (addr & 3) * 8;
        let mem = self.load32(bus, addr & !3)?;
        if shift == 0 {
            self.set64(rt, sext32(mem));
        } else {
            // The upper 32 bits are kept.
            let mask = 0xFFFF_FF00u32 << (24 - shift);
            let v = (self.r32(rt) & mask) | (mem >> shift);
            self.set64(rt, (self.r64(rt) & 0xFFFF_FFFF_0000_0000) | u64::from(v));
        }
        Some(())
    }

    fn swl(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = (addr & 3) * 8;
        let mem = self.load32(bus, addr & !3)?;
        let mask = 0xFFFF_FF00u32.checked_shl(shift).unwrap_or(0);
        let mask = if shift == 24 { 0 } else { mask };
        self.store32(bus, addr & !3, (mem & mask) | (self.r32(rt) >> (24 - shift)))?;
        Some(())
    }

    fn swr(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = (addr & 3) * 8;
        let mem = self.load32(bus, addr & !3)?;
        let mask = 0x00FF_FFFFu32 >> (24 - shift);
        let mask = if shift == 0 { 0 } else { mask };
        self.store32(bus, addr & !3, (mem & mask) | (self.r32(rt) << shift))?;
        Some(())
    }

    fn ldl(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = u64::from(addr & 7) * 8;
        let mem = self.load64(bus, addr & !7)?;
        let mask = 0x00FF_FFFF_FFFF_FFFFu64 >> shift;
        self.set64(rt, (self.r64(rt) & mask) | (mem << (56 - shift)));
        Some(())
    }

    fn ldr(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = u64::from(addr & 7) * 8;
        let mem = self.load64(bus, addr & !7)?;
        let mask = if shift == 0 {
            0
        } else {
            0xFFFF_FFFF_FFFF_FF00u64 << (56 - shift)
        };
        self.set64(rt, (self.r64(rt) & mask) | (mem >> shift));
        Some(())
    }

    fn sdl(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = u64::from(addr & 7) * 8;
        let mem = self.load64(bus, addr & !7)?;
        let mask = if shift == 56 {
            0
        } else {
            0xFFFF_FFFF_FFFF_FF00u64 << shift
        };
        self.store64(bus, addr & !7, (mem & mask) | (self.r64(rt) >> (56 - shift)))?;
        Some(())
    }

    fn sdr(&mut self, rs: u32, rt: u32, simm: u64, bus: &mut Bus) -> Option<()> {
        let addr = self.addr(rs, simm);
        let shift = u64::from(addr & 7) * 8;
        let mem = self.load64(bus, addr & !7)?;
        let mask = if shift == 0 {
            0
        } else {
            0x00FF_FFFF_FFFF_FFFFu64 >> (56 - shift)
        };
        self.store64(bus, addr & !7, (mem & mask) | (self.r64(rt) << shift))?;
        Some(())
    }

    // --- COP0 ---------------------------------------------------------------------

    fn cop0_op(&mut self, i: u32, rs: u32, rt: u32, rd: u32, bus: &mut Bus) {
        self.flush_ticks();
        let eie = self.cop0.regs[cop0::STATUS] & cop0::STATUS_EIE;
        self.cop0_op_inner(i, rs, rt, rd, bus);
        let status = self.cop0.regs[cop0::STATUS];
        if let Some(log) = &mut self.eie_log
            && status & cop0::STATUS_EIE != eie
            && log.len() < 1000
        {
            log.push((self.current_pc, status));
        }
    }

    fn cop0_op_inner(&mut self, i: u32, rs: u32, rt: u32, rd: u32, bus: &mut Bus) {
        match rs {
            0x00 => {
                let v = self.cop0.read(rd, i & 0x7FF);
                self.set64(rt, sext32(v));
            }
            0x04 => {
                let v = self.r32(rt);
                self.cop0.write(rd, i & 0x7FF, v);
            }
            0x08 => {
                // BC0F, BC0T, BC0FL, BC0TL: on the DMA controller's condition.
                let cond = bus.cpcond0();
                let taken = if rt & 1 == 0 { !cond } else { cond };
                self.cond_branch(taken, sext16(i as u16) as u32, rt & 2 != 0);
            }
            0x10 => match i & 0x3F {
                0x01 => self.cop0.tlbr(),
                0x02 => self.cop0.tlbwi(),
                0x06 => self.cop0.tlbwr(),
                0x08 => self.cop0.tlbp(),
                0x18 => {
                    let target = self.cop0.eret();
                    self.pc = target;
                    self.next_pc = target.wrapping_add(4);
                    self.delay_pending = false;
                }
                0x38 => self.cop0.ei(),
                0x39 => self.pending_di |= self.cop0.may_toggle_interrupts(),
                _ => self.unimplemented("COP0 ?"),
            },
            _ => self.unimplemented("COP0 ?"),
        }
    }

    // --- COP2 (VU0 macro mode) -------------------------------------------------

    fn cop2_op(&mut self, i: u32, rs: u32, rt: u32, rd: u32, bus: &mut Bus) {
        if rs & 0x10 != 0 {
            return self.vu0_macro(i, bus);
        }
        match rs {
            // QMFC2, CFC2, QMTC2, CTC2.
            0x01 => self.set128(rt, get_vf(&bus.vu0, rd as usize)),
            0x02 => {
                let v = match rd {
                    27 => bus.cmsar0,
                    _ => bus.vu0.control(rd),
                };
                self.set64(rt, sext32(v));
            }
            0x05 => set_vf(&mut bus.vu0, rd as usize, self.gpr[rt as usize]),
            0x06 => {
                let v = self.r32(rt);
                match rd {
                    27 => bus.cmsar0 = v & 0xFFFF,
                    // FBRST: bit 1 resets VU0, bit 9 VU1.
                    28 => {
                        if v & 2 != 0 {
                            bus.vu0.reset();
                        }
                        if v & 0x200 != 0 {
                            bus.vu1q.feed(&mut bus.vu1).reset();
                        }
                    }
                    // CMSAR1: starts VU1 there.
                    31 => bus.vu1q.feed(&mut bus.vu1).start_at(v & 0xFFFF),
                    _ => bus.vu0.set_control(rd, v),
                }
            }
            // BC2F, BC2T...: VU0 is never busy (its programs run at once).
            0x08 => {
                let taken = rt & 1 == 0;
                self.cond_branch(taken, sext16(i as u16) as u32, rt & 2 != 0);
            }
            _ => self.unimplemented("COP2 ?"),
        }
    }

    /// VU0 macro mode: VU instructions issued by the EE.
    fn vu0_macro(&mut self, i: u32, bus: &mut Bus) {
        let funct = i & 0x3F;
        let it = ((i >> 16) & 15) as usize;
        let is = ((i >> 11) & 15) as usize;
        let id = ((i >> 6) & 15) as usize;
        match funct {
            0x00..=0x2F => bus.vu0.macro_upper(i),
            0x30..=0x35 => bus.vu0.integer(funct, it, is, id, i),
            // VCALLMS, VCALLMSR: a VU0 microprogram, run to its end.
            0x38 => bus.vu0.run((i >> 6) & 0x7FFF, &mut bus.gif, &mut bus.gs),
            0x39 => bus.vu0.run(bus.cmsar0, &mut bus.gif, &mut bus.gs),
            0x3C..=0x3F => {
                let op = (funct & 3) | ((i >> 4) & 0x7C);
                if op < 0x30 {
                    bus.vu0.macro_upper(i);
                } else {
                    bus.vu0.special2(op, i, None);
                }
            }
            _ => self.unimplemented("VU0 macro ?"),
        }
    }
}

/// Signed division as the R5900 does it, by zero and overflowing too:
/// (quotient, remainder).
fn div_signed(n: u32, d: u32) -> (u32, u32) {
    let (n, d) = (n as i32, d as i32);
    if d == 0 {
        (if n < 0 { 1 } else { u32::MAX }, n as u32)
    } else if n == i32::MIN && d == -1 {
        (n as u32, 0)
    } else {
        ((n / d) as u32, (n % d) as u32)
    }
}

fn div_unsigned(n: u32, d: u32) -> (u32, u32) {
    match (n.checked_div(d), n.checked_rem(d)) {
        (Some(q), Some(r)) => (q, r),
        _ => (u32::MAX, n),
    }
}

#[cfg(test)]
mod tests;

/// A VU register as the EE's 128-bit transfers see it.
fn get_vf(vu: &crate::vu::Vu, r: usize) -> u128 {
    vu.vf[r & 31]
        .iter()
        .enumerate()
        .fold(0, |a, (k, &x)| a | u128::from(x) << (32 * k))
}

fn set_vf(vu: &mut crate::vu::Vu, r: usize, v: u128) {
    if r & 31 != 0 {
        vu.vf[r & 31] = std::array::from_fn(|k| (v >> (32 * k)) as u32);
    }
}
