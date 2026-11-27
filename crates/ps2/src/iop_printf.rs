//! What the IOP's modules print (diagnostics): retail consoles send it
//! nowhere, but every module calls stdio's `printf` through an import stub,
//! which loadcore links to `printf` itself. Linked stubs keep their function
//! number, so `printf` is found by looking for stdio's import tables in IOP
//! memory; calls to it are formatted from the IOP's registers and stack.
//!
//! Diagnostics, when on (`calls`): every linked import is counted the same
//! way, by library, function number and caller.

use texel_ps1::cpu::Cpu;

/// An import table's first word, and stdio's `printf` function number.
const IMPORT_MAGIC: u32 = 0x41E0_0000;
const PRINTF: u32 = 4;

#[derive(Default)]
pub struct IopPrintf {
    /// Where `printf` is (it moves when the IOP restarts).
    targets: Vec<u32>,
    /// What was printed.
    pub out: Vec<u8>,
    /// Calls to the linked imports, when on: by (library, function
    /// number, caller's ra), how many, the last a0, and when (`now`).
    pub calls: Option<CallLog>,
    /// The time, in the caller's units, for `calls`.
    pub now: u64,
    /// Diagnostics, when on: the last calls to linked imports from callers
    /// in a range of addresses, in order (see `CallTrace`).
    pub trace: Option<CallTrace>,
    /// Diagnostics, when on: how many times the IOP ran each instruction.
    pub pcs: Option<std::collections::HashMap<u32, u64>>,
    /// Where each linked import goes: its library and function number.
    imports: std::collections::HashMap<u32, (String, u32)>,
}

/// See `IopPrintf::trace`: the callers' range, and (time, library,
/// function number, ra, a0, a1, sp) for each call, the last 20000.
#[derive(Default)]
pub struct CallTrace {
    pub from: u32,
    pub to: u32,
    pub calls: std::collections::VecDeque<(u64, String, u32, u32, u32, u32, u32)>,
}

/// See `IopPrintf::calls`.
pub type CallLog = std::collections::BTreeMap<(String, u32, u32), (u64, u32, u64)>;

fn word(ram: &[u8], at: u32) -> u32 {
    let at = (at as usize) & (ram.len() - 1) & !3;
    u32::from_le_bytes(ram[at..at + 4].try_into().unwrap_or_default())
}

impl IopPrintf {
    /// Looks for stdio's linked import tables, to find `printf`.
    pub fn scan(&mut self, ram: &[u8]) {
        self.targets.clear();
        self.imports.clear();
        for at in (0..ram.len() as u32 - 32).step_by(4) {
            if word(ram, at) != IMPORT_MAGIC {
                continue;
            }
            if self.calls.is_some() {
                let name = &ram[at as usize + 12..at as usize + 20];
                let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
                let mut stub = at + 20;
                while word(ram, stub) != 0 && stub < at + 20 + 8 * 128 {
                    let (jump, number) = (word(ram, stub), word(ram, stub + 4));
                    if jump >> 26 == 0x02 && number >> 16 == 0x2400 {
                        self.imports
                            .insert((jump & 0x03FF_FFFF) << 2, (name.clone(), number & 0xFFFF));
                    }
                    stub += 8;
                }
            }
            if &ram[at as usize + 12..at as usize + 20] != b"stdio\0\0\0" {
                continue;
            }
            let mut stub = at + 20;
            while word(ram, stub) != 0 && stub < at + 20 + 8 * 64 {
                let (jump, number) = (word(ram, stub), word(ram, stub + 4));
                // Linked: `j printf`, then the function number.
                if jump >> 26 == 0x02 && number == 0x2400_0000 | PRINTF {
                    let target = (jump & 0x03FF_FFFF) << 2;
                    if !self.targets.contains(&target) {
                        self.targets.push(target);
                    }
                }
                stub += 8;
            }
        }
    }

    /// Before the IOP runs the instruction at its PC: formats a call to
    /// `printf`.
    #[inline]
    pub fn check(&mut self, cpu: &Cpu, ram: &[u8]) {
        if let Some(pcs) = &mut self.pcs {
            *pcs.entry(cpu.pc).or_insert(0) += 1;
        }
        if self.targets.contains(&(cpu.pc & 0x1FFF_FFFF)) {
            self.format(cpu, ram);
        }
        if let Some(calls) = &mut self.calls
            && let Some((lib, n)) = self.imports.get(&(cpu.pc & 0x1FFF_FFFF))
        {
            let entry = calls.entry((lib.clone(), *n, cpu.reg(31))).or_default();
            *entry = (entry.0 + 1, cpu.reg(4), self.now);
            let ra = cpu.reg(31);
            if let Some(t) = &mut self.trace
                && (t.from..t.to).contains(&ra)
            {
                if t.calls.len() >= 20_000 {
                    t.calls.pop_front();
                }
                t.calls
                    .push_back((self.now, lib.clone(), *n, ra, cpu.reg(4), cpu.reg(5), cpu.reg(29)));
            }
        }
    }

    fn format(&mut self, cpu: &Cpu, ram: &[u8]) {
        let string = |mut at: u32| -> Vec<u8> {
            let mut s = Vec::new();
            while s.len() < 512 {
                let b = ram[(at as usize) & (ram.len() - 1)];
                if b == 0 {
                    break;
                }
                s.push(b);
                at += 1;
            }
            s
        };
        let format = string(cpu.reg(4));
        let sp = cpu.reg(29);
        let mut next = 1;
        let mut arg = || {
            next += 1;
            if next <= 4 {
                cpu.reg(3 + next)
            } else {
                word(ram, sp + 4 * (next as u32 - 1))
            }
        };
        let mut i = 0;
        while i < format.len() {
            let c = format[i];
            i += 1;
            if c != b'%' {
                self.out.push(c);
                continue;
            }
            // Flags, width and precision, then the conversion.
            let start = i;
            while i < format.len() && b"-+ #0123456789.lh".contains(&format[i]) {
                i += 1;
            }
            let spec = String::from_utf8_lossy(&format[start..i]).replace(['l', 'h'], "");
            let Some(&conv) = format.get(i) else { break };
            i += 1;
            let width: usize = spec
                .trim_start_matches(['-', '0', '+', ' ', '#'])
                .split('.')
                .next()
                .and_then(|w| w.parse().ok())
                .unwrap_or(0);
            let zero = spec.starts_with('0');
            let text = match conv {
                b'%' => "%".to_owned(),
                b'd' | b'i' => (arg() as i32).to_string(),
                b'u' => arg().to_string(),
                b'x' | b'p' => format!("{:x}", arg()),
                b'X' => format!("{:X}", arg()),
                b'c' => ((arg() as u8) as char).to_string(),
                b's' => String::from_utf8_lossy(&string(arg())).into_owned(),
                other => format!("%{}", other as char),
            };
            let pad = width.saturating_sub(text.len());
            let fill = if zero && conv != b's' { b'0' } else { b' ' };
            if spec.starts_with('-') {
                self.out.extend_from_slice(text.as_bytes());
                self.out.extend(std::iter::repeat_n(b' ', pad));
            } else {
                self.out.extend(std::iter::repeat_n(fill, pad));
                self.out.extend_from_slice(text.as_bytes());
            }
        }
    }
}
