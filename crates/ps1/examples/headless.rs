//! Runs the emulator without a frontend, for testing:
//!
//! ```sh
//! cargo run --release -p texel-ps1 --example headless -- BIOS [DISC.cue|DISC.iso|DISC.bin|PROGRAM.exe] [FRAMES] [OUT_DIR]
//! ```
//!
//! Writes a PPM screenshot every 60 frames, and the last one, to OUT_DIR.
//! `CDLOG=1` prints the CD-ROM's commands and the XA sectors played, with
//! the SPU's volumes every 250 frames.
//! `VRAM=FRAME` also writes all of VRAM at that frame (as 15-bit colours).
//! `SHOTS=FROM:TO` writes a screenshot of every frame in that range.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use texel_disc::{Disc, FileReader, Layout, ReadError, Subchannel, parse_cue};
use texel_ps1::System;

struct Files(Vec<File>);

impl FileReader for Files {
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let f = self
            .0
            .get_mut(file)
            .ok_or_else(|| ReadError::Io("no such file".into()))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| ReadError::Io(e.to_string()))?;
        f.read_exact(buf).map_err(|e| ReadError::Io(e.to_string()))
    }
}

fn open_disc(path: &Path) -> Result<texel_ps1::DiscImage, Box<dyn std::error::Error>> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let layout = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
        let text = std::fs::read_to_string(path)?;
        parse_cue(&text, |name| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))?
    } else {
        let name = path.file_name().and_then(|n| n.to_str()).ok_or("bad file name")?;
        Layout::single_file(name, std::fs::metadata(path)?.len()).ok_or("unrecognised image size")?
    };
    let files = layout
        .files
        .iter()
        .map(|f| File::open(dir.join(f)))
        .collect::<Result<_, _>>()?;
    let mut disc = Disc::new(layout, Box::new(Files(files)) as Box<dyn FileReader>);
    // A protected disc's subchannel data: DISC.sbi next to DISC.cue.
    let sbi = path.with_extension("sbi");
    if sbi.exists() {
        let subchannel = Subchannel::parse_sbi(&std::fs::read(&sbi)?)?;
        println!("SBI: {} sectors", subchannel.len());
        disc = disc.with_subchannel(subchannel);
    }
    Ok(disc)
}

fn write_ppm(path: &Path, rgba: &[u8], w: usize, h: usize) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    write!(f, "P6\n{w} {h}\n255\n")?;
    let rgb: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    f.write_all(&rgb)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let bios = std::fs::read(
        args.get(1)
            .ok_or("usage: headless BIOS [DISC|EXE] [FRAMES] [OUT_DIR]")?,
    )?;
    let mut system = System::new(&bios)?;
    println!("BIOS: {:?}", system.bios_info());
    if let Some(path) = args.get(2).filter(|p| !p.is_empty()).map(PathBuf::from) {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
            system.sideload_exe(std::fs::read(&path)?)?;
        } else {
            let mut disc = open_disc(&path)?;
            match texel_disc::read_system_cnf(&mut disc) {
                Ok(cnf) => println!("Disc: {cnf:?}"),
                Err(e) => println!("Disc: {e}"),
            }
            system.insert_disc(Some(disc));
        }
    }
    // LOAD=state.bin: starts from a save state (uncompressed).
    if let Ok(path) = std::env::var("LOAD") {
        system.load_state(&std::fs::read(path)?)?;
        println!("loaded the state");
    }
    if let Ok(letter) = std::env::var("LICENCE") {
        system.debug_set_licence_letter(letter.as_bytes().first().copied().unwrap_or(b'A'));
    }
    if let Ok(scale) = std::env::var("SCALE") {
        system.set_resolution_scale(scale.parse()?);
    }
    let frames: u32 = args.get(3).map_or(Ok(600), |f| f.parse())?;
    let out = PathBuf::from(args.get(4).map_or(".", String::as_str));
    let start = std::time::Instant::now();
    let mut audio = Vec::new();
    // PRESS="1900:cross,2100:start,2500:square:300": hold a button from a
    // frame, for 6 frames or the number given.
    let presses: Vec<(u32, u16, u32)> = std::env::var("PRESS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| {
            let mut parts = p.split(':');
            let frame = parts.next()?.parse().ok()?;
            use texel_ps1::button::*;
            let b = match parts.next()? {
                "cross" => CROSS,
                "circle" => CIRCLE,
                "square" => SQUARE,
                "triangle" => TRIANGLE,
                "start" => START,
                "select" => SELECT,
                "up" => UP,
                "down" => DOWN,
                "left" => LEFT,
                "right" => RIGHT,
                "l1" => L1,
                "r1" => R1,
                "l2" => L2,
                "r2" => R2,
                _ => return None,
            };
            let len = parts.next().map_or(Some(6), |l| l.parse().ok())?;
            Some((frame, b, len))
        })
        .collect();
    let shots: Option<(u32, u32)> = std::env::var("SHOTS").ok().and_then(|s| {
        let (from, to) = s.split_once(':')?;
        Some((from.parse().ok()?, to.parse().ok()?))
    });
    // SWAP="3000:3060": open the lid at a frame, close it on the disc again later.
    let swap: Option<(u32, u32)> = std::env::var("SWAP").ok().and_then(|s| {
        let (a, b) = s.split_once(':')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let disc_path = args.get(2).filter(|p| !p.is_empty()).map(PathBuf::from);
    let state_frames: Option<(u32, u32)> = std::env::var("STATE").ok().and_then(|s| {
        let (a, b) = s.split_once(':')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let mut saved: Option<Vec<u8>> = None;
    let mut peak_per_second = Vec::new();
    let mut flips_per_second = Vec::new();
    // Pictures that changed from the frame before (whatever the game does
    // with display areas: interlaced games draw into one).
    let (mut last_picture, mut changed, mut changes_per_second) = (0u64, 0u32, Vec::new());
    for n in 1..=frames {
        let held = presses
            .iter()
            .filter(|(f, _, len)| (*f..*f + len).contains(&n))
            .fold(0, |a, (_, b, _)| a | b);
        system.set_buttons(0, held);
        // ANALOG=600: press the Analog button at that frame (analog mode).
        if std::env::var("ANALOG").is_ok_and(|f| f.parse() == Ok(n)) {
            println!("analog mode: {}", system.press_analog_button(0));
        }
        if let (Some((open, close)), Some(path)) = (swap, &disc_path) {
            if n == open {
                system.open_lid();
            }
            if n == close {
                system.insert_disc(Some(open_disc(path)?));
            }
        }
        // WATCH=8008dabc: the CPU's stores to that word, by frame.
        if n == 1
            && let Ok(w) = std::env::var("WATCH")
        {
            system.debug_watch(u32::from_str_radix(w.trim_start_matches("0x"), 16).ok());
        }
        for (pc, addr, v) in system.debug_watched() {
            println!("watch {n}: pc {pc:08x} stores {v:08x} at {addr:08x}");
        }
        // CDLOG=1: the CD-ROM's commands, by frame.
        if n == 1 && std::env::var("CDLOG").is_ok() {
            system.debug_cd_log();
        }
        system.run_frame();
        if std::env::var("CDLOG").is_ok() {
            for line in system.debug_cd_log() {
                println!("cd {n}: {line}");
            }
            if n % 250 == 0 {
                // SPUCNT, main volume, CD volume.
                let (cnt, main, cd) = (
                    system.debug_read32(0x1F80_1DA8),
                    system.debug_read32(0x1F80_1D80),
                    system.debug_read32(0x1F80_1DB0),
                );
                println!(
                    "spu {n}: SPUCNT {:04x} main volume {main:08x} CD volume {cd:08x}",
                    cnt >> 16
                );
            }
        }
        system.take_audio(&mut audio);
        let picture = system.frame().0.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        changed += u32::from(picture != last_picture);
        last_picture = picture;
        // STATE="3000:3300": save at a frame, reload at another (determinism check).
        if let Some((save_at, load_at)) = state_frames {
            if n == save_at {
                let t = std::time::Instant::now();
                saved = Some(system.save_state());
                let len = saved.as_ref().map_or(0, Vec::len);
                println!("saved state at frame {n}: {len} bytes in {:?}", t.elapsed());
            }
            if n == load_at
                && let Some(state) = &saved
            {
                system.load_state(state)?;
                println!("loaded state at frame {n}");
            }
        }
        // CHECKSUM=60: the machine's checksum (as netplay compares it) every
        // so many frames.
        if let Some(every) = std::env::var("CHECKSUM").ok().and_then(|v| v.parse::<u32>().ok())
            && n % every == 0
        {
            println!("checksum {n}: {:016x}", system.state_checksum());
        }
        if n % 50 == 0 {
            peak_per_second.push(audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0));
            audio.clear();
            flips_per_second.push(system.take_flips());
            changes_per_second.push(std::mem::take(&mut changed));
        }
        if n % 60 == 0 || n == frames || shots.is_some_and(|(from, to)| (from..=to).contains(&n)) {
            println!("frame {n}: {}", system.debug_summary());
            let (rgba, w, h) = system.frame();
            write_ppm(&out.join(format!("frame-{n:05}.ppm")), rgba, w, h)?;
        }
        // VRAM=1700: all of VRAM at that frame, as 15-bit colours.
        if std::env::var("VRAM").is_ok_and(|v| v.parse() == Ok(n)) {
            let rgba: Vec<u8> = system
                .vram()
                .iter()
                .flat_map(|&p| [(p << 3) as u8, ((p >> 5) << 3) as u8, ((p >> 10) << 3) as u8, 255])
                .collect();
            write_ppm(&out.join(format!("vram-{n:05}.ppm")), &rgba, 1024, 512)?;
        }
    }
    if let Ok(range) = std::env::var("DUMP") {
        let (a, n) = range.split_once(':').ok_or("DUMP=addr:count")?;
        let a = u32::from_str_radix(a.trim_start_matches("0x"), 16)?;
        for i in 0..n.parse::<u32>()? {
            println!("{:08x}: {:08x}", a + i * 4, system.debug_read32(a + i * 4));
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{frames} frames in {elapsed:.2} s ({:.0} fps), PAL: {}\naudio peak per 50 frames: {peak_per_second:?}\npage flips per 50 frames: {flips_per_second:?}\nchanged pictures per 50 frames: {changes_per_second:?}",
        f64::from(frames) / elapsed,
        system.is_pal()
    );
    Ok(())
}
