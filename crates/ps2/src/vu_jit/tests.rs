//! The compiler's output is valid WebAssembly, for every instruction, and
//! records the instructions it compiled. (That compiled programs run as the
//! interpreter does is checked in WebAssembly itself, on a game:
//! frontend/scripts/check-ps2-vu.mjs.)

use super::compile;

const NOP: u32 = 0x2FF;
const LNOP: u32 = 0x8000_033C;
const E: u32 = 1 << 30;

/// Micro memory holding (upper, lower) pairs from 0.
fn micro(prog: &[(u32, u32)]) -> Vec<u8> {
    let mut m = vec![0u8; 16 * 1024];
    for (k, (u, l)) in prog.iter().enumerate() {
        m[k * 8..k * 8 + 4].copy_from_slice(&l.to_le_bytes());
        m[k * 8 + 4..k * 8 + 8].copy_from_slice(&u.to_le_bytes());
    }
    m
}

fn compiled(prog: &[(u32, u32)]) -> super::Compiled {
    let c = compile(&micro(prog), 0, 1024, 7).expect("a program");
    let mut v = wasmparser::Validator::new();
    if let Err(e) = v.validate_all(&c.module) {
        panic!("invalid module: {e}");
    }
    c
}

/// An upper instruction: funct with fd 3, fs 1, ft 2, all lanes.
fn up(funct: u32) -> u32 {
    (0xF << 21) | (2 << 16) | (1 << 11) | (3 << 6) | funct
}

/// A special-table upper instruction (op 0x00..0x7F).
fn up_special(op: u32) -> u32 {
    (0xF << 21) | (2 << 16) | (1 << 11) | ((op >> 2) << 6) | 0x3C | (op & 3)
}

/// A special-table lower instruction.
fn low_special(op: u32, it: u32, is: u32) -> u32 {
    (0x40 << 25) | (0xF << 21) | (it << 16) | (is << 11) | ((op >> 2) << 6) | 0x3C | (op & 3)
}

#[test]
fn every_upper_instruction() {
    let mut prog = Vec::new();
    for funct in 0..0x30 {
        prog.push((up(funct), LNOP));
    }
    for op in 0..0x30 {
        prog.push((up_special(op), LNOP));
    }
    // Partial masks, ACC, and VF0 as the destination.
    prog.push(((0x5 << 21) | (2 << 16) | (1 << 11) | 0x28, LNOP));
    prog.push(((0x8 << 21) | (2 << 16) | (1 << 11) | 0x2A, LNOP));
    // The I bit: the lower word is I.
    prog.push((up(0x1E) | (1 << 31), 0x3F80_0000));
    prog.push((NOP | E, LNOP));
    prog.push((NOP, LNOP));
    let c = compiled(&prog);
    assert_eq!(c.words.len(), prog.len());
}

#[test]
fn every_lower_instruction() {
    let i = |op: u32, it: u32, is: u32, imm: u32| (op << 25) | (0xF << 21) | (it << 16) | (is << 11) | (imm & 0x7FF);
    let mut prog = vec![
        (NOP, i(0x00, 2, 1, 4)),
        (NOP, i(0x01, 2, 1, 0x7FF)),
        (NOP, i(0x04, 2, 1, 4)),
        (NOP, i(0x05, 2, 1, 4)),
        (NOP, i(0x08, 2, 1, 5)),
        (NOP, i(0x09, 2, 1, 5)),
    ];
    for op in [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x1A, 0x1B, 0x1C] {
        prog.push((NOP, i(op, 2, 1, 0x123)));
    }
    for funct in 0x30..=0x35 {
        prog.push((NOP, (0x40 << 25) | (2 << 16) | (1 << 11) | (3 << 6) | funct));
    }
    for op in [
        0x30, 0x31, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43,
        0x64, 0x68, 0x69, 0x6C, 0x70, 0x78, 0x7C, 0x7E,
    ] {
        prog.push((NOP, low_special(op, 2, 1)));
        // VF0 and VI0 as destinations.
        prog.push((NOP, low_special(op, 0, 0)));
    }
    prog.push((NOP | E, LNOP));
    prog.push((NOP, LNOP));
    let c = compiled(&prog);
    assert_eq!(c.words.len(), prog.len());
}

#[test]
fn loops_branches_and_subroutines() {
    let iaddiu = |it: u32, is: u32, imm: u32| (0x08 << 25) | (it << 16) | (is << 11) | imm;
    let isubiu = |it: u32, is: u32, imm: u32| (0x09 << 25) | (it << 16) | (is << 11) | imm;
    let ibne = |it: u32, is: u32, off: i32| (0x29 << 25) | (it << 16) | (is << 11) | (off as u32 & 0x7FF);
    let bal = |it: u32, off: i32| (0x21 << 25) | (it << 16) | (off as u32 & 0x7FF);
    let jr = |is: u32| (0x24 << 25) | (is << 11);
    let prog = [
        (NOP, iaddiu(1, 0, 3)),
        // 1: loop
        (NOP, isubiu(1, 1, 1)),
        (NOP, ibne(1, 0, -2)),
        (NOP, LNOP),
        // 4: call 8, returning to 6
        (NOP, bal(15, 3)),
        (NOP, LNOP),
        (NOP | E, LNOP),
        (NOP, LNOP),
        // 8: the subroutine
        (NOP, iaddiu(2, 2, 1)),
        (NOP, jr(15)),
        (NOP, LNOP),
    ];
    let c = compiled(&prog);
    // Every instruction compiled, some in two blocks.
    let mut pcs: Vec<u32> = c.words.iter().map(|&(pc, _)| pc).collect();
    pcs.sort_unstable();
    pcs.dedup();
    assert_eq!(pcs, (0..11).collect::<Vec<_>>());
}

#[test]
fn rare_cases_stop_for_the_interpreter() {
    let b = |off: i32| (0x20 << 25) | (off as u32 & 0x7FF);
    // A branch in a delay slot, an E bit on a branch: not compiled.
    let c = compiled(&[(NOP, LNOP), (NOP, b(2)), (NOP, b(2)), (NOP, LNOP)]);
    assert_eq!(c.words.len(), 1);
    let c = compiled(&[(NOP | E, b(2)), (NOP, LNOP)]);
    assert!(c.words.is_empty());
}
