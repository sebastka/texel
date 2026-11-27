//! Instruction tests: small programs placed in the BIOS.

use super::*;

/// Runs `program` from the reset vector, `steps` instructions.
fn run(program: &[u32], steps: usize, setup: impl FnOnce(&mut Ee, &mut Bus)) -> (Ee, Bus) {
    let rom: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut bus = Bus::new(&rom);
    let mut ee = Ee::new();
    setup(&mut ee, &mut bus);
    for _ in 0..steps {
        ee.step(&mut bus);
    }
    (ee, bus)
}

fn r(op: u32, rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}

fn imm(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | u32::from(imm)
}

#[test]
fn arithmetic_sign_extends() {
    // LUI r1, 0x8000; ADDIU r2, r1, -1; DADDU r3, r1, r1
    let (ee, _) = run(
        &[imm(0x0F, 0, 1, 0x8000), imm(0x09, 1, 2, 0xFFFF), r(0, 1, 1, 3, 0, 0x2D)],
        3,
        |_, _| {},
    );
    assert_eq!(ee.gpr[1] as u64, 0xFFFF_FFFF_8000_0000);
    assert_eq!(ee.gpr[2] as u64, 0x7FFF_FFFF);
    assert_eq!(ee.gpr[3] as u64, 0xFFFF_FFFF_0000_0000);
}

#[test]
fn branch_delay_slot_and_likely() {
    // BEQ r0, r0, +2; ORI r1, r0, 1 (delay slot); ORI r2, r0, 2 (skipped);
    // BNEL r0, r0, +1; ORI r3, r0, 3 (nullified); ORI r4, r0, 4
    let (ee, _) = run(
        &[
            imm(0x04, 0, 0, 2),
            imm(0x0D, 0, 1, 1),
            imm(0x0D, 0, 2, 2),
            imm(0x15, 0, 0, 1),
            imm(0x0D, 0, 3, 3),
            imm(0x0D, 0, 4, 4),
        ],
        4,
        |_, _| {},
    );
    assert_eq!(ee.gpr[1], 1);
    assert_eq!(ee.gpr[2], 0);
    assert_eq!(ee.gpr[3], 0);
    assert_eq!(ee.gpr[4], 4);
}

#[test]
fn mult_writes_rd_and_div_by_zero() {
    // MULT r3, r1, r2; DIV r1, r0
    let (ee, _) = run(&[r(0, 1, 2, 3, 0, 0x18), r(0, 1, 0, 0, 0, 0x1A)], 2, |ee, _| {
        ee.gpr[1] = 0x1_0000;
        ee.gpr[2] = (-0x1_0000i64) as u64 as u128;
    });
    assert_eq!(ee.gpr[3] as u64, 0);
    assert_eq!(ee.lo as u64, u64::MAX);
    assert_eq!(ee.hi as u64, 0x1_0000);
}

#[test]
fn unaligned_loads_and_stores() {
    // LWL r1, 3(r0)... from the bus at 0x1000: LWR r1, 0x1001; LWL r1, 0x1004
    let (ee, bus) = run(
        &[
            imm(0x26, 4, 1, 0x1001),
            imm(0x22, 4, 1, 0x1004),
            imm(0x2E, 4, 2, 0x2002),
            imm(0x2A, 4, 2, 0x2005),
        ],
        4,
        |ee, bus| {
            bus.write64(0x1000, 0x8877_6655_4433_2211);
            ee.gpr[2] = 0xAABB_CCDD;
        },
    );
    // Bytes 0x1001..=0x1004.
    assert_eq!(ee.gpr[1] as u32, 0x5544_3322);
    let mut b = Bus::new(&[]);
    let _ = &mut b;
    let mut bus = bus;
    // SWR at 0x2002 then SWL at 0x2005: 0xAABBCCDD at 0x2002..=0x2005.
    assert_eq!(bus.read32(0x2000) >> 16, 0xCCDD);
    assert_eq!(bus.read32(0x2004) & 0xFFFF, 0xAABB);
}

#[test]
fn quadword_loads_and_mmi() {
    // LQ r1, 0x100; LQ r2, 0x110; PEXTLW r3, r1, r2; PCPYUD r4, r1, r2; POR r5, r1, r2
    let (ee, _) = run(
        &[
            imm(0x1E, 0, 1, 0x100),
            imm(0x1E, 0, 2, 0x110),
            r(0x1C, 1, 2, 3, 0x12, 0x08),
            r(0x1C, 1, 2, 4, 0x0E, 0x29),
            r(0x1C, 1, 2, 5, 0x12, 0x29),
        ],
        5,
        |_, bus| {
            bus.write128(0x100, 0x4444_4444_3333_3333_2222_2222_1111_1111);
            bus.write128(0x110, 0x8888_8888_7777_7777_6666_6666_5555_5555);
        },
    );
    assert_eq!(ee.gpr[3], 0x2222_2222_6666_6666_1111_1111_5555_5555);
    assert_eq!(ee.gpr[4], 0x8888_8888_7777_7777_4444_4444_3333_3333);
    assert_eq!(
        ee.gpr[5],
        0xCCCC_CCCC_7777_7777_6666_6666_5555_5555 | 0x4444_4444_3333_3333_2222_2222_1111_1111
    );
}

#[test]
fn syscall_and_eret() {
    // SYSCALL at the reset vector, with BEV: to 0xBFC00380; EPC is the SYSCALL.
    let (mut ee, mut bus) = run(&[r(0, 0, 0, 0, 0, 0x0C)], 1, |_, _| {});
    assert_eq!(ee.pc, 0xBFC0_0380);
    assert_eq!(ee.cop0.regs[cop0::EPC], RESET_VECTOR);
    assert_eq!((ee.cop0.regs[cop0::CAUSE] >> 2) & 31, 8);
    // ERET: ERL is still set from reset, so back to ErrorEPC; clear it first.
    ee.cop0.regs[cop0::STATUS] &= !cop0::STATUS_ERL;
    ee.cop0.regs[cop0::EPC] = 0xBFC0_0004;
    ee.execute(r(0x10, 0x10, 0, 0, 0, 0x18), &mut bus);
    assert_eq!(ee.pc, 0xBFC0_0004);
    assert!(!ee.cop0.exl());
}

#[test]
fn interrupts_need_ie_and_eie() {
    let (mut ee, _) = run(&[0; 4], 1, |_, _| {});
    ee.cop0.regs[cop0::STATUS] = cop0::STATUS_IE | (1 << 10);
    ee.check_interrupts(true, false);
    assert_eq!(ee.pc, RESET_VECTOR + 4);
    ee.cop0.regs[cop0::STATUS] |= cop0::STATUS_EIE;
    ee.check_interrupts(true, false);
    assert_eq!(ee.pc, 0x8000_0200);
    assert_eq!(ee.cop0.regs[cop0::EPC], RESET_VECTOR + 4);
}

#[test]
fn fpu_saturates() {
    // MTC1 r1, f0; MUL.S f1, f0, f0; CVT.W.S f2, f1
    let (ee, _) = run(
        &[
            r(0x11, 0x04, 1, 0, 0, 0),
            r(0x11, 0x10, 0, 0, 1, 0x02),
            r(0x11, 0x10, 0, 1, 2, 0x24),
        ],
        3,
        |ee, _| ee.gpr[1] = u128::from(f32::MAX.to_bits()),
    );
    assert_eq!(ee.fpu.fpr[1], 0x7F7F_FFFF);
    assert_eq!(ee.fpu.fpr[2], i32::MAX as u32);
}

#[test]
fn tlb_refill_and_modified_exceptions() {
    // SW r1, 0(r2); with r2 = 0x40000000, which nothing maps.
    let (ee, _) = run(&[imm(0x2B, 2, 1, 0)], 1, |ee, _| {
        ee.cop0.regs[cop0::STATUS] = 0;
        ee.gpr[2] = 0x4000_0000;
    });
    // To the refill vector, as a store.
    assert_eq!(ee.pc, 0x8000_0000);
    assert_eq!((ee.cop0.regs[cop0::CAUSE] >> 2) & 31, 3);
    assert_eq!(ee.cop0.regs[cop0::BAD_VADDR], 0x4000_0000);
    assert_eq!(ee.cop0.regs[cop0::EPC], RESET_VECTOR);

    // A read-only page: the Modified exception, to the common vector.
    let (ee, bus) = run(&[imm(0x2B, 2, 1, 0)], 1, |ee, _| {
        let c = &mut ee.cop0;
        c.regs[cop0::STATUS] = 0;
        c.regs[cop0::INDEX] = 0;
        c.regs[cop0::ENTRY_HI] = 0x4000_0000;
        c.regs[cop0::ENTRY_LO0] = (0x10 << 6) | 0b011;
        c.regs[cop0::ENTRY_LO1] = 0b011;
        c.tlbwi();
        ee.gpr[2] = 0x4000_0000;
        ee.gpr[1] = 0x1234;
    });
    assert_eq!(ee.pc, 0x8000_0180);
    assert_eq!((ee.cop0.regs[cop0::CAUSE] >> 2) & 31, 1);
    let mut bus = bus;
    assert_eq!(bus.read32(0x1_0000), 0);
}

#[test]
fn multiply_accumulate_mmi() {
    // MMI2 (op 0x1C, funct 0x09): PMADDW (sa 0x00), PHMADH (sa 0x11),
    // PDIVBW (sa 0x1D).
    let mmi2 = |rs, rt, rd, sa| r(0x1C, rs, rt, rd, sa, 0x09);
    let (ee, _) = run(
        &[mmi2(1, 2, 3, 0x00), mmi2(1, 2, 4, 0x11), mmi2(5, 6, 0, 0x1D)],
        3,
        |ee, _| {
            // Words 0 and 2: 3 × -4 and 100000 × 100000, plus HI:LO (10 and 0).
            ee.gpr[1] = (100_000u128 << 64) | 3;
            ee.gpr[2] = (100_000u128 << 64) | (-4i32 as u32 as u128);
            ee.lo = 10;
            ee.gpr[5] =
                u128::from_le_bytes(std::array::from_fn(|k| [7u8, 0, 0, 0][k % 4])) | (u128::from(u32::MAX) << 96);
            ee.gpr[6] = 2;
        },
    );
    // PMADDW: 10 + 3 × -4 = -2; 10^10 in the upper doubleword.
    assert_eq!(ee.gpr[3] as u64, -2i64 as u64);
    assert_eq!((ee.gpr[3] >> 64) as u64, 10_000_000_000);
    // PHMADH, words 0: rs's halves (3, 0) by rt's (-4, -1), summed.
    assert_eq!(ee.gpr[4] as u32, -12i32 as u32);
    // PDIVBW: 7 / 2 in each word (but the last: -1 / 2), quotients in LO,
    // remainders in HI.
    assert_eq!(ee.lo as u32, 3);
    assert_eq!(ee.hi as u32, 1);
    assert_eq!((ee.lo >> 96) as u32, 0);
    assert_eq!((ee.hi >> 96) as u32, -1i32 as u32);
}

/// DI's write lands an instruction late: the next instruction still reads
/// EIE set (and writing that back keeps interrupts enabled, as the kernel
/// does when it starts a program).
#[test]
fn di_lands_an_instruction_late() {
    let di = 0x4200_0039;
    // MFC0 r1, Status (rd 12); then a NOP.
    let mfc0 = (0x10 << 26) | (1 << 16) | (12 << 11);
    let (ee, _) = run(&[di, mfc0, 0], 2, |ee, _| {
        ee.cop0.regs[cop0::STATUS] = cop0::STATUS_EIE;
    });
    assert_ne!(ee.gpr[1] as u32 & cop0::STATUS_EIE, 0);
    assert_eq!(ee.cop0.regs[cop0::STATUS] & cop0::STATUS_EIE, 0);
    // In user mode without EDI, DI does nothing.
    let (ee, _) = run(&[di, 0, 0], 2, |ee, _| {
        ee.cop0.regs[cop0::STATUS] = cop0::STATUS_EIE | 2 << 3;
    });
    assert_ne!(ee.cop0.regs[cop0::STATUS] & cop0::STATUS_EIE, 0);
}
