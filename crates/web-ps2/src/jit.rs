//! Compiled EE blocks and VU1 programs in the browser: modules are compiled
//! (the EE's in batches, in the background) and instantiated with this
//! module's own memory and function table, their functions added to the
//! table, and called from there.
//!
//! The workspace's only unsafe code is here, and only on wasm32: compiled
//! blocks get the EE and the bus as addresses in this module's memory
//! (they read and write the EE's registers there), and call back into the
//! interpreter with them; Rust calls a block through its function-table
//! index, which is what a function pointer is in WebAssembly.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Object, Reflect, Uint8Array, WebAssembly};
use texel_ps2::bus::Bus;
use texel_ps2::ee::Ee;
use texel_ps2::ee::jit::{Backend, helpers};
use texel_ps2::gif::Gif;
use texel_ps2::gs::Gs;
use texel_ps2::iop::IopBus;
use texel_ps2::ps1::cpu::Cpu;
use texel_ps2::vu::Vu;
use texel_ps2::vu_jit::{VuBackend, helpers as vu_helpers};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

type Block = extern "C" fn(*mut Ee, *mut Bus, u32) -> u32;
type IopBlock = extern "C" fn(*mut Cpu, *mut IopBus) -> u32;
type IopExec = extern "C" fn(*mut Cpu, *mut IopBus, u32, u32, u32) -> u32;
type Exec = extern "C" fn(*mut Ee, *mut Bus, u32, u32, u32) -> u32;
type Program = extern "C" fn(*mut Vu, *mut u8, *mut Gif, *mut Gs) -> u32;
type Lower = extern "C" fn(*mut Vu, *mut Gif, *mut Gs, u32, u32);

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
    /// Table slots of functions dropped, to reuse.
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

    /// Compiles a module exporting one function, `block`, at once: a VU
    /// program, needed now.
    fn install_now(&mut self, module: &[u8]) -> Option<u32> {
        let module = WebAssembly::Module::new(&Uint8Array::from(module).into()).ok()?;
        let instance = WebAssembly::Instance::new(&module, &self.imports).ok()?;
        let block = Reflect::get(&instance.exports(), &"block".into()).ok()?;
        let reused = self.shared.borrow_mut().free.pop();
        let index = match reused {
            Some(i) => i,
            None => self.table.grow(1).ok()?,
        };
        self.table.set(index, block.unchecked_ref()).ok()?;
        Some(index)
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

    fn call(&mut self, func: u32, ee: &mut Ee, bus: &mut Bus, left: u32) -> u32 {
        // SAFETY: `func` is the table index `ready` gave a block's
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

impl VuBackend for WebBackend {
    fn install(&mut self, module: &[u8]) -> Option<u32> {
        self.install_now(module)
    }

    fn release(&mut self, func: u32) {
        self.shared.borrow_mut().free.push(func);
    }

    fn call(&mut self, func: u32, vu: &mut Vu, gif: &mut Gif, gs: &mut Gs) -> u32 {
        // SAFETY: `func` is the table index `install` gave a program's
        // function, whose type is `Program`.
        let program = unsafe { std::mem::transmute::<usize, Program>(func as usize) };
        let data = vu.data.as_mut_ptr();
        program(vu, data, gif, gs)
    }

    fn helper_index(&self) -> u32 {
        lower as Lower as usize as u32
    }
}

/// What compiled programs call for the lower instructions they leave to
/// the interpreter (see `vu_helpers::lower`).
extern "C" fn lower(vu: *mut Vu, gif: *mut Gif, gs: *mut Gs, instr: u32, pc: u32) {
    // SAFETY: programs are only run by `call`, which gives them the live VU,
    // GIF and GS, borrowed for the call; they pass them back here unchanged.
    let (vu, gif, gs) = unsafe { (&mut *vu, &mut *gif, &mut *gs) };
    vu_helpers::lower(vu, gif, gs, instr, pc);
}

/// The IOP's compiled blocks (the PlayStation's compiler, see
/// `Ps2::iop_jit`): modules made and put in the table as the EE's are.
impl texel_ps2::ps1::jit::Backend<IopBus> for WebBackend {
    fn submit(&mut self, module: &[u8], count: usize) -> u32 {
        Backend::submit(self, module, count)
    }

    fn ready(&mut self) -> Vec<(u32, Option<Vec<u32>>)> {
        Backend::ready(self)
    }

    fn call(&mut self, func: u32, cpu: &mut Cpu, bus: &mut IopBus) -> u32 {
        // SAFETY: `func` is the table index `ready` gave a block's
        // function, whose type is `IopBlock`; in WebAssembly a function
        // pointer is its table index.
        let block = unsafe { std::mem::transmute::<usize, IopBlock>(func as usize) };
        block(cpu, bus)
    }

    fn release(&mut self, func: u32) {
        Backend::release(self, func);
    }

    fn exec_index(&self) -> u32 {
        iop_exec as IopExec as usize as u32
    }
}

/// What compiled IOP blocks call for the instructions they leave to the
/// interpreter (see `Ps2::iop_exec`).
extern "C" fn iop_exec(cpu: *mut Cpu, bus: *mut IopBus, instr: u32, pc: u32, flags: u32) -> u32 {
    // SAFETY: blocks are only run by `call`, which gives them the live CPU
    // and bus, borrowed for the call; they pass them back here unchanged.
    let (cpu, bus) = unsafe { (&mut *cpu, &mut *bus) };
    texel_ps2::Ps2::iop_exec(cpu, bus, instr, pc, flags)
}
