//! Just enough of WebAssembly's binary format for compiled blocks: one
//! function, importing the emulator's memory and function table. Shared by
//! the PlayStation's CPU and the PS2's EE.

/// Value types.
pub const I32: u8 = 0x7F;
pub const I64: u8 = 0x7E;

/// Instruction opcodes used by the compiler.
pub mod op {
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0B;
    pub const BR: u8 = 0x0C;
    pub const BR_IF: u8 = 0x0D;
    pub const RETURN: u8 = 0x0F;
    pub const CALL_INDIRECT: u8 = 0x11;
    pub const SELECT: u8 = 0x1B;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const LOCAL_TEE: u8 = 0x22;
    pub const I32_LOAD: u8 = 0x28;
    pub const I64_LOAD: u8 = 0x29;
    pub const I32_STORE: u8 = 0x36;
    pub const I64_STORE: u8 = 0x37;
    pub const I32_LOAD8_S: u8 = 0x2C;
    pub const I32_LOAD8_U: u8 = 0x2D;
    pub const I32_LOAD16_S: u8 = 0x2E;
    pub const I32_LOAD16_U: u8 = 0x2F;
    pub const I32_STORE16: u8 = 0x3B;
    pub const I64_LOAD8_S: u8 = 0x30;
    pub const I64_LOAD8_U: u8 = 0x31;
    pub const I64_LOAD16_S: u8 = 0x32;
    pub const I64_LOAD16_U: u8 = 0x33;
    pub const I64_LOAD32_S: u8 = 0x34;
    pub const I64_LOAD32_U: u8 = 0x35;
    pub const I32_STORE8: u8 = 0x3A;
    pub const I64_STORE8: u8 = 0x3C;
    pub const I64_STORE16: u8 = 0x3D;
    pub const I64_STORE32: u8 = 0x3E;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;
    pub const I32_EQZ: u8 = 0x45;
    pub const I32_EQ: u8 = 0x46;
    pub const I32_NE: u8 = 0x47;
    pub const I32_LT_S: u8 = 0x48;
    pub const I32_LT_U: u8 = 0x49;
    pub const I32_GT_S: u8 = 0x4A;
    pub const I32_GT_U: u8 = 0x4B;
    pub const I32_LE_S: u8 = 0x4C;
    pub const I32_GE_S: u8 = 0x4E;
    pub const I32_GE_U: u8 = 0x4F;
    pub const I32_ADD: u8 = 0x6A;
    pub const I32_SUB: u8 = 0x6B;
    pub const I32_MUL: u8 = 0x6C;
    pub const I32_DIV_U: u8 = 0x6E;
    pub const I32_AND: u8 = 0x71;
    pub const I32_OR: u8 = 0x72;
    pub const I32_XOR: u8 = 0x73;
    pub const I32_SHL: u8 = 0x74;
    pub const I32_SHR_S: u8 = 0x75;
    pub const I32_SHR_U: u8 = 0x76;
    pub const I64_EQZ: u8 = 0x50;
    pub const I64_EQ: u8 = 0x51;
    pub const I64_NE: u8 = 0x52;
    pub const I64_LT_S: u8 = 0x53;
    pub const I64_LT_U: u8 = 0x54;
    pub const I64_GT_S: u8 = 0x55;
    pub const I64_LE_S: u8 = 0x57;
    pub const I64_GE_S: u8 = 0x59;
    pub const I64_ADD: u8 = 0x7C;
    pub const I64_SUB: u8 = 0x7D;
    pub const I64_AND: u8 = 0x83;
    pub const I64_OR: u8 = 0x84;
    pub const I64_XOR: u8 = 0x85;
    pub const I64_SHL: u8 = 0x86;
    pub const I64_SHR_S: u8 = 0x87;
    pub const I64_SHR_U: u8 = 0x88;
    pub const I32_WRAP_I64: u8 = 0xA7;
    pub const I64_EXTEND_I32_S: u8 = 0xAC;
    pub const I64_EXTEND_I32_U: u8 = 0xAD;
    pub const I64_EXTEND32_S: u8 = 0xC4;
}

/// Appends `v` as unsigned LEB128.
pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends `v` as signed LEB128.
pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A function body being written.
#[derive(Default)]
pub struct Code {
    pub bytes: Vec<u8>,
}

impl Code {
    pub fn op(&mut self, op: u8) -> &mut Self {
        self.bytes.push(op);
        self
    }

    pub fn local_get(&mut self, i: u32) -> &mut Self {
        self.op(op::LOCAL_GET);
        uleb(&mut self.bytes, u64::from(i));
        self
    }

    pub fn local_set(&mut self, i: u32) -> &mut Self {
        self.op(op::LOCAL_SET);
        uleb(&mut self.bytes, u64::from(i));
        self
    }

    pub fn local_tee(&mut self, i: u32) -> &mut Self {
        self.op(op::LOCAL_TEE);
        uleb(&mut self.bytes, u64::from(i));
        self
    }

    pub fn i32_const(&mut self, v: i32) -> &mut Self {
        self.op(op::I32_CONST);
        sleb(&mut self.bytes, i64::from(v));
        self
    }

    pub fn i64_const(&mut self, v: i64) -> &mut Self {
        self.op(op::I64_CONST);
        sleb(&mut self.bytes, v);
        self
    }

    /// A load or store, at a static `offset` from the address on the stack;
    /// `align`: log2 of the natural alignment (a hint).
    pub fn mem(&mut self, op: u8, align: u32, offset: u32) -> &mut Self {
        self.op(op);
        uleb(&mut self.bytes, u64::from(align));
        uleb(&mut self.bytes, u64::from(offset));
        self
    }

    /// `if` with no result.
    pub fn if_(&mut self) -> &mut Self {
        self.op(op::IF).op(0x40)
    }

    /// `if` with an i32 result.
    pub fn if_i32(&mut self) -> &mut Self {
        self.op(op::IF).op(I32)
    }

    /// `block` with no result.
    pub fn block(&mut self) -> &mut Self {
        self.op(op::BLOCK).op(0x40)
    }

    /// `loop` with no result.
    pub fn loop_(&mut self) -> &mut Self {
        self.op(op::LOOP).op(0x40)
    }

    pub fn br(&mut self, depth: u32) -> &mut Self {
        self.op(op::BR);
        uleb(&mut self.bytes, u64::from(depth));
        self
    }

    pub fn br_if(&mut self, depth: u32) -> &mut Self {
        self.op(op::BR_IF);
        uleb(&mut self.bytes, u64::from(depth));
        self
    }

    /// An indirect call through table 0, with type `ty`.
    pub fn call_indirect(&mut self, ty: u32) -> &mut Self {
        self.op(op::CALL_INDIRECT);
        uleb(&mut self.bytes, u64::from(ty));
        self.bytes.push(0);
        self
    }
}

/// A function type: parameters and results.
pub struct FuncType<'a> {
    pub params: &'a [u8],
    pub results: &'a [u8],
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// A module of one function, exported as "block", of type `types[0]`,
/// with `locals` (count, type) groups and `code` as its body (its final
/// `end` included). It imports `env.memory` and `env.table` (funcref).
pub fn module(types: &[FuncType], locals: &[(u32, u8)], code: &Code) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();

    let mut body = Vec::new();
    uleb(&mut body, types.len() as u64);
    for t in types {
        body.push(0x60);
        uleb(&mut body, t.params.len() as u64);
        body.extend_from_slice(t.params);
        uleb(&mut body, t.results.len() as u64);
        body.extend_from_slice(t.results);
    }
    section(&mut out, 1, &body);

    body.clear();
    uleb(&mut body, 2);
    name(&mut body, "env");
    name(&mut body, "memory");
    // Memory, at least one page, no maximum.
    body.extend_from_slice(&[0x02, 0x00, 0x01]);
    name(&mut body, "env");
    name(&mut body, "table");
    // Table of funcref, at least one entry, no maximum.
    body.extend_from_slice(&[0x01, 0x70, 0x00, 0x01]);
    section(&mut out, 2, &body);

    // One function, of type 0.
    section(&mut out, 3, &[1, 0]);

    body.clear();
    uleb(&mut body, 1);
    name(&mut body, "block");
    body.extend_from_slice(&[0x00, 0x00]);
    section(&mut out, 7, &body);

    let mut func = Vec::new();
    uleb(&mut func, locals.len() as u64);
    for &(n, t) in locals {
        uleb(&mut func, u64::from(n));
        func.push(t);
    }
    func.extend_from_slice(&code.bytes);
    body.clear();
    uleb(&mut body, 1);
    uleb(&mut body, func.len() as u64);
    body.extend_from_slice(&func);
    section(&mut out, 10, &body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let enc = |v: i64| {
            let mut o = Vec::new();
            sleb(&mut o, v);
            o
        };
        assert_eq!(enc(0), [0]);
        assert_eq!(enc(-1), [0x7F]);
        assert_eq!(enc(63), [0x3F]);
        assert_eq!(enc(64), [0xC0, 0x00]);
        assert_eq!(enc(-64), [0x40]);
        assert_eq!(enc(-65), [0xBF, 0x7F]);
        let mut o = Vec::new();
        uleb(&mut o, 624_485);
        assert_eq!(o, [0xE5, 0x8E, 0x26]);
    }
}
