//! Boots a PlayStation 2 BIOS on the EE alone, for testing:
//!
//! ```sh
//! cargo run --release -p texel-ps2 --example boot -- BIOS [SECONDS]
//! ```
//!
//! Prints what the BIOS kernel writes to its debug port, then where the
//! EE is, what it waits for, and what it touched that is not emulated.
//!
//! `DISC=image.iso` (or a raw `.bin` of 2352-byte sectors, or a zip holding
//! one, unpacked in memory) puts a disc in the drive and boots its game
//! directly (`OSD=1`: through the browser instead).
//!
//! `LOAD=state` starts from a save state (with the same BIOS and disc),
//! `SAVE=state` saves one at the end.

use std::collections::BTreeMap;
use std::io::Write;

use texel_ps2::Ps2;
use texel_ps2::bus::EE_CLOCK;
use texel_ps2::gs::REG_NAMES;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let bios_path = args.next().ok_or("usage: boot BIOS [SECONDS]")?;
    let seconds: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(2.0);
    let bios = std::fs::read(&bios_path)?;
    match texel_ps2::bios::identify(&bios) {
        Some(info) => println!("BIOS {} ({}, {})", info.version, info.date, info.region.as_str()),
        None => return Err("not a PlayStation 2 BIOS".into()),
    }
    let mut ps2 = Ps2::new(&bios);
    if std::env::var("CDVDREADS").is_ok() {
        ps2.bus.iop.cdvd.read_log = Some(Vec::new());
    }
    // What the IOP's modules print; IOPCALLS=1: their calls to the linked
    // imports too.
    ps2.iop_printf = Some(Default::default());
    if std::env::var("IOPCALLS").is_ok()
        && let Some(p) = &mut ps2.iop_printf
    {
        p.calls = Some(Default::default());
    }
    // NOGFX=1: draw nothing (faster, when frames do not matter).
    if std::env::var("NOGFX").is_ok() {
        ps2.bus.gs.rendering = texel_ps2::gs::Rendering::Off;
    }
    for (slot, var) in ["MC1", "MC2"].iter().enumerate() {
        if let Ok(path) = std::env::var(var) {
            let image = std::fs::read(&path).ok();
            ps2.bus.iop.sio2.cards[slot] = Some(Box::new(texel_ps2::memcard::MemoryCard::new(image.as_deref())));
        }
    }
    if let Ok(path) = std::env::var("DISC") {
        let medium = if path.ends_with(".zip") {
            let (name, image) = unzip_image(&path)?;
            let layout = texel_disc::Layout::single_file(&name, image.len() as u64).ok_or("not a disc image")?;
            texel_disc::Disc::new(layout, Box::new(MemoryImage(image)) as _)
        } else {
            let size = std::fs::metadata(&path)?.len();
            let layout = texel_disc::Layout::single_file(&path, size).ok_or("not a disc image")?;
            texel_disc::Disc::new(layout, Box::new(FileImage(std::fs::File::open(&path)?)) as _)
        };
        let cnf = ps2.insert_disc(medium, std::env::var("OSD").is_err())?;
        println!("Disc: {} ({})", cnf.boot, cnf.volume_id);
    }
    if let Ok(path) = std::env::var("LOAD") {
        ps2.load_state(&std::fs::read(&path)?)?;
        println!("Loaded {path}");
    }
    // IPUTRACE=path: the IPU's input stream (path.bin) and its commands
    // (path.txt), for another decoder to compare.
    if std::env::var("IPUTRACE").is_ok() {
        ps2.bus.ipu.trace = Some(Box::default());
        ps2.bus.ipu_sources = Some((Vec::new(), 0, 0));
        // IPUWATCH=address,...: writes to those lines of 64 bytes, in the
        // same log.
        ps2.bus.ipu_watch = std::env::var("IPUWATCH")
            .unwrap_or_default()
            .split(',')
            .filter_map(|a| u32::from_str_radix(a, 16).ok())
            .collect();
    }
    // BREAK=pc: the EE's registers the first time it reaches that
    // instruction (interpreted); BREAKIF=reg=hex: when that register's low
    // word holds that value.
    ps2.ee.break_if = std::env::var("BREAKIF").ok().and_then(|v| {
        let (r, x) = v.split_once('=')?;
        Some((r.parse().ok()?, u32::from_str_radix(x, 16).ok()?))
    });
    ps2.ee.break_at = std::env::var("BREAK")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok());
    // TAGWATCH=ch,from,to (hex): the chain tags that channel reads in that
    // range of addresses, or that point into it.
    if let Ok(v) = std::env::var("TAGWATCH") {
        let p: Vec<u32> = v.split(',').filter_map(|x| u32::from_str_radix(x, 16).ok()).collect();
        if let [ch, from, to] = p[..] {
            ps2.bus.tag_watch = Some((ch, from, to, Vec::new()));
        }
    }
    // VALUEWATCH=hex: the EE instructions storing that 32-bit value.
    if let Some(v) = std::env::var("VALUEWATCH")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
    {
        ps2.bus.value_watch = Some((v, std::collections::BTreeMap::new()));
    }
    // SYSCALLS=1: every EE syscall, by number and address (reported with
    // EXC).
    if std::env::var("SYSCALLS").is_ok() {
        ps2.ee.syscalls = Some(Default::default());
    }
    // PROBE=x,y (window pixels): the triangles drawn over that pixel
    // (reported with DRAWS, the last ones).
    if let Some((x, y)) = std::env::var("PROBE").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    }) {
        ps2.bus.gs.probe = Some(((x, y), Vec::new()));
    }
    // TRANSFERS=1: the GS's transfers, from the start (reported with
    // DRAWS).
    if std::env::var("TRANSFERS").is_ok() {
        ps2.bus.gs.transfer_log = Some(Vec::new());
    }
    // VU1THREAD=1: VU1 in a thread of its own (see `vu1q`).
    if std::env::var("VU1THREAD").is_ok() {
        let b = &mut ps2.bus;
        b.vu1q.set_backend(
            Some(Box::new(texel_ps2::vu1q::Thread::spawn())),
            &b.vu1,
            &b.gif,
            &b.vif1,
        );
    }
    // TRACE=N: the first N instructions' addresses, with the stack pointer
    // and Status, when they change.
    if let Some(n) = std::env::var("TRACE").ok().and_then(|v| v.parse::<u64>().ok()) {
        let (mut sp, mut status) = (u128::MAX, u32::MAX);
        for _ in 0..n {
            let pc = ps2.ee.pc;
            ps2.ee.step(&mut ps2.bus);
            if ps2.ee.gpr[29] != sp || ps2.ee.cop0.regs[12] != status {
                sp = ps2.ee.gpr[29];
                status = ps2.ee.cop0.regs[12];
                println!("{pc:08X}: sp {:016X} status {status:08X}", sp as u64);
            }
        }
        for (i, e) in ps2
            .ee
            .cop0
            .tlb
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entry_lo0 | e.entry_lo1 != 0)
        {
            println!(
                "TLB {i:2}: mask {:08X} hi {:08X} lo0 {:08X} lo1 {:08X}",
                e.page_mask, e.entry_hi, e.entry_lo0, e.entry_lo1
            );
        }
        return Ok(());
    }
    let mut sound: Vec<i16> = Vec::new();
    let total = (seconds * EE_CLOCK as f64) as u64;
    let started = std::time::Instant::now();
    let (mut ee_printed, mut iop_printed) = (0, 0);
    let mut out = std::io::stdout();
    // Run in tenths of a second, printing the debug port as it fills.
    let slice = EE_CLOCK / 10;
    let mut done = 0;
    // SHOTS=dir: the displayed frame every second, as PPM.
    let shots = std::env::var("SHOTS").ok().map(std::path::PathBuf::from);
    if let Some(dir) = &shots {
        std::fs::create_dir_all(dir)?;
    }
    let mut frame = Vec::new();
    // PRESS="16:cross,20.5:down": hold a button for 0.2 s from a time (in
    // seconds of console time).
    let presses: Vec<(f64, u16)> = std::env::var("PRESS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| {
            let (at, name) = p.split_once(':')?;
            use texel_ps1::button as b;
            let button = match name {
                "cross" => b::CROSS,
                "circle" => b::CIRCLE,
                "square" => b::SQUARE,
                "triangle" => b::TRIANGLE,
                "start" => b::START,
                "select" => b::SELECT,
                "up" => b::UP,
                "down" => b::DOWN,
                "left" => b::LEFT,
                "right" => b::RIGHT,
                _ => return None,
            };
            Some((at.parse().ok()?, button))
        })
        .collect();
    while done < total {
        let now = done as f64 / EE_CLOCK as f64;
        let held = presses
            .iter()
            .filter(|(at, _)| now >= *at && now < at + 0.2)
            .fold(0, |acc, (_, b)| acc | b);
        ps2.set_pad(0, held, [0x80; 4]);
        ps2.run(slice.min(total - done));
        ps2.bus.iop.spu2.drain_output(&mut sound);
        done += slice;
        if let Some(dir) = &shots
            && done % EE_CLOCK < slice
        {
            let (w, h) = ps2.bus.gs.display(&mut frame);
            if w > 0 {
                let path = dir.join(format!("frame-{:03}.ppm", done / EE_CLOCK));
                write_ppm(&path, w, h, &frame)?;
            }
        }
        print_lines(&mut out, "EE  | ", &ps2.bus.sio_out, &mut ee_printed)?;
        let iop_out = ps2.iop_printf.as_ref().map_or(&ps2.bus.iop.out, |p| &p.out);
        print_lines(&mut out, "IOP | ", iop_out, &mut iop_printed)?;
    }
    // How long the run took, and a hash of the state it reached (equal on
    // two runs that went the same way).
    let ran = started.elapsed().as_secs_f64();
    ps2.settle();
    let hash = ps2.save_state().iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
    });
    println!("Ran in {ran:.2} s; state {hash:016x}");
    if let Ok(path) = std::env::var("SAVE") {
        let state = ps2.save_state();
        std::fs::write(&path, &state)?;
        println!("Saved {path} ({} bytes)", state.len());
    }
    // DRAWS=frames: what the GS is asked to draw over that many more frames
    // (the most common vertex positions, the largest primitives).
    if let Some(n) = std::env::var("DRAWS").ok().and_then(|v| v.parse::<u64>().ok()) {
        dump_draws(&mut ps2, n);
    }
    // Where the EE spends its time now: a sample of PCs.
    let mut pcs: BTreeMap<u32, u32> = BTreeMap::new();
    for _ in 0..20_000 {
        ps2.ee.step(&mut ps2.bus);
        *pcs.entry(ps2.ee.pc).or_insert(0) += 1;
    }
    // And the IOP's.
    let mut iop_pcs: BTreeMap<u32, u32> = BTreeMap::new();
    for _ in 0..200_000 {
        ps2.iop.step(&mut ps2.bus.iop);
        *iop_pcs.entry(ps2.iop.pc).or_insert(0) += 1;
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!();
    println!(
        "--- after {seconds} s of EE time ({:.2} s here, {:.0} MHz)",
        elapsed,
        done as f64 / elapsed / 1e6
    );
    println!("PC {:08X}, frames {}", ps2.ee.pc, ps2.bus.frames);
    let (lo, hi) = (
        pcs.keys().next().copied().unwrap_or(0),
        pcs.keys().last().copied().unwrap_or(0),
    );
    println!("busy in {lo:08X}-{hi:08X} ({} addresses)", pcs.len());
    // MEM=start,end: EE memory as words (hex, physical); IOPMEM likewise.
    for (var, iop) in [("MEM", false), ("IOPMEM", true)] {
        if let Some((a, b)) = std::env::var(var).ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
        }) {
            for x in (a..b).step_by(16) {
                let w: Vec<u32> = (0..4)
                    .map(|k| {
                        if iop {
                            let at = ((x + 4 * k) as usize) & 0x1F_FFFF;
                            u32::from_le_bytes(ps2.bus.iop.ram[at..at + 4].try_into().unwrap())
                        } else {
                            ps2.bus.read32(x + 4 * k)
                        }
                    })
                    .collect();
                let text: String = w
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .map(|b| {
                        if b.is_ascii_graphic() || b == b' ' {
                            b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                println!("  {var} {x:08X}: {w:08X?} {text}");
            }
        }
    }
    // CODE=start,end: kernel code to show (hex, physical).
    if let Some((a, b)) = std::env::var("CODE").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
    }) {
        for x in (a..b).step_by(4) {
            println!("  {:08X}: {:08X}", 0x8000_0000 | x, ps2.bus.read32(x));
        }
    }
    // IOPRAM=path: the IOP's memory, saved there (to disassemble).
    if let Ok(path) = std::env::var("IOPRAM") {
        std::fs::write(&path, &ps2.bus.iop.ram)?;
    }
    // IOPFIND=imm: IOP loads and stores with that offset.
    if let Some(imm) = std::env::var("IOPFIND")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
    {
        for a in (0..0x0020_0000usize).step_by(4) {
            let w = u32::from_le_bytes(ps2.bus.iop.ram[a..a + 4].try_into().unwrap());
            if w & 0xFFFF == imm && matches!(w >> 26, 0x20..=0x2B | 0x0D | 0x09) {
                let ctx: Vec<String> = (0..12)
                    .map(|k| {
                        let b = a.saturating_sub(24) + 4 * k;
                        format!(
                            "{:08X}",
                            u32::from_le_bytes(ps2.bus.iop.ram[b..b + 4].try_into().unwrap())
                        )
                    })
                    .collect();
                println!("  IOP {a:08X}: {w:08X} | {}", ctx.join(" "));
            }
        }
    }
    // FIND=imm: loads, stores, LUI, ADDIU and ORI with that 16-bit
    // immediate, anywhere in RAM.
    if let Some(imm) = std::env::var("FIND")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
    {
        for a in (0..0x0200_0000u32).step_by(4) {
            let w = ps2.bus.read32(a);
            // Loads and stores (any width), LUI, ADDIU and ORI with that
            // immediate.
            if w & 0xFFFF == imm && matches!(w >> 26, 0x09 | 0x0D | 0x0F | 0x1A..=0x1F | 0x20..=0x3F) {
                println!("  {:08X}: {w:08X} op {:02X}", 0x8000_0000 | a, w >> 26);
            }
        }
    }
    // DUMP=1: the sampled addresses, and the busy loop's instructions.
    if std::env::var("DUMP").is_ok() {
        for (pc, n) in &pcs {
            if let Ok(pa) = ps2.ee.cop0.translate(*pc, false) {
                println!("  {pc:08X} x{n}: {:08X}", ps2.bus.read32(pa));
            }
        }
        for (r, v) in ps2.ee.gpr.iter().enumerate().skip(1) {
            print!("r{r}={:X} ", *v as u64);
        }
        println!();
        for va in (lo.saturating_sub(16)..=hi.min(lo + 0x200) + 4).step_by(4) {
            if let Ok(pa) = ps2.ee.cop0.translate(va, false) {
                println!("  {va:08X}: {:08X}", ps2.bus.read32(pa));
            }
        }
    }
    let status = ps2.ee.cop0.regs[12];
    let c = &ps2.ee.cop0.regs;
    println!(
        "COP0 Status {status:08X}, Cause {:08X}, EPC {:08X}, BadVAddr {:08X}, ErrorEPC {:08X}",
        c[13], c[14], c[8], c[30]
    );
    println!("INTC stat {:04X} mask {:04X}", ps2.bus.intc.stat, ps2.bus.intc.mask);
    for (k, t) in ps2.bus.timers.t.iter().enumerate() {
        println!(
            "  EE timer {k}: count {:X} mode {:X} compare {:X}",
            t.count, t.mode, t.compare
        );
    }
    if std::env::var("EEREGS").is_ok() {
        for r in (0..32).step_by(2) {
            println!("  EE r{r:<2} {:032X} {:032X}", ps2.ee.gpr[r], ps2.ee.gpr[r + 1]);
        }
        println!("  EE hi {:032X} lo {:032X}", ps2.ee.hi, ps2.ee.lo);
    }
    if std::env::var("IOPREGS").is_ok() {
        for r in (0..32).step_by(4) {
            println!(
                "  IOP r{r:<2} {:08X} {:08X} {:08X} {:08X}",
                ps2.iop.reg(r),
                ps2.iop.reg(r + 1),
                ps2.iop.reg(r + 2),
                ps2.iop.reg(r + 3)
            );
        }
    }
    let [sr, cause, epc, bad] = ps2.iop.cop0_state();
    println!(
        "IOP PC {:08X}; Status {sr:08X} Cause {cause:08X} EPC {epc:08X} BadVaddr {bad:08X}; ra {:08X}",
        ps2.iop.pc,
        ps2.iop.reg(31)
    );
    if std::env::var("DUMP").is_ok() {
        // The hottest 4 KiB pages, then the hottest addresses.
        let mut pages: BTreeMap<u32, u32> = BTreeMap::new();
        for (pc, n) in &iop_pcs {
            *pages.entry(pc & !0xFFF).or_insert(0) += n;
        }
        let mut pages: Vec<_> = pages.into_iter().collect();
        pages.sort_by_key(|p| std::cmp::Reverse(p.1));
        println!("  IOP hot pages: {:X?}", &pages[..pages.len().min(12)]);
        let regs: Vec<String> = (1..32).map(|r| format!("r{r}={:X}", ps2.iop.reg(r))).collect();
        println!("  IOP registers: {}", regs.join(" "));
        let mut hot: Vec<_> = iop_pcs.iter().collect();
        hot.sort_by_key(|p| std::cmp::Reverse(*p.1));
        for (pc, n) in hot.iter().take(25) {
            println!(
                "  IOP {pc:08X} x{n}: {:08X}",
                texel_ps1::cpu::CpuBus::load32(&mut ps2.bus.iop, **pc)
            );
        }
    }
    if let Some(p) = &ps2.iop_printf
        && let Some(calls) = &p.calls
    {
        // The most recent last (a slice is 512 EE cycles).
        let mut calls: Vec<_> = calls.iter().collect();
        calls.sort_by_key(|(_, (_, _, at))| *at);
        for ((lib, n, ra), (count, a0, at)) in &calls {
            let ago = (p.now - at) as f64 * 512.0 / 294_912_000.0;
            println!("  IOP call {lib}#{n} from {ra:08X}: {count} times, last a0 {a0:08X}, {ago:.2} s ago");
        }
    }
    if let Some((at, hits)) = &ps2.bus.gs.probe {
        println!("Triangles over {at:?}: {}", hits.len());
        for v in hits.iter().rev().take(40).rev() {
            println!("  {:x?}", v);
        }
    }
    if let Some(log) = &ps2.ee.syscalls {
        let now = ps2.ee.cycles;
        for ((n, pc), (count, a0, at)) in log {
            let ago = (now - at) as f64 / 294_912_000.0;
            println!(
                "  syscall {:3} ({n:08X}) at {pc:08X}: {count} times, last a0 {a0:08X}, {ago:.2} s ago",
                *n as i32
            );
        }
    }
    if std::env::var("EXC").is_ok() {
        for (code, epc, v1, a0) in &ps2.ee.trace {
            if *code != 0 {
                println!("  exception {code} at {epc:08X}, v1 {v1:08X} a0 {a0:08X}");
            }
        }
    }
    let sif = &ps2.bus.iop.sif;
    println!(
        "SIF MSFLG {:08X} SMFLG {:08X} MSCOM {:08X} SMCOM {:08X} (SMFLG read {} times)",
        sif.msflg, sif.smflg, sif.mscom, sif.smcom, sif.smflg_reads
    );
    println!(
        "SIF words: IOP->EE {}, EE->IOP {}; FIFOs {} / {}",
        sif.sif0_words,
        sif.sif1_words,
        sif.sif0.len(),
        sif.sif1.len()
    );
    for (name, base) in [("SIF0", 0x1000_C000u32), ("SIF1", 0x1000_C400)] {
        println!(
            "EE {name}: CHCR {:08X} MADR {:08X} QWC {:X} TADR {:08X}",
            ps2.bus.register(base),
            ps2.bus.register(base + 0x10),
            ps2.bus.register(base + 0x20),
            ps2.bus.register(base + 0x30)
        );
    }
    println!(
        "EE D_STAT {:08X}, written {:08X?}",
        ps2.bus.register(0x1000_E010),
        ps2.bus.d_stat_writes
    );
    println!(
        "EE D_CTRL {:08X} D_PCR {:08X}",
        ps2.bus.register(0x1000_E000),
        ps2.bus.register(0x1000_E020)
    );
    println!("IOP SIF writes: {:X?}", sif.iop_writes);
    if std::env::var("PACKETS").is_ok() {
        for (dir, p) in &sif.packets {
            println!("  {dir}: {p:08X?}");
        }
    }
    if std::env::var("TAGS").is_ok() {
        for (who, a, b) in &sif.tags {
            println!("  {who:14} {a:08X} {b:08X}");
        }
    }
    println!("IOP registers: {:X?}", ps2.bus.iop.registers());
    let tadr = ps2.bus.iop.dma.ch[9][3];
    for a in (tadr.saturating_sub(48)..tadr + 16).step_by(16) {
        let w: Vec<u32> = (0..4)
            .map(|k| {
                let at = ((a + 4 * k) as usize) & 0x1F_FFFF;
                u32::from_le_bytes(ps2.bus.iop.ram[at..at + 4].try_into().unwrap())
            })
            .collect();
        println!("  IOP SIF0 tags at {a:08X}: {w:08X?}");
    }
    let d = &ps2.bus.iop.dma;
    println!(
        "IOP SIF0 ch9 {:08X?}, SIF1 ch10 {:08X?}; DICR {:08X} DICR2 {:08X}",
        d.ch[9], d.ch[10], d.dicr, d.dicr2
    );
    // Channels still busy (CHCR bit 24): MADR, BCR, CHCR, TADR.
    for (n, c) in d.ch.iter().enumerate() {
        if c[2] & 1 << 24 != 0 {
            println!("IOP DMA ch{n} busy {c:08X?}");
        }
    }
    let i = &ps2.bus.iop.intc;
    println!("IOP INTC stat {:08X} mask {:08X} ctrl {:X}", i.stat, i.mask, i.ctrl);
    let raised: Vec<(usize, u64)> = i.counts.iter().copied().enumerate().filter(|(_, n)| *n > 0).collect();
    println!("IOP interrupts raised (source, count): {raised:?}");
    let t = &ps2.bus.iop.timers.t;
    for (k, t) in t.iter().enumerate() {
        println!(
            "  IOP timer {k}: count {:X} mode {:X} target {:X}",
            t.count, t.mode, t.target
        );
    }
    println!(
        "GIF packets by path {:?}; GS primitives drawn {}; VIF1 programs started {}; VU1 steps {} ({} runaways), VU0 steps {} ({} runaways)",
        ps2.bus.gif.packets,
        ps2.bus.gs.drawn,
        ps2.bus.vif1.calls,
        ps2.bus.vu1.steps,
        ps2.bus.vu1.runaways,
        ps2.bus.vu0.steps,
        ps2.bus.vu0.runaways
    );
    println!("VU1 queue {:?}", ps2.bus.vu1q.stats);
    println!("GIF quadwords by path {:?}", ps2.bus.gif.qwords);
    println!(
        "Path 3: GIF_MODE {:08X}, VIF1's MSKPATH3 {}; GIF DMA CHCR {:08X} MADR {:08X} QWC {:X} TADR {:08X}; D_CTRL {:08X}",
        ps2.bus.register(0x1000_3010),
        ps2.bus.vif1.mask_path3,
        ps2.bus.register(0x1000_A000),
        ps2.bus.register(0x1000_A010),
        ps2.bus.register(0x1000_A020),
        ps2.bus.register(0x1000_A030),
        ps2.bus.register(0x1000_E000)
    );
    let ipu = &ps2.bus.ipu;
    println!(
        "IPU commands (BCLR IDEC BDEC VDEC FDEC SETIQ SETVQ CSC PACK SETTH) {:?}, errors {}, busy {}, output {} quadwords",
        &ipu.commands[..10],
        ipu.errors,
        ipu.busy(),
        ipu.output.len()
    );
    if let (Some(pc), Some(regs)) = (ps2.ee.break_at, &ps2.ee.break_regs) {
        println!("Registers at {pc:08X}:");
        for (k, r) in regs.iter().enumerate() {
            print!("r{k}={:X}{}", *r as u64, if k % 8 == 7 { "\n" } else { " " });
        }
    }
    if let Some((ch, from, to, log)) = &ps2.bus.tag_watch {
        println!("Channel {ch}'s tags in or into {from:08X}-{to:08X} ({}):", log.len());
        for (tadr, tag) in log.iter().take(4096) {
            let id = (tag >> 28) & 7;
            println!(
                "  at {tadr:08X}: {tag:016X} (id {id}, qwc {}, to {:08X})",
                tag & 0xFFFF,
                tag >> 32
            );
        }
    }
    if let Some((v, hits)) = &ps2.bus.value_watch {
        println!("Stores of {v:#010x}, by EE instruction:");
        for (pc, (pa, n)) in hits {
            println!("  {pc:08X}: {n} times, last at {pa:08X}");
        }
    }
    // TEXDUMP=TEX0 (hex): that texture as it is in GS memory, to texture.pgm
    // (4- and 8-bit indices, scaled to grey) or texture.ppm (colours).
    if let Some(tex0) = std::env::var("TEXDUMP")
        .ok()
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
    {
        let field = |lo: u32, n: u32| ((tex0 >> lo) & ((1 << n) - 1)) as u32;
        let (tbp, tbw, psm) = (field(0, 14), field(14, 6), field(20, 6));
        let (w, h) = (1u32 << field(26, 4), 1u32 << field(30, 4));
        let mem = &ps2.bus.gs.mem;
        let indexed = matches!(psm, 0x13 | 0x14 | 0x1B | 0x24 | 0x2C);
        let mut out = if indexed {
            format!("P5\n{w} {h}\n255\n").into_bytes()
        } else {
            format!("P6\n{w} {h}\n255\n").into_bytes()
        };
        let scale = if matches!(psm, 0x14 | 0x24 | 0x2C) { 17 } else { 1 };
        for y in 0..h {
            for x in 0..w {
                let v = mem.read(psm, tbp, tbw, x, y);
                if indexed {
                    out.push((v * scale).min(255) as u8);
                } else {
                    out.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
                }
            }
        }
        let first: Vec<String> = (0..w.min(16))
            .map(|x| format!("{:08X}", mem.read(psm, tbp, tbw, x, 0)))
            .collect();
        println!("  first row, raw: {}", first.join(" "));
        let path = if indexed { "texture.pgm" } else { "texture.ppm" };
        std::fs::write(path, out)?;
        println!("Texture {tex0:#x}: {w}x{h}, format {psm:#x}, at block {tbp:#x} (width {tbw}), in {path}");
    }
    if let (Ok(path), Some(t)) = (std::env::var("IPUTRACE"), &ipu.trace) {
        std::fs::write(format!("{path}.bin"), &t.stream)?;
        let lines: String = t
            .commands
            .iter()
            .map(|c| {
                format!(
                    "{:X} {:07X} {} {} {} {:08X} {}\n",
                    c.code,
                    c.opt,
                    c.start,
                    c.end,
                    u8::from(c.err),
                    c.data,
                    c.frame
                )
            })
            .collect();
        std::fs::write(format!("{path}.txt"), lines)?;
        if let Some((log, _, _)) = &ps2.bus.ipu_sources {
            let lines: String = log
                .iter()
                .map(|&(at, a, tag)| match tag {
                    0 => format!("{at} {a:08X}\n"),
                    t if t >> 61 == 1 => format!("{at} write {a:08X} {:08X} ({} bytes)\n", t as u32, (t >> 32) & 0xFF),
                    t if t >> 60 == 1 => format!("{at} PC {a:08X}\n"),
                    t if t >> 59 == 1 => format!(
                        "{at} IPU_CMD {a:08X} at {:08X}{}\n",
                        t as u32,
                        if (t >> 32) & 1 != 0 { " (IPU busy)" } else { "" }
                    ),
                    t if t >> 58 == 1 => format!(
                        "{at} CHCR write {a:08X} at {:08X}, QWC {}\n",
                        t as u32,
                        (t >> 32) & 0xFFFF
                    ),
                    t if t >> 62 == 1 => format!(
                        "{at} BP {a:08X} IFC {} FP {} QWC {}\n",
                        (t >> 8) & 0xFF,
                        t & 0xFF,
                        (t >> 16) & 0xFFFF
                    ),
                    t if t >> 63 != 0 => format!(
                        "{at} {a:08X} start CHCR {:08X} after {:08X}\n",
                        t as u32,
                        (t >> 32) as u32 & 0x7FFF_FFFF
                    ),
                    t => format!("{at} {a:08X} tag {t:016X}\n"),
                })
                .collect();
            std::fs::write(format!("{path}.src"), lines)?;
        }
        println!(
            "IPU trace: {} bytes of stream, {} commands",
            t.stream.len(),
            t.commands.len()
        );
    }
    for (code, opt, started, failed, next) in &ipu.error_log {
        println!(
            "  IPU error: command {code:X} options {opt:08X}, from bit {started} to {failed} (+{}), then {next:08X}",
            failed.saturating_sub(*started)
        );
    }
    if let Some((ctrl, words)) = &ipu.error_input {
        println!("  IPU first non-intra BDEC error: IPU_CTRL {ctrl:08X}, input:");
        for q in words {
            println!("    {q:032X}");
        }
    }
    let mut gs_regs: BTreeMap<u32, u64> = BTreeMap::new();
    for &(off, v) in &ps2.bus.gs.privileged.writes {
        gs_regs.insert(off, v);
    }
    println!("GS registers written ({} writes):", ps2.bus.gs.privileged.writes.len());
    for (off, v) in gs_regs {
        let name = if off < 0x100 {
            REG_NAMES[(off >> 4) as usize]
        } else {
            "CSR/IMR/SIGLBLID"
        };
        println!("  {:08X} {name:9} = {v:016X}", 0x1200_0000 + off);
    }
    if !ps2.bus.dma_starts.is_empty() {
        // Mode: 0 normal, 1 chain, 2 interleave; TTE: tags transferred.
        println!("DMA transfers started (channel, mode, TTE: count):");
        for ((addr, bits), n) in &ps2.bus.dma_starts {
            println!("  {addr:08X} mode {} TTE {}: {n}", (bits >> 2) & 3, (bits >> 6) & 1);
        }
    }
    if !ps2.ee.unimplemented.is_empty() {
        println!("Instructions not emulated:");
        for (name, n) in &ps2.ee.unimplemented {
            println!("  {name}: {n}");
        }
    }
    println!(
        "CDVD accesses ((write, register), count): {:X?}",
        ps2.bus.iop.cdvd.accesses
    );
    for (slot, var) in ["MC1", "MC2"].iter().enumerate() {
        if let (Ok(path), Some(card)) = (std::env::var(var), ps2.bus.iop.sio2.cards[slot].as_mut())
            && let Some(image) = card.take_written()
        {
            std::fs::write(&path, image)?;
            println!("Memory card {} saved to {path}", slot + 1);
        }
    }
    if let Ok(path) = std::env::var("WAV") {
        write_wav(&path, &sound)?;
        let peak = sound.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        println!("Sound: {} s, peak {peak}, saved to {path}", sound.len() / 2 / 48_000);
    }
    // USBLOG=1: the USB controller's first accesses.
    if std::env::var("USBLOG").is_ok() {
        println!("USB controller accesses ({}):", ps2.bus.iop.usb.log.len());
        for (write, offset, v) in &ps2.bus.iop.usb.log {
            println!("  {} {offset:02X} {v:08X}", if *write { "write" } else { "read " });
        }
    }
    println!("SIO2 transfers: {}", ps2.bus.iop.sio2.transfers);
    if std::env::var("SIO2TRACE").is_ok() {
        for (w, off, v) in &ps2.bus.iop.sio2.trace {
            println!("  SIO2 {} {off:02X} {v:08X}", if *w { "W" } else { "R" });
        }
    }
    if std::env::var("CARDLOG").is_ok() {
        for (port, send, receive, out, back) in &ps2.bus.iop.sio2.card_log {
            println!("  card port {port} {send}/{receive}: {out:02X?} -> {back:02X?}");
        }
    }
    for (port, send, receive, out, back) in &ps2.bus.iop.sio2.log {
        println!("  SIO2 port {port} {send}/{receive}: {out:02X?} -> {back:02X?}");
    }
    println!("CDVD S command writers: {:X?}", ps2.bus.iop.cdvd.s_callers);
    println!(
        "CDVD: {} sectors read; fast boot {}",
        ps2.bus.iop.cdvd.sectors_read,
        if ps2.boot_failed { "FAILED" } else { "ok (or off)" }
    );
    println!(
        "CDVD {}; DMA 3 {:08X?}",
        ps2.bus.iop.cdvd.state(),
        ps2.bus.iop.dma.ch[3]
    );
    if std::env::var("TLB").is_ok() {
        for (i, e) in ps2
            .ee
            .cop0
            .tlb
            .iter()
            .enumerate()
            .filter(|(_, e)| e.entry_lo0 | e.entry_lo1 != 0)
        {
            println!(
                "TLB {i:2}: mask {:08X} hi {:08X} lo0 {:08X} lo1 {:08X}",
                e.page_mask, e.entry_hi, e.entry_lo0, e.entry_lo1
            );
        }
    }
    if let Some(log) = &ps2.bus.iop.cdvd.read_log {
        for &(cmd, lsn, count, left, waiting) in log {
            if cmd == 0xFC {
                println!("    words {lsn:08X} {count:08X} {left:08X} {waiting:08X}");
                continue;
            }
            if cmd == 0xFD {
                println!(
                    "  SPU2DMA ch{left} {lsn:08X} BCR {count:08X} (auto {}, read {})",
                    waiting & 1,
                    waiting >> 1
                );
                continue;
            }
            if cmd == 0xFE {
                println!("  SIF0 {lsn:08X} {count} words to EE {left:08X} (EE tag {waiting:08X})");
                continue;
            }
            if cmd == 0xFF {
                println!("  DMA3 {lsn:08X} {count} bytes (BCR {left:08X}, waiting {waiting})");
                continue;
            }
            println!("  READ {cmd:02X} {lsn} {count} (previous left {left}, waiting {waiting})");
        }
    }
    if std::env::var("CDVDCMDS").is_ok() {
        for (madr, n, bcr) in &ps2.bus.iop.dma.cdvd_log {
            println!("  DMA3 {madr:08X} {n} bytes (BCR {bcr:08X})");
        }
        for (kind, cmd, params) in &ps2.bus.iop.cdvd.commands {
            println!("  {kind} {cmd:02X} {params:02X?}");
        }
    }
    if !ps2.bus.iop.cdvd.unknown_s.is_empty() {
        println!("CDVD S commands not emulated: {:X?}", ps2.bus.iop.cdvd.unknown_s);
    }
    if !ps2.bus.iop.unknown.is_empty() {
        println!("IOP: unknown addresses ({}):", ps2.bus.iop.unknown.len());
        for (addr, n) in ps2.bus.iop.unknown.iter().take(60) {
            println!("  {addr:08X}: {n}");
        }
    }
    if !ps2.bus.unknown.is_empty() {
        println!("Unknown addresses ({}):", ps2.bus.unknown.len());
        for (addr, n) in ps2.bus.unknown.iter().take(40) {
            println!("  {addr:08X}: {n}");
        }
    }
    Ok(())
}

/// Prints the complete lines of `text` past `done`, each with `prefix`.
fn print_lines(out: &mut impl Write, prefix: &str, text: &[u8], done: &mut usize) -> std::io::Result<()> {
    while let Some(end) = text[*done..].iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(&text[*done..*done + end]);
        writeln!(out, "{prefix}{}", line.trim_end_matches('\r'))?;
        *done += end + 1;
    }
    Ok(())
}

/// Writes RGBA pixels as a binary PPM.
fn write_ppm(path: &std::path::Path, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<()> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, out)
}

/// A disc image file.
struct FileImage(std::fs::File);

/// A zipped image (Redump's), unpacked: its largest `.iso` or `.bin`.
fn unzip_image(path: &str) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut best: Option<(usize, u64, String)> = None;
    for k in 0..archive.len() {
        let entry = archive.by_index(k)?;
        let name = entry.name()?.into_owned();
        let lower = name.to_ascii_lowercase();
        if (lower.ends_with(".iso") || lower.ends_with(".bin")) && best.as_ref().is_none_or(|b| entry.size() > b.1) {
            best = Some((k, entry.size(), name));
        }
    }
    let (k, size, name) = best.ok_or("no .iso or .bin in the zip")?;
    let mut image = Vec::with_capacity(size as usize);
    archive.by_index(k)?.read_to_end(&mut image)?;
    Ok((name, image))
}

/// An image in memory.
struct MemoryImage(Vec<u8>);

impl texel_disc::FileReader for MemoryImage {
    fn read_at(&mut self, _file: usize, offset: u64, buf: &mut [u8]) -> Result<(), texel_disc::ReadError> {
        let start = usize::try_from(offset).map_err(|_| texel_disc::ReadError::Io("past the end".into()))?;
        let part = self
            .0
            .get(start..start + buf.len())
            .ok_or_else(|| texel_disc::ReadError::Io("past the end".into()))?;
        buf.copy_from_slice(part);
        Ok(())
    }
}

impl texel_disc::FileReader for FileImage {
    fn read_at(&mut self, _file: usize, offset: u64, buf: &mut [u8]) -> Result<(), texel_disc::ReadError> {
        use std::io::{Read, Seek, SeekFrom};
        self.0
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.0.read_exact(buf))
            .map_err(|e| texel_disc::ReadError::Io(e.to_string()))
    }
}

/// Writes interleaved stereo 16-bit samples at 48 kHz as a WAV file.
fn write_wav(path: &str, samples: &[i16]) -> std::io::Result<()> {
    let data = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)
}

fn dump_draws(ps2: &mut Ps2, frames: u64) {
    use texel_ps2::gs::Command;
    ps2.bus.gs.rendering = texel_ps2::gs::Rendering::Record;
    ps2.bus.gs.take_commands();
    ps2.bus.vu1.kicks = Some(Vec::new());
    // VIFWATCH=from,to: VIF1's UNPACKs writing VU1 memory there
    // (quadwords, hex), and the programs started.
    ps2.bus.vif1.watch = std::env::var("VIFWATCH").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
    });
    ps2.bus.vu1.trace_from = std::env::var("PROG")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok());
    ps2.bus.gif.context_log = Some(Vec::new());
    ps2.bus.gif.find = std::env::var("FINDXY").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
    });
    ps2.run(frames * ps2.bus.frame_cycles());
    let commands = ps2.bus.gs.take_commands();
    let kicks = ps2.bus.vu1.kicks.take().unwrap_or_default();
    // The context's changes, and the draws near the origin in the main
    // view's (offset 0x7000) between them.
    let log = ps2.bus.gif.context_log.take().unwrap_or_default();
    println!("--- FRAME_1 / XYOFFSET_1 / TEX0_1 / TEX2_1 writes (path, register, value, at command):");
    let near = |c: &Command| {
        matches!(c, Command::Draw { regs, vertices, .. }
        if regs.xyoffset & 0xFFFF == 0x7000 && vertices.iter().any(|v| v.x < 0x100 || v.y < 0x100))
    };
    for (i, (path, r, v, at)) in log.iter().enumerate() {
        let until = log.get(i + 1).map_or(commands.len(), |l| l.3);
        let bad = commands[(*at).min(commands.len())..until.min(commands.len())]
            .iter()
            .filter(|c| near(c))
            .count();
        println!(
            "  path {} reg {r:#04x} {v:#018x} at {at}: then {} commands, {bad} near the origin",
            path + 1,
            until.saturating_sub(*at)
        );
    }
    // By program: packets, vertices, and vertices far off the screen's area
    // (window coordinates under 0x100 or over 0xF000: not ones the games'
    // offsets around 0x7000-0x9000 give).
    let find: Option<(u32, u32)> = std::env::var("FINDXY").ok().and_then(|v| {
        let (a, b) = v.split_once(',')?;
        Some((u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?))
    });
    if find.is_some() {
        println!(
            "FOUND on paths {:?}",
            ps2.bus.gif.found.iter().map(|p| p + 1).collect::<Vec<_>>()
        );
    }
    if let Some((fx, fy)) = find {
        for k in &kicks {
            if let Some(j) = k.packet.iter().position(|q| q[0] & 0xFFFF == fx && q[1] & 0xFFFF == fy) {
                println!(
                    "FOUND in program {:#x}, XGKICK at {:#x} from {:#x}, quadword {j} of {}",
                    k.start,
                    k.pc,
                    k.addr,
                    k.packet.len()
                );
            }
        }
    }
    let mut by_program: BTreeMap<u32, (u32, u32, u32)> = BTreeMap::new();
    let mut shown = 0;
    // KICKZ=hex: show (as a bad packet) the first ones with a vertex at
    // that depth, with PROG's trace.
    let kickz = std::env::var("KICKZ")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok());
    // KICKJUMP=hex: those where a vertex is further than that (12.4 fixed
    // point) from the one before it, in x or y.
    let kickjump = std::env::var("KICKJUMP")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok());
    for k in &kicks {
        let e = by_program.entry(k.start).or_default();
        e.0 += 1;
        let mut i = 0;
        let mut bad_here = 0;
        let mut last: Option<(u32, u32)> = None;
        while i < k.packet.len() {
            let tag = k.packet[i];
            let lo = u64::from(tag[0]) | (u64::from(tag[1]) << 32);
            let regs = u64::from(tag[2]) | (u64::from(tag[3]) << 32);
            let nloop = (lo & 0x7FFF) as usize;
            let nreg = match (lo >> 60) & 15 {
                0 => 16,
                n => n as usize,
            };
            let format = (lo >> 58) & 3;
            i += 1;
            if format != 0 {
                i += if format == 1 { (nloop * nreg).div_ceil(2) } else { nloop };
                continue;
            }
            for l in 0..nloop {
                for r in 0..nreg {
                    let Some(q) = k.packet.get(i + l * nreg + r) else { break };
                    let desc = (regs >> (4 * r)) & 15;
                    if desc == 4 || desc == 5 {
                        e.1 += 1;
                        let (x, y) = (q[0] & 0xFFFF, q[1] & 0xFFFF);
                        if !(0x100..=0xF000).contains(&x) || !(0x100..=0xF000).contains(&y) {
                            e.2 += 1;
                            bad_here += 1;
                        }
                        // Packed XYZF2 (4) holds Z in bits 4-27; XYZ2 (5) in all.
                        let z = if desc == 4 { (q[2] >> 4) & 0xFF_FFFF } else { q[2] };
                        if kickz == Some(z) {
                            bad_here += 1;
                        }
                        if let (Some(jump), Some((lx, ly))) = (kickjump, last)
                            && (x.abs_diff(lx) > jump || y.abs_diff(ly) > jump)
                        {
                            bad_here += 1;
                        }
                        last = Some((x, y));
                    }
                }
            }
            i += nloop * nreg;
        }
        let want = std::env::var("PROG")
            .ok()
            .and_then(|v| u32::from_str_radix(&v, 16).ok());
        if bad_here > 0 && shown < 2 && want.is_none_or(|w| w == k.start) {
            shown += 1;
            println!(
                "  bad packet: program {:#x}, XGKICK at {:#x}, from {:#x}, {} quadwords:",
                k.start,
                k.pc,
                k.addr,
                k.packet.len()
            );
            for (j, q) in k.packet.iter().take(24).enumerate() {
                println!("    {j:3}: {:08x} {:08x} {:08x} {:08x}", q[0], q[1], q[2], q[3]);
            }
            if shown == 1 {
                for pc in 0..k.micro.len() / 8 {
                    let at = pc * 8;
                    let lo = u32::from_le_bytes(k.micro[at..at + 4].try_into().unwrap_or_default());
                    let hi = u32::from_le_bytes(k.micro[at + 4..at + 8].try_into().unwrap_or_default());
                    println!("KCODE {pc:04x} {hi:08x} {lo:08x}");
                }
                for (pc, vi) in &k.trace {
                    println!(
                        "TRACE {:04x} {}",
                        pc & 0x7FF,
                        vi.iter().map(|v| format!("{v:04x}")).collect::<Vec<_>>().join(" ")
                    );
                }
            }
        }
    }
    if let Some(w) = std::env::var("PROG")
        .ok()
        .and_then(|v| u32::from_str_radix(&v, 16).ok())
    {
        // The program's code, to its end (E bit) or 600 instructions.
        let micro = &ps2.bus.vu1.micro;
        println!("--- code of {w:#x}:");
        for pc in 0..micro.len() as u32 / 8 {
            let at = pc as usize * 8;
            let lo = u32::from_le_bytes(micro[at..at + 4].try_into().unwrap_or_default());
            let hi = u32::from_le_bytes(micro[at + 4..at + 8].try_into().unwrap_or_default());
            println!("CODE {pc:04x} {hi:08x} {lo:08x}");
        }
    }
    if let Some(log) = &ps2.bus.gs.transfer_log {
        println!(
            "--- GS transfers ({}), in order (repeats folded): dir, to (base width format x y w h), \
             from (base width format), data words (non-zero), the data's EE address (GIF DMA)",
            log.len()
        );
        let mut k = 0;
        while k < log.len() {
            let t = log[k];
            let mut n = 1;
            while k + n < log.len() && log[k + n] == t {
                n += 1;
            }
            println!(
                "  {n:4}x dir {} to {:#06x} w{} psm {:#04x} ({},{}) {}x{} from {:#06x} w{} psm {:#04x}: {} words ({} non-zero) at {:#010x}",
                t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7], t[8], t[9], t[10], t[11], t[12], t[13]
            );
            k += n;
        }
    }
    if ps2.bus.vif1.watch.is_some() {
        println!("--- VIF1 writes watched ({}):", ps2.bus.vif1.watched.len());
        for line in &ps2.bus.vif1.watched {
            println!("{line}");
        }
    }
    println!("--- VU1 programs (start: packets, vertices, off-screen ones)");
    for (start, (n, v, bad)) in &by_program {
        println!("  {start:#06x}: {n} packets, {v} vertices, {bad} off");
    }
    let mut positions: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut big = Vec::new();
    let mut kinds = [0u32; 8];
    // Draw contexts (FRAME, XYOFFSET, SCISSOR): draws, and ones with
    // vertices near the origin.
    let mut contexts: BTreeMap<(u64, u64, u64), (u32, u32)> = BTreeMap::new();
    for c in &commands {
        if let Command::Draw { kind, regs, vertices } = c {
            let e = contexts.entry((regs.frame, regs.xyoffset, regs.scissor)).or_default();
            e.0 += 1;
            if vertices.iter().any(|v| v.x < 0x100 || v.y < 0x100) {
                e.1 += 1;
            }
            kinds[*kind as usize & 7] += 1;
            for v in vertices {
                *positions.entry((v.x, v.y)).or_default() += 1;
            }
            let (ox, oy) = ((regs.xyoffset & 0xFFFF) as i64, ((regs.xyoffset >> 32) & 0xFFFF) as i64);
            let xs = vertices.iter().map(|v| (i64::from(v.x) - ox) / 16);
            let ys = vertices.iter().map(|v| (i64::from(v.y) - oy) / 16);
            let w = xs.clone().max().unwrap_or(0) - xs.min().unwrap_or(0);
            let h = ys.clone().max().unwrap_or(0) - ys.min().unwrap_or(0);
            if *kind != 6 && w.max(h) > 400 {
                big.push((w.max(h), *kind, regs.prim, vertices.clone(), regs.xyoffset));
            }
        }
    }
    println!(
        "--- draws over {frames} frames: {} commands, by kind {kinds:?}",
        commands.len()
    );
    // DRAWSTATE=1: the draws by their state (kind, PRIM's flags, TEX0,
    // TEX1, ALPHA, TEST, FRAME), with the first vertices of each group (DRAWSTATE=n: up to n, 12 by default).
    if let Ok(limit) = std::env::var("DRAWSTATE") {
        // DRAWSTATE=n: up to n vertices of each group (12 otherwise).
        let limit: usize = limit.parse().unwrap_or(12);
        type State = (u32, u64, u64, u64, u64, u64, u64);
        let mut groups: std::collections::BTreeMap<State, (usize, Vec<texel_ps2::gs::Vertex>)> =
            std::collections::BTreeMap::new();
        for c in &commands {
            if let Command::Draw { kind, regs, vertices } = c {
                let key = (
                    *kind,
                    regs.prim & 0x7F8,
                    regs.tex0,
                    regs.tex1,
                    regs.alpha,
                    regs.test,
                    regs.frame,
                );
                let g = groups.entry(key).or_insert((0, Vec::new()));
                g.0 += 1;
                if g.1.len() < limit {
                    g.1.extend(vertices.iter().copied());
                }
            }
        }
        let mut list: Vec<_> = groups.into_iter().collect();
        list.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        println!("--- draws by state ({} states):", list.len());
        for ((kind, flags, tex0, tex1, alpha, test, frame), (n, vs)) in list.iter().take(24) {
            println!(
                "  {n:5} kind {kind} prim flags {flags:#05x} TEX0 {tex0:#018x} TEX1 {tex1:#x} ALPHA {alpha:#012x} TEST {test:#07x} FRAME {frame:#012x}"
            );
            for v in vs {
                println!(
                    "        vertex xy {:.2},{:.2} z {:#x} rgba {:?} st {:.4},{:.4} q {:.3} uv {:.2},{:.2}",
                    f64::from(v.x) / 16.0,
                    f64::from(v.y) / 16.0,
                    v.z,
                    v.rgba,
                    v.s,
                    v.t,
                    v.q,
                    f64::from(v.u) / 16.0,
                    f64::from(v.v) / 16.0
                );
            }
        }
    }
    let mut ctx: Vec<_> = contexts.into_iter().collect();
    ctx.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    for ((frame, off, sc), (n, near)) in ctx.iter().take(14) {
        println!(
            "  FRAME {frame:#012x} XYOFFSET {:#x},{:#x} SCISSOR {sc:#018x}: {n} draws, {near} near the origin",
            off & 0xFFFF,
            (off >> 32) & 0xFFFF
        );
    }
    let mut common: Vec<_> = positions.into_iter().collect();
    common.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for ((x, y), n) in common.iter().take(8) {
        println!("  vertex at ({x:#06x}, {y:#06x}) x{n}");
    }
    big.sort_by_key(|b| std::cmp::Reverse(b.0));
    println!("  {} primitives over 400 pixels; the largest:", big.len());
    for (size, kind, prim, vs, off) in big.iter().take(6) {
        println!(
            "  kind {kind} prim {prim:#x} size {size} offset {:#x},{:#x}:",
            off & 0xFFFF,
            (off >> 32) & 0xFFFF
        );
        for v in vs {
            println!(
                "    x {:#06x} y {:#06x} z {:#010x} q {} rgba {:?}",
                v.x, v.y, v.z, v.q, v.rgba
            );
        }
    }
}
