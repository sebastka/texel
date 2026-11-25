//! Runs a game twice, in step: drawing at a higher resolution in software,
//! and recording for the GPU renderer; compares the displayed pictures
//! every second (they must be the same), and writes both when they differ.
//!
//! ```sh
//! cargo run --release -p texel-ps1-gpu --example compare -- BIOS DISC.cue [FRAMES] [SCALE] [OUT_DIR]
//! ```
//!
//! `GPU_ONLY=1` runs only the recording console, to measure it; `PRESS`
//! as the PlayStation's headless example (frame:button, held 6 frames).
//! `PRECISE=1` and `WIDESCREEN=1` turn the enhancements on for the GPU
//! (which then draws otherwise: its pictures are written every second,
//! with how many triangles were precise, not compared). `SHOTS=1` writes
//! the GPU's pictures every second in any case (with `GPU_ONLY`).
//! `SOFT=1` replays the recording in software (`gpu_record::Replay`), as a
//! browser's drawing worker does, rather than on the GPU.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use texel_disc::{Disc, FileReader, ReadError, parse_cue};
use texel_ps1::System;
use texel_ps1_gpu::GpuRenderer;

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
    let text = std::fs::read_to_string(path)?;
    let layout = parse_cue(&text, |name| std::fs::metadata(dir.join(name)).ok().map(|m| m.len()))?;
    let files = layout
        .files
        .iter()
        .map(|f| File::open(dir.join(f)))
        .collect::<Result<_, _>>()?;
    Ok(Disc::new(layout, Box::new(Files(files)) as Box<dyn FileReader>))
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
    let usage = "usage: compare BIOS DISC.cue [FRAMES] [SCALE] [OUT_DIR]";
    let bios = std::fs::read(args.get(1).ok_or(usage)?)?;
    let disc = PathBuf::from(args.get(2).ok_or(usage)?);
    let frames: u32 = args.get(3).map_or(Ok(1200), |f| f.parse())?;
    let scale: usize = args.get(4).map_or(Ok(2), |f| f.parse())?;
    let out = PathBuf::from(args.get(5).map_or(".", String::as_str));
    let gpu_only = std::env::var("GPU_ONLY").is_ok();
    let presses: Vec<(u32, u16)> = std::env::var("PRESS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| {
            let (frame, name) = p.split_once(':')?;
            let b = match name {
                "cross" => texel_ps1::button::CROSS,
                "circle" => texel_ps1::button::CIRCLE,
                "start" => texel_ps1::button::START,
                "up" => texel_ps1::button::UP,
                "down" => texel_ps1::button::DOWN,
                "left" => texel_ps1::button::LEFT,
                "right" => texel_ps1::button::RIGHT,
                _ => return None,
            };
            Some((frame.parse().ok()?, b))
        })
        .collect();
    let soft = std::env::var("SOFT").is_ok();
    let mut gpu = if soft { None } else { Some(GpuRenderer::new_blocking()?) };
    let mut replay = texel_ps1::gpu_record::Replay::new();
    let mut replayed = Vec::new();
    if let Some(g) = &gpu {
        println!("GPU: {}", g.adapter);
    }
    let mut recorded = System::new(&bios)?;
    recorded.insert_disc(Some(open_disc(&disc)?));
    recorded.set_resolution_scale(scale);
    recorded.set_gpu_recording(true);
    let enhanced = std::env::var("PRECISE").is_ok() || std::env::var("WIDESCREEN").is_ok();
    recorded.set_precise_geometry(std::env::var("PRECISE").is_ok());
    recorded.set_widescreen(std::env::var("WIDESCREEN").is_ok());
    let (mut precise, mut rounded) = (0u64, 0u64);
    let mut software = if gpu_only || enhanced {
        None
    } else {
        let mut s = System::new(&bios)?;
        s.insert_disc(Some(open_disc(&disc)?));
        s.set_resolution_scale(scale);
        Some(s)
    };
    let started = std::time::Instant::now();
    let mut differing = 0;
    for n in 1..=frames {
        let held = presses
            .iter()
            .filter(|(f, _)| (*f..*f + 6).contains(&n))
            .fold(0, |a, (_, b)| a | b);
        recorded.set_buttons(0, held);
        recorded.run_frame();
        let commands = if soft {
            recorded.take_gpu_commands_for_replay()
        } else {
            recorded.take_gpu_commands()
        };
        for c in &commands {
            match c {
                texel_ps1::gpu_record::Command::Precise { .. } => precise += 1,
                texel_ps1::gpu_record::Command::Triangle { .. } => rounded += 1,
                _ => {}
            }
        }
        match gpu.as_mut() {
            Some(g) => g.submit(&commands),
            None => replay.apply(&commands),
        }
        if let Some(s) = software.as_mut() {
            s.set_buttons(0, held);
            s.run_frame();
        }
        if n % 60 != 0 && n != frames {
            continue;
        }
        let picture = match gpu.as_ref() {
            Some(g) => g.display(recorded.gpu_display(), recorded.vram()),
            None => {
                let (w, h) = replay.render_display(recorded.gpu_display(), &mut replayed);
                (w > 0 && h > 0).then(|| (w, h, replayed.clone()))
            }
        };
        let Some(s) = software.as_mut() else {
            if (enhanced || std::env::var("SHOTS").is_ok())
                && let Some((w, h, g)) = &picture
            {
                write_ppm(&out.join(format!("gpu-{n:05}.ppm")), g, *w, *h)?;
                println!("frame {n}: triangles {precise} precise, {rounded} rounded");
            }
            let fps = f64::from(n) / started.elapsed().as_secs_f64();
            let (batches, primitives) = gpu.as_ref().map_or((0, 0), |g| (g.batches, g.primitives));
            println!("frame {n}: {fps:.0} fps, {batches} batches, {primitives} primitives");
            continue;
        };
        let (rgba, w, h) = s.frame();
        match picture {
            Some((gw, gh, g)) if (gw, gh) == (w, h) => {
                let diff = g
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(rgba.as_chunks::<4>().0)
                    .filter(|(a, b)| a != b)
                    .count();
                if diff == 0 {
                    println!("frame {n}: {w}x{h}, same");
                } else {
                    differing += 1;
                    println!("frame {n}: {w}x{h}, {diff} pixels differ");
                    write_ppm(&out.join(format!("gpu-{n:05}.ppm")), &g, gw, gh)?;
                    write_ppm(&out.join(format!("sw-{n:05}.ppm")), rgba, w, h)?;
                }
            }
            Some((gw, gh, _)) => {
                differing += 1;
                println!("frame {n}: sizes differ: GPU {gw}x{gh}, software {w}x{h}");
            }
            None => println!("frame {n}: no picture on the GPU (software {w}x{h})"),
        }
    }
    println!(
        "{frames} frames in {:.1} s; {differing} differing",
        started.elapsed().as_secs_f64()
    );
    if differing > 0 {
        std::process::exit(1);
    }
    Ok(())
}
