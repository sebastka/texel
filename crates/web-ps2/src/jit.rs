//! Compiled EE blocks in the browser: modules are compiled and instantiated
//! with this module's own memory and function table, their functions added
//! to the table, and called from there.
//!
//! The workspace's only unsafe code is here, and only on wasm32: compiled
//! blocks get the EE and the bus as addresses in this module's memory
//! (they read and write the EE's registers there), and call back into the
//! interpreter with them; Rust calls a block through its function-table
//! index, which is what a function pointer is in WebAssembly.
#![allow(unsafe_code)]

use js_sys::{Object, Reflect, Uint8Array, WebAssembly};
use texel_ps2::bus::Bus;
use texel_ps2::ee::Ee;
use texel_ps2::ee::jit::{Backend, helpers};
use wasm_bindgen::{JsCast, JsValue};

type Block = extern "C" fn(*mut Ee, *mut Bus, u32) -> u32;
type Exec = extern "C" fn(*mut Ee, *mut Bus, u32, u32, u32) -> u32;

pub struct WebBackend {
    table: WebAssembly::Table,
    imports: Object,
    /// Table slots of blocks dropped, to reuse.
    free: Vec<u32>,
}

impl WebBackend {
    pub fn new() -> Result<Self, JsValue> {
        let table: WebAssembly::Table = wasm_bindgen::function_table().unchecked_into();
        let env = Object::new();
        Reflect::set(&env, &"memory".into(), &wasm_bindgen::memory())?;
        Reflect::set(&env, &"table".into(), &table)?;
        let imports = Object::new();
        Reflect::set(&imports, &"env".into(), &env)?;
        Ok(Self {
            table,
            imports,
            free: Vec::new(),
        })
    }
}

impl Backend for WebBackend {
    fn install(&mut self, module: &[u8]) -> Option<u32> {
        let module = WebAssembly::Module::new(&Uint8Array::from(module).into()).ok()?;
        let instance = WebAssembly::Instance::new(&module, &self.imports).ok()?;
        let block = Reflect::get(&instance.exports(), &"block".into()).ok()?;
        let index = match self.free.pop() {
            Some(i) => i,
            None => self.table.grow(1).ok()?,
        };
        self.table.set(index, block.unchecked_ref()).ok()?;
        Some(index)
    }

    fn release(&mut self, func: u32) {
        self.free.push(func);
    }

    fn call(&mut self, func: u32, ee: &mut Ee, bus: &mut Bus, left: u32) -> u32 {
        // SAFETY: `func` is the table index `install` gave a block's
        // function, whose type is `Block`; in WebAssembly a function
        // pointer is its table index.
        let block = unsafe { std::mem::transmute::<usize, Block>(func as usize) };
        block(ee, bus, left)
    }

    fn exec_index(&self) -> u32 {
        exec as Exec as usize as u32
    }
}

/// What compiled blocks call for the instructions they leave to the
/// interpreter (see `helpers::exec`).
extern "C" fn exec(ee: *mut Ee, bus: *mut Bus, instr: u32, pc: u32, flags: u32) -> u32 {
    // SAFETY: blocks are only run by `call`, which gives them the live EE
    // and bus, borrowed for the call; they pass them back here unchanged.
    let (ee, bus) = unsafe { (&mut *ee, &mut *bus) };
    helpers::exec(ee, bus, instr, pc, flags)
}
