//! MIPS R3000A interpreter with load and branch delay slots, coprocessor 0
//! (exceptions) and coprocessor 2 (the GTE).

use crate::bus::Bus;
use crate::gte::Gte;

/// What the CPU needs from the machine around it: the PlayStation's bus,
/// or the PlayStation 2's IOP, which is this CPU too.
pub trait CpuBus {
    /// An instruction fetch.
    fn fetch(&mut self, addr: u32) -> u32;
    fn load8(&mut self, addr: u32) -> u8;
    fn load16(&mut self, addr: u32) -> u16;
    fn load32(&mut self, addr: u32) -> u32;
    fn store8(&mut self, addr: u32, v: u8);
    fn store16(&mut self, addr: u32, v: u16);
    fn store32(&mut self, addr: u32, v: u32);
    /// Whether the instruction cache is on (cache control bit 11).
    fn icache_enabled(&self) -> bool;
    /// The interrupt controller's line to the CPU.
    fn irq_active(&self) -> bool;
    /// A vertex the GTE projected: the screen position it was rounded to
    /// (packed as SXY) and its exact x, y and depth (an enhancement: see
    /// `Gte::precise`).
    fn gte_vertex(&mut self, _sxy: u32, _exact: [f32; 3]) {}
}

impl CpuBus for Bus {
    fn fetch(&mut self, addr: u32) -> u32 {
        Bus::fetch(self, addr)
    }
    fn load8(&mut self, addr: u32) -> u8 {
        Bus::load8(self, addr)
    }
    fn load16(&mut self, addr: u32) -> u16 {
        Bus::load16(self, addr)
    }
    fn load32(&mut self, addr: u32) -> u32 {
        Bus::load32(self, addr)
    }
    fn store8(&mut self, addr: u32, v: u8) {
        Bus::store8(self, addr, v);
    }
    fn store16(&mut self, addr: u32, v: u16) {
        Bus::store16(self, addr, v);
    }
    fn store32(&mut self, addr: u32, v: u32) {
        Bus::store32(self, addr, v);
    }
    fn icache_enabled(&self) -> bool {
        Bus::icache_enabled(self)
    }
    fn irq_active(&self) -> bool {
        self.irq.active()
    }
    fn gte_vertex(&mut self, sxy: u32, exact: [f32; 3]) {
        self.gpu.exact_vertex(sxy, exact);
    }
}

/// The PlayStation's processor ID (COP0 PRId).
const PS1_PRID: u32 = 2;

fn default_prid() -> u32 {
    PS1_PRID
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exception {
    Interrupt = 0x0,
    LoadAddressError = 0x4,
    StoreAddressError = 0x5,
    Syscall = 0x8,
    Break = 0x9,
    ReservedInstruction = 0xa,
    CoprocessorUnusable = 0xb,
    Overflow = 0xc,
}

/// Reset vector, in the BIOS.
pub const RESET_PC: u32 = 0xbfc0_0000;

/// Saves a field that only holds the instruction being executed (every
/// step sets it before reading it) as its default: kept in the save states'
/// format, but the state between instructions no longer depends on how the
/// last one ran (interpreted or compiled), so the two compare equal.
fn scratch<T: Default + serde::Serialize, S: serde::Serializer>(_: &T, s: S) -> Result<S::Ok, S::Error> {
    T::default().serialize(s)
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub pc: u32,
    pub(crate) next_pc: u32,
    /// Address of the instruction being executed, for EPC.
    #[serde(serialize_with = "scratch")]
    current_pc: u32,
    pub(crate) regs: [u32; 32],
    /// The same as `regs` between instructions: kept for the save states'
    /// format (it once held the current instruction's writes).
    pub(crate) out_regs: [u32; 32],
    /// Pending load: register and value, applied after the next instruction.
    pub(crate) load: (usize, u32),
    /// The load landing after the current instruction (the delay slot: the
    /// instruction still reads the old value), and the register the
    /// instruction wrote (its write wins over the load's). Empty between
    /// instructions, so not in save states.
    #[serde(skip)]
    landing: (usize, u32),
    #[serde(skip)]
    wrote: usize,
    hi: u32,
    lo: u32,
    pub(crate) branch: bool,
    #[serde(serialize_with = "scratch")]
    pub(crate) delay_slot: bool,
    // Coprocessor 0.
    pub(crate) sr: u32,
    cause: u32,
    epc: u32,
    bad_vaddr: u32,
    /// The debug registers BPC (3), BDA (5), DCIC (7), BDAM (9) and BPCM
    /// (11). Their breakpoints are not emulated, but they keep what is
    /// written: LibCrypt hides its key and a pointer there. Saved after
    /// the rest of the state (see `System::save_state`).
    #[serde(skip)]
    debug: [u32; 5],
    pub gte: Gte,
    /// COP0 PRId: 2 on the PlayStation, 0x1F as the PS2's IOP.
    #[serde(skip, default = "default_prid")]
    pub prid: u32,
    /// The last exception other than interrupts and syscalls: cause code,
    /// EPC and the instruction there. For debugging.
    #[serde(skip)]
    pub last_fault: Option<(u32, u32, u32)>,
    /// Diagnostics: a word's physical address whose stores to record (pc,
    /// address, value), when set.
    #[serde(skip)]
    pub watch: Option<u32>,
    #[serde(skip)]
    pub watched: Vec<(u32, u32, u32)>,
    #[serde(serialize_with = "scratch")]
    current_instruction: u32,
    /// Cycles elapsed since reset.
    pub cycles: u64,
    /// Extra cycles of the instruction being executed (memory, stalls).
    #[serde(serialize_with = "scratch")]
    extra: u32,
    /// When the multiplier/divider and the GTE have their results ready.
    hilo_ready: u64,
    gte_ready: u64,
    /// Instruction cache: per 16-byte line, the tag (physical address >> 12)
    /// plus one, 0 when invalid.
    pub(crate) icache: Box<[u32]>,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

/// Base cost of an instruction (pipeline), and of exceptions.
const CYCLES_PER_INSTRUCTION: u32 = 1;
/// Memory access times, in CPU cycles, approximating the hardware's.
pub(crate) const RAM_ACCESS: u32 = 5;
pub(crate) const BIOS_ACCESS: u32 = 22;
pub(crate) const IO_ACCESS: u32 = 3;
/// Filling an instruction cache line (4 words) from RAM.
pub(crate) const ICACHE_FILL: u32 = RAM_ACCESS + 3;

/// GTE command durations (psx-spx), by function code.
fn gte_cycles(cmd: u32) -> u32 {
    match cmd & 0x3f {
        0x01 => 15,
        0x06 => 8,
        0x0c => 6,
        0x10 | 0x11 | 0x12 | 0x29 => 8,
        0x13 => 19,
        0x14 => 13,
        0x16 => 44,
        0x1b => 17,
        0x1c => 11,
        0x1e => 14,
        0x20 => 30,
        0x28 | 0x2d | 0x3d | 0x3e => 5,
        0x2a => 17,
        0x2e => 6,
        0x30 => 23,
        0x3f => 39,
        _ => 1,
    }
}

/// Cost of a data access to `addr` (beyond the instruction's own cycle).
fn data_access_cycles(addr: u32) -> u32 {
    let p = addr & 0x1fff_ffff;
    match p {
        0x0000_0000..=0x007f_ffff => RAM_ACCESS,
        0x1f80_0000..=0x1f80_03ff => 0,
        0x1fc0_0000..=0x1fc7_ffff => BIOS_ACCESS,
        _ => IO_ACCESS,
    }
}

impl Cpu {
    /// Diagnostics: the state, field by field.
    #[doc(hidden)]
    pub fn debug_fields(&self) -> Vec<(&'static str, String)> {
        let gte = postcard::to_stdvec(&self.gte).unwrap_or_default();
        let gte = gte.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        vec![
            ("pc", format!("{:08x}", self.pc)),
            ("next_pc", format!("{:08x}", self.next_pc)),
            ("current_pc", format!("{:08x}", self.current_pc)),
            ("regs", format!("{:08x?}", self.regs)),
            ("out_regs", format!("{:08x?}", self.out_regs)),
            ("load", format!("{:?}", self.load)),
            ("hi/lo", format!("{:08x} {:08x}", self.hi, self.lo)),
            ("branch", format!("{}", self.branch)),
            ("delay_slot", format!("{}", self.delay_slot)),
            (
                "sr/cause/epc/bad",
                format!(
                    "{:08x} {:08x} {:08x} {:08x}",
                    self.sr, self.cause, self.epc, self.bad_vaddr
                ),
            ),
            ("gte", format!("{gte:016x}")),
            ("current_instruction", format!("{:08x}", self.current_instruction)),
            ("cycles", format!("{}", self.cycles)),
            ("extra", format!("{}", self.extra)),
            (
                "hilo_ready/gte_ready",
                format!("{} {}", self.hilo_ready, self.gte_ready),
            ),
            ("icache", {
                let valid = self.icache.iter().filter(|&&t| t != 0).count();
                let h = self.icache.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &t| {
                    (h ^ u64::from(t)).wrapping_mul(0x0100_0000_01b3)
                });
                format!("{valid} valid, {h:016x}")
            }),
        ]
    }

    pub fn new() -> Self {
        Self {
            pc: RESET_PC,
            next_pc: RESET_PC.wrapping_add(4),
            current_pc: RESET_PC,
            regs: [0; 32],
            out_regs: [0; 32],
            load: (0, 0),
            landing: (0, 0),
            wrote: 0,
            hi: 0,
            lo: 0,
            branch: false,
            delay_slot: false,
            sr: 0,
            cause: 0,
            epc: 0,
            bad_vaddr: 0,
            debug: [0; 5],
            gte: Gte::default(),
            watch: None,
            watched: Vec::new(),
            last_fault: None,
            current_instruction: 0,
            cycles: 0,
            extra: 0,
            hilo_ready: 0,
            gte_ready: 0,
            icache: vec![0; 256].into_boxed_slice(),
            prid: PS1_PRID,
        }
    }

    /// Cycles to fetch the instruction at `pc`, through the instruction
    /// cache when the address is cached (KUSEG/KSEG0) and the cache is on.
    fn fetch_cycles(&mut self, pc: u32, bus: &impl CpuBus) -> u32 {
        let p = pc & 0x1fff_ffff;
        let uncached = match p {
            0x0000_0000..=0x007f_ffff => RAM_ACCESS,
            0x1fc0_0000..=0x1fc7_ffff => BIOS_ACCESS,
            _ => IO_ACCESS,
        };
        let kseg1 = (pc >> 29) == 5;
        if kseg1 || !bus.icache_enabled() {
            return uncached;
        }
        let line = ((pc >> 4) & 0xff) as usize;
        let tag = (p >> 12) + 1;
        if self.icache[line] == tag {
            1
        } else {
            self.icache[line] = tag;
            if uncached == RAM_ACCESS {
                ICACHE_FILL
            } else {
                uncached * 4
            }
        }
    }

    /// Waits for a pending multiplier or GTE result.
    fn stall_until(&mut self, ready: u64) {
        let now = self.cycles + u64::from(self.extra);
        if ready > now {
            self.extra += (ready - now) as u32;
        }
    }

    /// A general-purpose register, for diagnostics.
    pub fn reg(&self, i: usize) -> u32 {
        self.regs[i & 31]
    }

    pub fn set_reg_now(&mut self, i: usize, v: u32) {
        if i != 0 {
            self.regs[i] = v;
            self.out_regs[i] = v;
        }
    }

    /// Jumps straight to `pc`, cancelling any delay slot. Used to start
    /// side-loaded executables.
    pub fn jump(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.load = (0, 0);
        self.branch = false;
        self.delay_slot = false;
    }

    fn set_reg(&mut self, i: usize, v: u32) {
        if i != 0 {
            self.regs[i] = v;
            self.out_regs[i] = v;
        }
        self.wrote = i;
    }

    /// The load landing after this instruction, unless the instruction
    /// wrote the register itself.
    fn land(&mut self) {
        let (reg, val) = std::mem::take(&mut self.landing);
        if reg != 0 && reg != self.wrote {
            self.regs[reg] = val;
            self.out_regs[reg] = val;
        }
    }

    /// Whether `step` would run the next instruction plainly (not in a delay
    /// slot, no load in flight, no interrupt taken, pc aligned), and Cause
    /// as it would set it (`irq`: the interrupt controller's line): where a
    /// compiled block may start.
    pub(crate) fn plain_start(&self, irq: bool) -> Option<u32> {
        let cause = if irq {
            self.cause | (1 << 10)
        } else {
            self.cause & !(1 << 10)
        };
        let interrupt = self.sr & 1 != 0 && (self.sr & cause & 0x700) != 0;
        (!self.branch && self.load.0 == 0 && self.pc & 3 == 0 && !interrupt).then_some(cause)
    }

    pub(crate) fn set_cause(&mut self, cause: u32) {
        self.cause = cause;
    }

    /// One instruction of a compiled block, through the interpreter: what
    /// `step` does after fetching it (the fetch's extra cycles given), with
    /// the CPU's state as `step` would find it. Returns its cycles, and
    /// whether it ran plainly (no exception, no load issued).
    pub(crate) fn step_compiled(
        &mut self,
        bus: &mut impl CpuBus,
        instr: u32,
        pc: u32,
        in_delay: bool,
        fetch_extra: u32,
    ) -> (u32, bool) {
        self.current_pc = pc;
        self.delay_slot = in_delay;
        self.extra = fetch_extra;
        let expected = self.next_pc;
        self.pc = self.next_pc;
        self.next_pc = self.pc.wrapping_add(4);
        self.landing = std::mem::take(&mut self.load);
        self.wrote = 0;
        self.branch = false;
        self.current_instruction = instr;
        self.execute(instr, bus);
        self.land();
        let cycles = CYCLES_PER_INSTRUCTION + self.extra;
        self.cycles += u64::from(cycles);
        (cycles, self.pc == expected && self.load.0 == 0)
    }

    /// Executes one instruction; returns the cycles it took.
    /// COP0's Status, Cause, EPC and BadVaddr, for diagnostics.
    pub fn cop0_state(&self) -> [u32; 4] {
        [self.sr, self.cause, self.epc, self.bad_vaddr]
    }

    /// Whether the next instruction is a branch's delay slot.
    pub fn in_delay_slot(&self) -> bool {
        self.branch
    }

    pub fn step(&mut self, bus: &mut impl CpuBus) -> u32 {
        let cycles = self.step_inner(bus);
        self.cycles += u64::from(cycles);
        cycles
    }

    fn step_inner(&mut self, bus: &mut impl CpuBus) -> u32 {
        self.current_pc = self.pc;
        // Whether this instruction sits in the delay slot of a branch, for
        // exceptions raised before it executes.
        self.delay_slot = self.branch;
        if self.current_pc & 3 != 0 {
            self.bad_vaddr = self.current_pc;
            self.exception(Exception::LoadAddressError);
            return CYCLES_PER_INSTRUCTION;
        }
        let instruction = bus.fetch(self.pc);
        self.extra = self.fetch_cycles(self.pc, bus).saturating_sub(1);

        // Hardware interrupt line from the interrupt controller.
        if bus.irq_active() {
            self.cause |= 1 << 10;
        } else {
            self.cause &= !(1 << 10);
        }
        if self.sr & 1 != 0 && (self.sr & self.cause & 0x700) != 0 {
            // A GTE command at the interrupted address still executes; the
            // BIOS handler then skips it by looking at EPC.
            if instruction >> 25 == 0x25 {
                self.gte_command(instruction & 0x1ff_ffff, bus);
            }
            self.exception(Exception::Interrupt);
            return CYCLES_PER_INSTRUCTION + self.extra;
        }

        self.pc = self.next_pc;
        self.next_pc = self.pc.wrapping_add(4);
        self.landing = std::mem::take(&mut self.load);
        self.wrote = 0;
        self.branch = false;

        self.current_instruction = instruction;
        self.execute(instruction, bus);
        self.land();
        CYCLES_PER_INSTRUCTION + self.extra
    }

    fn gte_command(&mut self, cmd: u32, bus: &mut impl CpuBus) {
        self.stall_until(self.gte_ready);
        self.gte.command(cmd);
        self.gte_ready = self.cycles + u64::from(self.extra) + u64::from(gte_cycles(cmd));
        for &(sxy, exact) in &self.gte.projected {
            bus.gte_vertex(sxy, exact);
        }
    }

    /// Multiplications take longer with larger operands; divisions always 36.
    fn start_muldiv(&mut self, cycles: u32) {
        self.hilo_ready = self.cycles + u64::from(self.extra) + u64::from(cycles);
    }

    fn mult_cycles(v: u32, signed: bool) -> u32 {
        let magnitude = if signed && (v as i32) < 0 { !v } else { v };
        if magnitude < 0x800 {
            6
        } else if magnitude < 0x10_0000 {
            9
        } else {
            13
        }
    }

    fn exception(&mut self, cause: Exception) {
        let handler = if self.sr & (1 << 22) != 0 {
            0xbfc0_0180
        } else {
            0x8000_0080
        };
        // Push the interrupt-enable/user-mode stack.
        let mode = self.sr & 0x3f;
        self.sr = (self.sr & !0x3f) | ((mode << 2) & 0x3f);
        self.cause = (self.cause & !0x8000_007c) | ((cause as u32) << 2);
        self.epc = self.current_pc;
        if !matches!(cause, Exception::Interrupt | Exception::Syscall) {
            self.last_fault = Some((cause as u32, self.current_pc, self.current_instruction));
        }
        if self.delay_slot {
            self.epc = self.epc.wrapping_sub(4);
            self.cause |= 1 << 31;
        }
        // The pending loads still land: the one landing after the faulting
        // instruction, then any it issued (or, when an interrupt comes
        // before the instruction, the one in flight).
        self.wrote = 0;
        self.land();
        let (reg, val) = std::mem::take(&mut self.load);
        if reg != 0 {
            self.regs[reg] = val;
            self.out_regs[reg] = val;
        }
        self.pc = handler;
        self.next_pc = handler.wrapping_add(4);
        self.branch = false;
        self.delay_slot = false;
    }

    fn branch_to(&mut self, target: u32) {
        self.next_pc = target;
        self.branch = true;
    }

    fn branch_rel(&mut self, imm: u32) {
        // `pc` already points to the delay slot.
        self.branch_to(self.pc.wrapping_add(imm << 2));
    }

    fn execute(&mut self, i: u32, bus: &mut impl CpuBus) {
        let op = i >> 26;
        let rs = ((i >> 21) & 0x1f) as usize;
        let rt = ((i >> 16) & 0x1f) as usize;
        let rd = ((i >> 11) & 0x1f) as usize;
        let shamt = (i >> 6) & 0x1f;
        let imm = i & 0xffff;
        let imm_se = imm as i16 as u32;
        let s = self.regs[rs];
        let t = self.regs[rt];

        match op {
            0x00 => match i & 0x3f {
                0x00 => self.set_reg(rd, t << shamt),
                0x02 => self.set_reg(rd, t >> shamt),
                0x03 => self.set_reg(rd, ((t as i32) >> shamt) as u32),
                0x04 => self.set_reg(rd, t << (s & 0x1f)),
                0x06 => self.set_reg(rd, t >> (s & 0x1f)),
                0x07 => self.set_reg(rd, ((t as i32) >> (s & 0x1f)) as u32),
                0x08 => self.branch_to(s),
                0x09 => {
                    let ra = self.next_pc;
                    self.branch_to(s);
                    self.set_reg(rd, ra);
                }
                0x0c => self.exception(Exception::Syscall),
                0x0d => self.exception(Exception::Break),
                0x10 => {
                    self.stall_until(self.hilo_ready);
                    self.set_reg(rd, self.hi);
                }
                0x11 => self.hi = s,
                0x12 => {
                    self.stall_until(self.hilo_ready);
                    self.set_reg(rd, self.lo);
                }
                0x13 => self.lo = s,
                0x18 => {
                    self.start_muldiv(Self::mult_cycles(s, true));
                    let r = i64::from(s as i32) * i64::from(t as i32);
                    self.hi = (r >> 32) as u32;
                    self.lo = r as u32;
                }
                0x19 => {
                    self.start_muldiv(Self::mult_cycles(s, false));
                    let r = u64::from(s) * u64::from(t);
                    self.hi = (r >> 32) as u32;
                    self.lo = r as u32;
                }
                0x1a => {
                    self.start_muldiv(36);
                    let (n, d) = (s as i32, t as i32);
                    if d == 0 {
                        self.hi = n as u32;
                        self.lo = if n >= 0 { 0xffff_ffff } else { 1 };
                    } else if n as u32 == 0x8000_0000 && d == -1 {
                        self.hi = 0;
                        self.lo = 0x8000_0000;
                    } else {
                        self.hi = (n % d) as u32;
                        self.lo = (n / d) as u32;
                    }
                }
                0x1b => {
                    self.start_muldiv(36);
                    if t == 0 {
                        self.hi = s;
                        self.lo = 0xffff_ffff;
                    } else {
                        self.hi = s % t;
                        self.lo = s / t;
                    }
                }
                0x20 => match (s as i32).checked_add(t as i32) {
                    Some(v) => self.set_reg(rd, v as u32),
                    None => self.exception(Exception::Overflow),
                },
                0x21 => self.set_reg(rd, s.wrapping_add(t)),
                0x22 => match (s as i32).checked_sub(t as i32) {
                    Some(v) => self.set_reg(rd, v as u32),
                    None => self.exception(Exception::Overflow),
                },
                0x23 => self.set_reg(rd, s.wrapping_sub(t)),
                0x24 => self.set_reg(rd, s & t),
                0x25 => self.set_reg(rd, s | t),
                0x26 => self.set_reg(rd, s ^ t),
                0x27 => self.set_reg(rd, !(s | t)),
                0x2a => self.set_reg(rd, u32::from((s as i32) < (t as i32))),
                0x2b => self.set_reg(rd, u32::from(s < t)),
                _ => self.exception(Exception::ReservedInstruction),
            },
            0x01 => {
                // BLTZ, BGEZ, BLTZAL, BGEZAL (other rt values alias these).
                let ge = (i >> 16) & 1 != 0;
                let taken = ((s as i32) < 0) != ge;
                if (i >> 17) & 0xf == 0x8 {
                    let ra = self.next_pc;
                    self.set_reg(31, ra);
                }
                if taken {
                    self.branch_rel(imm_se);
                }
            }
            0x02 => self.branch_to((self.pc & 0xf000_0000) | ((i & 0x3ff_ffff) << 2)),
            0x03 => {
                let ra = self.next_pc;
                self.branch_to((self.pc & 0xf000_0000) | ((i & 0x3ff_ffff) << 2));
                self.set_reg(31, ra);
            }
            0x04 => {
                if s == t {
                    self.branch_rel(imm_se);
                }
            }
            0x05 => {
                if s != t {
                    self.branch_rel(imm_se);
                }
            }
            0x06 => {
                if (s as i32) <= 0 {
                    self.branch_rel(imm_se);
                }
            }
            0x07 => {
                if (s as i32) > 0 {
                    self.branch_rel(imm_se);
                }
            }
            0x08 => match (s as i32).checked_add(imm_se as i32) {
                Some(v) => self.set_reg(rt, v as u32),
                None => self.exception(Exception::Overflow),
            },
            0x09 => self.set_reg(rt, s.wrapping_add(imm_se)),
            0x0a => self.set_reg(rt, u32::from((s as i32) < (imm_se as i32))),
            0x0b => self.set_reg(rt, u32::from(s < imm_se)),
            0x0c => self.set_reg(rt, s & imm),
            0x0d => self.set_reg(rt, s | imm),
            0x0e => self.set_reg(rt, s ^ imm),
            0x0f => self.set_reg(rt, imm << 16),
            0x10 => self.cop0(i, rs, rt, rd),
            0x12 => self.cop2(i, rs, rt, rd, bus),
            0x11 | 0x13 => self.exception(Exception::CoprocessorUnusable),
            0x20..=0x26 => self.load_op(op, s.wrapping_add(imm_se), rt, bus),
            0x28..=0x2e => self.store_op(op, s.wrapping_add(imm_se), t, bus),
            0x32 => {
                // LWC2
                let addr = s.wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.bad_vaddr = addr;
                    self.exception(Exception::LoadAddressError);
                } else {
                    self.extra += data_access_cycles(addr);
                    let v = bus.load32(addr);
                    self.gte.write_data(rt as u32, v);
                }
            }
            0x3a => {
                // SWC2
                let addr = s.wrapping_add(imm_se);
                if addr & 3 != 0 {
                    self.bad_vaddr = addr;
                    self.exception(Exception::StoreAddressError);
                } else if self.sr & 0x1_0000 == 0 {
                    self.stall_until(self.gte_ready);
                    bus.store32(addr, self.gte.read_data(rt as u32));
                }
            }
            0x30 | 0x31 | 0x33 | 0x38 | 0x39 | 0x3b => self.exception(Exception::CoprocessorUnusable),
            _ => self.exception(Exception::ReservedInstruction),
        }
    }

    fn load_op(&mut self, op: u32, addr: u32, rt: usize, bus: &mut impl CpuBus) {
        self.extra += data_access_cycles(addr);
        let align = match op {
            0x21 | 0x25 => 1,
            0x23 => 3,
            _ => 0,
        };
        if addr & align != 0 {
            self.bad_vaddr = addr;
            self.exception(Exception::LoadAddressError);
            return;
        }
        let v = match op {
            0x20 => bus.load8(addr) as i8 as u32,
            0x21 => bus.load16(addr) as i16 as u32,
            0x23 => bus.load32(addr),
            0x24 => u32::from(bus.load8(addr)),
            0x25 => u32::from(bus.load16(addr)),
            0x22 | 0x26 => {
                // LWL/LWR merge with the value of a load still in flight.
                let cur = if self.landing.0 == rt {
                    self.landing.1
                } else {
                    self.regs[rt]
                };
                let word = bus.load32(addr & !3);
                let shift = (addr & 3) * 8;
                if op == 0x22 {
                    let keep = 0x00ff_ffff_u32.checked_shr(shift).unwrap_or(0);
                    (cur & keep) | (word << (24 - shift))
                } else {
                    let keep = 0xffff_ff00_u32.checked_shl(24 - shift).unwrap_or(0);
                    (cur & keep) | (word >> shift)
                }
            }
            _ => unreachable!(),
        };
        self.load = (rt, v);
        // A load to r0 is discarded.
        if rt == 0 {
            self.load = (0, 0);
        }
    }

    fn store_op(&mut self, op: u32, addr: u32, t: u32, bus: &mut impl CpuBus) {
        let align = match op {
            0x29 => 1,
            0x2b => 3,
            _ => 0,
        };
        if addr & align != 0 {
            self.bad_vaddr = addr;
            self.exception(Exception::StoreAddressError);
            return;
        }
        if let Some(w) = self.watch
            && (addr & 0x1fff_fffc) == w
        {
            self.watched.push((self.current_pc, addr, t));
        }
        // With the cache isolated (used by the BIOS to flush it), stores go
        // to the cache only.
        if self.sr & 0x1_0000 != 0 {
            self.icache[((addr >> 4) & 0xff) as usize] = 0;
            return;
        }
        match op {
            0x28 => bus.store8(addr, t as u8),
            0x29 => bus.store16(addr, t as u16),
            0x2b => bus.store32(addr, t),
            0x2a | 0x2e => {
                let aligned = addr & !3;
                let cur = bus.load32(aligned);
                let shift = (addr & 3) * 8;
                let v = if op == 0x2a {
                    let keep = 0xffff_ff00_u32.checked_shl(shift).unwrap_or(0);
                    (cur & keep) | (t >> (24 - shift))
                } else {
                    let keep = 0x00ff_ffff_u32.checked_shr(24 - shift).unwrap_or(0);
                    (cur & keep) | (t << shift)
                };
                bus.store32(aligned, v);
            }
            _ => unreachable!(),
        }
    }

    fn cop0(&mut self, i: u32, rs: usize, rt: usize, rd: usize) {
        match rs {
            0x00 => {
                let v = match rd {
                    3 | 5 | 7 | 9 | 11 => self.debug[debug_index(rd)],
                    // JUMPDEST: where the last jump went, for breakpoints.
                    6 => 0,
                    8 => self.bad_vaddr,
                    12 => self.sr,
                    13 => self.cause,
                    14 => self.epc,
                    15 => self.prid,
                    _ => {
                        self.exception(Exception::ReservedInstruction);
                        return;
                    }
                };
                self.load = (rt, v);
            }
            0x04 => {
                let v = self.regs[rt];
                match rd {
                    3 | 5 | 7 | 9 | 11 => self.debug[debug_index(rd)] = v,
                    12 => self.sr = v,
                    // Only the two software interrupt bits are writable.
                    13 => self.cause = (self.cause & !0x300) | (v & 0x300),
                    _ => {}
                }
            }
            0x10 if i & 0x3f == 0x10 => {
                // RFE: pop the interrupt-enable/user-mode stack.
                let mode = self.sr & 0x3f;
                self.sr = (self.sr & !0xf) | (mode >> 2);
            }
            _ => self.exception(Exception::ReservedInstruction),
        }
    }

    pub fn debug_registers(&self) -> [u32; 5] {
        self.debug
    }

    pub fn set_debug_registers(&mut self, registers: [u32; 5]) {
        self.debug = registers;
    }

    fn cop2(&mut self, i: u32, rs: usize, rt: usize, rd: usize, bus: &mut impl CpuBus) {
        if self.sr & (1 << 30) == 0 {
            self.exception(Exception::CoprocessorUnusable);
            return;
        }
        if i & (1 << 25) != 0 {
            self.gte_command(i & 0x1ff_ffff, bus);
            return;
        }
        if matches!(rs, 0x00 | 0x02) {
            self.stall_until(self.gte_ready);
        }
        match rs {
            0x00 => self.load = (rt, self.gte.read_data(rd as u32)),
            0x02 => self.load = (rt, self.gte.read_control(rd as u32)),
            0x04 => self.gte.write_data(rd as u32, self.regs[rt]),
            0x06 => self.gte.write_control(rd as u32, self.regs[rt]),
            _ => self.exception(Exception::ReservedInstruction),
        }
        if self.load.0 == 0 {
            self.load = (0, 0);
        }
    }
}

/// Where a debug register (3, 5, 7, 9, 11) is kept in `Cpu::debug`.
fn debug_index(rd: usize) -> usize {
    match rd {
        3 => 0,
        5 => 1,
        7 => 2,
        9 => 3,
        _ => 4,
    }
}
