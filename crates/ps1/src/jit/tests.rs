//! Compiled blocks are valid WebAssembly, whatever the instructions (that
//! they run as interpreted is checked in a browser's engine: see
//! `frontend/scripts/check-ps1-compiler.mjs`).

use super::compile::{Layout, batch, compile};
use crate::bus::Bus;
use crate::cpu::Cpu;

fn check(pc: u32, words: &[u32], icache: bool) -> u32 {
    let cpu = Cpu::new();
    let bus = Bus::new(vec![0; 512 * 1024].into_boxed_slice());
    let layout = Layout::new(&cpu, &bus);
    let ram = (pc & 0x1fff_ffff < 0x0080_0000).then_some((pc & 0x1f_ffff, (pc & 0x1f_ffff) + 4 * words.len() as u32));
    let c = compile(pc, words, icache, ram, 7, &layout).expect("a block");
    let mut v = wasmparser::Validator::new();
    // Alone, and in a batch with others.
    for module in [batch(&[&c.code]), batch(&[&c.code, &c.code, &c.code])] {
        if let Err(e) = v.validate_all(&module) {
            panic!("invalid module: {e}");
        }
        v = wasmparser::Validator::new();
    }
    c.len
}

/// R-type `funct` rd, rs, rt.
fn r(funct: u32, rd: u32, rs: u32, rt: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (3 << 6) | funct
}

fn i(op: u32, rt: u32, rs: u32, imm: u32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm & 0xffff)
}

#[test]
fn every_kind_of_instruction() {
    let mut words = Vec::new();
    for funct in [
        0x00, 0x02, 0x03, 0x04, 0x06, 0x07, 0x10, 0x11, 0x12, 0x13, 0x18, 0x19, 0x1a, 0x1b, 0x20, 0x21, 0x22, 0x23,
        0x24, 0x25, 0x26, 0x27, 0x2a, 0x2b,
    ] {
        words.push(r(funct, 8, 9, 10));
        words.push(r(funct, 0, 9, 10));
    }
    for op in [0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f] {
        words.push(i(op, 8, 9, 0x8123));
    }
    // Loads, each followed by a use of what it loads (the delay slot).
    for op in [0x20, 0x21, 0x23, 0x24, 0x25] {
        words.push(i(op, 8, 29, 16));
        words.push(r(0x21, 9, 8, 8));
        words.push(i(op, 8, 29, 20));
        words.push(i(op, 8, 29, 24));
    }
    for op in [0x28, 0x29, 0x2b] {
        words.push(i(op, 8, 29, 0xfffc));
    }
    // GTE: a command, MTC2, then MFC2 (which stops the block).
    words.push((0x12 << 26) | (1 << 25) | 0x30);
    words.push((0x12 << 26) | (0x04 << 21) | (8 << 16) | (1 << 11));
    assert!(check(0x8001_0000, &words, true) >= 50);
    assert_eq!(check(0xa001_0000, &words[..20], false), 20);
}

#[test]
fn branches_end_with_their_delay_slot() {
    let lw = i(0x23, 8, 29, 0);
    for b in [
        i(0x04, 8, 9, 0xfff0),
        i(0x05, 8, 9, 3),
        i(0x06, 0, 9, 3),
        i(0x07, 0, 9, 3),
        i(0x01, 0x00, 9, 3),
        i(0x01, 0x11, 9, 3),
        (0x02 << 26) | 0x40,
        (0x03 << 26) | 0x40,
        r(0x08, 0, 31, 0),
        r(0x09, 31, 8, 0),
    ] {
        // Jumps end blocks; conditional branches do not (the block leaves
        // when they are taken).
        let jump = matches!(b >> 26, 0x00 | 0x02 | 0x03);
        // A load before the branch (landing after it), and in its slot.
        assert_eq!(check(0x8001_0000, &[lw, b, lw, 0, 0], true), if jump { 3 } else { 5 });
        // A slot left to the interpreter.
        assert_eq!(
            check(0xbfc0_0100, &[b, r(0x18, 0, 8, 9), 0], true),
            if jump { 2 } else { 3 }
        );
    }
    // A branch in a delay slot: the block ends before the first.
    assert_eq!(check(0x8001_0000, &[0, i(0x04, 0, 0, 1), i(0x04, 0, 0, 1)], true), 1);
    // A branch back to the block's start ends it (a loop), as does a slot
    // the block stops after.
    assert_eq!(check(0x8001_0000, &[lw, i(0x05, 8, 0, 0xfffe), 0, 0], true), 3);
    assert_eq!(check(0x8001_0000, &[i(0x05, 8, 0, 3), r(0x0c, 0, 0, 0), 0], true), 2);
    // Several conditional branches in a block, up to a jump.
    let words = [i(0x05, 8, 0, 3), 0, i(0x04, 8, 9, 5), lw, 0, r(0x08, 0, 31, 0), 0, 0];
    assert_eq!(check(0x8001_0000, &words, true), 7);
}
