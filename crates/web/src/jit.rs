//! Compiled CPU blocks in the browser: modules are compiled (in the
//! background) and instantiated with this module's own memory and function
//! table, their functions added to the table, and called from there.
//!
//! As in the PS2's module, the only unsafe code is here, and only on
//! wasm32: compiled blocks get the CPU and the bus as addresses in this
//! module's memory (they read and write the CPU's registers there), and
//! call back into the interpreter with them; Rust calls a block through its
//! function-table index, which is what a function pointer is in
//! WebAssembly.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Object, Reflect, Uint8Array, WebAssembly};
use texel_ps1::bus::Bus;
use texel_ps1::cpu::Cpu;
use texel_ps1::jit::{Backend, helpers};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

type Block = extern "C" fn(*mut Cpu, *mut Bus) -> u32;
type Exec = extern "C" fn(*mut Cpu, *mut Bus, u32, u32, u32) -> u32;

pub struct WebBackend {
    table: WebAssembly::Table,
    imports: Object,
    /// Shared with the compilations under way.
    shared: Rc<RefCell<Shared>>,
    /// The next module's number.
    next: u32,
}

#[derive(Default)]
struct Shared {
    /// Table slots of blocks dropped, to reuse.
    free: Vec<u32>,
    /// Modules compiled: their numbers and functions (see `Backend::ready`).
    done: Vec<(u32, Option<Vec<u32>>)>,
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
            shared: Rc::default(),
            next: 0,
        })
    }
}

impl Backend for WebBackend {
    fn submit(&mut self, module: &[u8], count: usize) -> u32 {
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        let bytes: JsValue = Uint8Array::from(module).into();
        let (table, imports, shared) = (self.table.clone(), self.imports.clone(), self.shared.clone());
        // Compiled off the emulation's way (WebAssembly.compile), put in
        // the table when ready; `ready` hands the slots over.
        wasm_bindgen_futures::spawn_local(async move {
            let funcs = async {
                let module = JsFuture::from(WebAssembly::compile(&bytes)).await.ok()?;
                let instance = WebAssembly::Instance::new(module.unchecked_ref(), &imports).ok()?;
                let exports = instance.exports();
                let mut slots = {
                    let free = &mut shared.borrow_mut().free;
                    let reused = count.min(free.len());
                    free.split_off(free.len() - reused)
                };
                if slots.len() < count {
                    // The table grown once for the rest.
                    let more = (count - slots.len()) as u32;
                    let first = table.grow(more).ok()?;
                    slots.extend(first..first + more);
                }
                for (k, &slot) in slots.iter().enumerate() {
                    let block = Reflect::get(&exports, &format!("b{k}").into()).ok()?;
                    table.set(slot, block.unchecked_ref()).ok()?;
                }
                Some(slots)
            }
            .await;
            shared.borrow_mut().done.push((number, funcs));
        });
        number
    }

    fn ready(&mut self) -> Vec<(u32, Option<Vec<u32>>)> {
        std::mem::take(&mut self.shared.borrow_mut().done)
    }

    fn release(&mut self, func: u32) {
        self.shared.borrow_mut().free.push(func);
    }

    fn call(&mut self, func: u32, cpu: &mut Cpu, bus: &mut Bus) -> u32 {
        // SAFETY: `func` is the table index `install` gave a block's
        // function, whose type is `Block`; in WebAssembly a function
        // pointer is its table index.
        let block = unsafe { std::mem::transmute::<usize, Block>(func as usize) };
        block(cpu, bus)
    }

    fn exec_index(&self) -> u32 {
        exec as Exec as usize as u32
    }
}

/// What compiled blocks call for the instructions they leave to the
/// interpreter (see `helpers::exec`).
extern "C" fn exec(cpu: *mut Cpu, bus: *mut Bus, instr: u32, pc: u32, flags: u32) -> u32 {
    // SAFETY: blocks are only run by `call`, which gives them the live CPU
    // and bus, borrowed for the call; they pass them back here unchanged.
    let (cpu, bus) = unsafe { (&mut *cpu, &mut *bus) };
    helpers::exec(cpu, bus, instr, pc, flags)
}
