//! The compiler's output is valid WebAssembly, and its blocks end where
//! they should. (That compiled blocks run as the interpreter does is checked
//! in WebAssembly itself: frontend/scripts/check-ps2-compiler.mjs.)

use super::compile::{Layout, Source, compile, idle_candidate};

const VA: u32 = 0x0010_0000;

fn r(rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}

fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | u32::from(imm)
}

/// A branch from word `from` to word `to` of the block.
fn rel(from: usize, to: usize) -> u16 {
    (to as i32 - from as i32 - 1) as i16 as u16
}

fn compiled(words: Vec<u32>) -> super::Compiled {
    compiled_with(words, Vec::new())
}

fn compiled_with(words: Vec<u32>, idle: Vec<u32>) -> super::Compiled {
    let layout = Layout {
        pages: 0x10_0000,
        ram: 0x20_0000,
        scratchpad: 0x28_0000,
        code_lines: 0x30_0000,
        stale_offset: 0x100,
        status_offset: 0x104,
    };
    let c = compile(&Source { va: VA, words, idle }, 7, layout).expect("a block");
    let mut v = wasmparser::Validator::new();
    if let Err(e) = v.validate_all(&c.module) {
        panic!("invalid module: {e}");
    }
    c
}

#[test]
fn integer_instructions() {
    let mut words = Vec::new();
    for funct in [
        0x00, 0x02, 0x03, 0x04, 0x06, 0x07, 0x0A, 0x0B, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x16, 0x17, 0x20, 0x21,
        0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x38, 0x3A, 0x3B, 0x3C, 0x3E, 0x3F,
    ] {
        words.push(r(4, 5, 6, 7, funct));
    }
    for op in [0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x18, 0x19, 0x2F, 0x33] {
        words.push(i(op, 4, 5, 0x8001));
    }
    // Writes to r0 compile to nothing.
    words.push(r(4, 5, 0, 0, 0x21));
    let n = words.len() as u32;
    let c = compiled(words);
    // It starts with one instruction left: it can stop after any.
    assert_eq!((c.words, c.native, c.first), (n, n, 1));
}

#[test]
fn loads_stores_and_the_interpreter() {
    let words = vec![
        i(0x20, 29, 8, 4),
        i(0x21, 29, 8, 4),
        i(0x23, 29, 8, 4),
        i(0x24, 29, 8, 4),
        i(0x25, 29, 8, 4),
        i(0x27, 29, 8, 4),
        i(0x37, 29, 8, 8),
        i(0x1E, 29, 8, 16),
        i(0x28, 29, 8, 4),
        i(0x29, 29, 8, 4),
        i(0x2B, 29, 8, 4),
        i(0x3F, 29, 8, 8),
        i(0x1F, 29, 8, 16),
        // A load to r0: the interpreter's.
        i(0x23, 29, 0, 4),
        // MULT and PMADDH: the interpreter's.
        r(4, 5, 6, 0, 0x18),
        (0x1C << 26) | r(4, 5, 6, 0x10, 0x09),
    ];
    let c = compiled(words);
    // The loads and stores compile (with the interpreter for slow paths).
    assert_eq!((c.words, c.native), (16, 13));
}

#[test]
fn fpu_and_vu0_moves() {
    let cop = |op: u32, rs: u32, rt: u32, rd: u32| (op << 26) | (rs << 21) | (rt << 16) | (rd << 11);
    let words = vec![
        // LWC1, SWC1, LQC2, SQC2.
        i(0x31, 29, 3, 4),
        i(0x39, 29, 3, 4),
        i(0x36, 29, 3, 16),
        i(0x3E, 29, 3, 16),
        // MFC1, MTC1, QMFC2, QMTC2.
        cop(0x11, 0x00, 8, 3),
        cop(0x11, 0x04, 8, 3),
        cop(0x12, 0x01, 8, 3),
        cop(0x12, 0x05, 8, 3),
        // To r0 and VF0: nothing, or the interpreter's for LQC2.
        cop(0x12, 0x01, 0, 3),
        cop(0x12, 0x05, 8, 0),
        i(0x36, 29, 0, 16),
        // CFC2 and VCALLMS: the interpreter's.
        cop(0x12, 0x02, 8, 3),
        cop(0x12, 0x10, 0, 0) | 0x38,
    ];
    let c = compiled(words);
    assert_eq!((c.words, c.native), (13, 10));
}

#[test]
fn vu0_macro_instructions() {
    // COP2 with the CO bit: fd 3, fs 1, ft 2, all lanes.
    let base = (0x12 << 26) | (0x1F << 21) | (2 << 16) | (1 << 11);
    let vop = |funct: u32| base | (3 << 6) | funct;
    let special = |op: u32| base | ((op >> 2) << 6) | 0x3C | (op & 3);
    let mut words: Vec<u32> = (0..0x30).map(vop).collect();
    let uppers: Vec<u32> = (0..0x30).filter(|&op| op != 0x2B && op != 0x2F).collect();
    words.extend(uppers.iter().map(|&op| special(op)));
    // Its NOP and the integer and lower ones: the interpreter's.
    words.extend([special(0x2F), vop(0x30), special(0x30)]);
    // In blocks of at most `MAX_LEN`.
    let (mut total, mut native) = (0, 0);
    for chunk in words.chunks(32) {
        let c = compiled(chunk.to_vec());
        assert_eq!(c.words, chunk.len() as u32);
        (total, native) = (total + c.words, native + c.native);
    }
    assert_eq!((total, native), (words.len() as u32, words.len() as u32 - 3));
}

#[test]
fn mmi_instructions() {
    let mmi = |funct: u32, sa: u32| (0x1C << 26) | r(4, 5, 6, sa, funct);
    let mut words = Vec::new();
    for sa in [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
        0x19, 0x1A, 0x1B,
    ] {
        words.push(mmi(0x08, sa));
    }
    for sa in [0x02, 0x03, 0x06, 0x07, 0x0A, 0x12, 0x14, 0x15, 0x16, 0x18, 0x19, 0x1A] {
        words.push(mmi(0x28, sa));
    }
    for sa in [0x0E, 0x12, 0x13] {
        words.push(mmi(0x09, sa));
        words.push(mmi(0x29, sa));
    }
    for funct in [0x34, 0x36, 0x37, 0x3C, 0x3E, 0x3F] {
        words.push(mmi(funct, 7));
    }
    let n = words.len() as u32;
    // PADDSW, PMADDH, PMFHL: the interpreter's.
    words.extend([mmi(0x08, 0x10), mmi(0x09, 0x10), mmi(0x30, 0)]);
    let c = compiled(words);
    assert_eq!((c.words, c.native), (n + 3, n));
}

#[test]
fn fpu_instructions() {
    let single = |funct: u32| (0x11 << 26) | (0x10 << 21) | (2 << 16) | (1 << 11) | (3 << 6) | funct;
    let mut words: Vec<u32> = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x16, 0x18, 0x19, 0x1A, 0x1C, 0x1D, 0x1E, 0x1F, 0x24, 0x30,
        0x32, 0x34, 0x36,
    ]
    .map(single)
    .to_vec();
    // CVT.S.W.
    words.push((0x11 << 26) | (0x14 << 21) | (1 << 11) | (3 << 6) | 0x20);
    let n = words.len() as u32;
    // MAX, MIN: the interpreter's.
    words.extend([single(0x28), single(0x29)]);
    let c = compiled(words);
    assert_eq!((c.words, c.native), (n + 2, n));
}

#[test]
fn every_branch() {
    let nop = 0;
    let branches = [
        (0x02 << 26) | ((VA >> 2) & 0x03FF_FFFF),
        (0x03 << 26) | ((VA >> 2) & 0x03FF_FFFF),
        r(31, 0, 0, 0, 0x08),
        r(4, 0, 31, 0, 0x09),
    ];
    for b in branches {
        // A jump ends the block, after its delay slot.
        let c = compiled(vec![r(4, 5, 6, 0, 0x21), b, r(4, 5, 7, 0, 0x21), nop, nop]);
        assert_eq!((c.words, c.first), (3, 1));
    }
    for op in [0x04, 0x05, 0x06, 0x07, 0x14, 0x15, 0x16, 0x17] {
        let words = vec![i(op, 4, 5, rel(0, 3)), nop, nop, nop];
        // Conditional: the block goes on past it.
        let c = compiled(words);
        // A branch first: it runs with its delay slot.
        assert_eq!((c.words, c.first), (4, 2));
    }
    for rt in [0x00, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13] {
        let c = compiled(vec![i(0x01, 4, rt, rel(0, 3)), nop, nop]);
        assert_eq!((c.words, c.first), (3, 2));
    }
}

#[test]
fn segments_and_stops() {
    let nop = 0;
    let words = vec![
        nop,
        i(0x05, 4, 0, rel(1, 6)),
        nop,
        nop,
        nop,
        i(0x04, 4, 5, rel(5, 0)),
        nop,
        // SYSCALL: the interpreter must look again after it.
        r(0, 0, 0, 0, 0x0C),
        nop,
    ];
    let c = compiled(words);
    assert_eq!((c.words, c.first), (8, 1));
    // A branch whose delay slot is another branch: not compiled.
    let c = compiled(vec![nop, i(0x04, 4, 5, 4), i(0x04, 4, 5, 4), nop]);
    assert_eq!(c.words, 1);
}

#[test]
fn idle_loops_stay_with_the_interpreter() {
    let beq_back = i(0x04, 0, 0, rel(2, 0));
    assert_eq!(idle_candidate(beq_back, VA + 8), Some(VA));
    let c = compiled_with(vec![0, 0, beq_back, 0], vec![VA + 8]);
    assert_eq!(c.words, 2);
    // Not an idle loop: compiled.
    let c = compiled(vec![0, 0, beq_back, 0]);
    assert_eq!(c.words, 4);
}
