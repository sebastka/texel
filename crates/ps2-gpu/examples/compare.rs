//! Boots a PlayStation 2 BIOS with the GPU GS, and compares it with the
//! software GS:
//!
//! ```sh
//! cargo run --release -p texel-ps2-gpu --example compare -- BIOS [SECONDS] [OUT_DIR]
//! ```
//!
//! Every second, the number of GS memory words that differ, and both
//! displayed frames as PPM in OUT_DIR. With `GPU_ONLY=1`, the GPU alone
//! renders, for its speed. `PRESS="16:cross"`, `DISC=image.iso` and
//! `MC1=card.ps2` as for the boot example.

use texel_ps2::Ps2;
use texel_ps2::bus::{EE_CLOCK, FRAME_CYCLES};
use texel_ps2::gs::Rendering;
use texel_ps2_gpu::GpuGs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let bios = std::fs::read(args.next().ok_or("usage: compare BIOS [SECONDS] [OUT_DIR]")?)?;
    let seconds: f64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let out = args.next().map(std::path::PathBuf::from);
    if let Some(dir) = &out {
        std::fs::create_dir_all(dir)?;
    }
    let gpu_only = std::env::var("GPU_ONLY").is_ok();
    let presses: Vec<(f64, u16)> = std::env::var("PRESS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| {
            let (at, name) = p.split_once(':')?;
            let b = match name {
                "cross" => texel_ps1::button::CROSS,
                "circle" => texel_ps1::button::CIRCLE,
                "down" => texel_ps1::button::DOWN,
                "up" => texel_ps1::button::UP,
                "left" => texel_ps1::button::LEFT,
                "right" => texel_ps1::button::RIGHT,
                "start" => texel_ps1::button::START,
                "triangle" => texel_ps1::button::TRIANGLE,
                "square" => texel_ps1::button::SQUARE,
                _ => return None,
            };
            Some((at.parse().ok()?, b))
        })
        .collect();
    let mut gpu = GpuGs::new_blocking()?;
    println!("GPU: {}", gpu.adapter);
    let mut ps2 = Ps2::new(&bios);
    ps2.bus.gs.rendering = if gpu_only { Rendering::Record } else { Rendering::Both };
    if let Ok(path) = std::env::var("DISC") {
        let size = std::fs::metadata(&path)?.len();
        let layout = texel_disc::Layout::single_file(&path, size).ok_or("not a disc image")?;
        let medium = texel_disc::Disc::new(layout, Box::new(FileImage(std::fs::File::open(&path)?)) as _);
        println!("Disc: {}", ps2.insert_disc(medium, true)?.boot);
    }
    let card = std::env::var("MC1").ok();
    if let Some(path) = &card {
        let image = std::fs::read(path).ok();
        ps2.bus.iop.sio2.cards[0] = Some(Box::new(texel_ps2::memcard::MemoryCard::new(image.as_deref())));
    }
    let frames = (seconds * 60.0) as u64;
    let started = std::time::Instant::now();
    let mut soft = Vec::new();
    for frame in 1..=frames {
        let now = frame as f64 / 60.0;
        let held = presses
            .iter()
            .filter(|(at, _)| now >= *at && now < at + 0.2)
            .fold(0, |acc, (_, b)| acc | b);
        ps2.set_pad(0, held, [0x80; 4]);
        ps2.run(FRAME_CYCLES);
        gpu.submit(&ps2.bus.gs.take_commands());
        if frame % 60 != 0 {
            continue;
        }
        let second = frame / 60;
        let elapsed = started.elapsed().as_secs_f64();
        let speed = (frame * FRAME_CYCLES) as f64 / elapsed / 1e6;
        if gpu_only {
            println!(
                "{second:3} s: {speed:.0} MHz ({:.1} fps), {} batches, {} primitives",
                frame as f64 / elapsed,
                gpu.batches,
                gpu.primitives
            );
        } else {
            let vram = gpu.read_vram();
            let differ = vram
                .iter()
                .zip(ps2.bus.gs.mem.words.iter())
                .filter(|(a, b)| a != b)
                .count();
            println!(
                "{second:3} s: {differ} words differ ({:.3}%)",
                differ as f64 * 100.0 / vram.len() as f64
            );
            if std::env::var_os("PAGES").is_some() {
                // Differing words per 2048-word page, and the largest difference
                // of a byte in it.
                for (page, (a, b)) in vram.chunks(2048).zip(ps2.bus.gs.mem.words.chunks(2048)).enumerate() {
                    let n = a.iter().zip(b).filter(|(x, y)| x != y).count();
                    if n > 0 {
                        let most = a
                            .iter()
                            .zip(b)
                            .flat_map(|(x, y)| {
                                (0..4).map(move |i| ((x >> (i * 8)) as u8).abs_diff((y >> (i * 8)) as u8))
                            })
                            .max()
                            .unwrap_or(0);
                        println!("      page {page:#x}: {n} words, up to {most}");
                    }
                }
            }
        }
        if let Some(dir) = &out {
            if let Some((w, h, rgba)) = gpu.display(&ps2.bus.gs.privileged) {
                write_ppm(&dir.join(format!("gpu-{second:03}.ppm")), w, h, &rgba)?;
            }
            if !gpu_only {
                let (w, h) = ps2.bus.gs.display(&mut soft);
                if w > 0 {
                    write_ppm(&dir.join(format!("soft-{second:03}.ppm")), w, h, &soft)?;
                }
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{seconds} s in {elapsed:.1} s: {:.0} MHz",
        (frames * FRAME_CYCLES) as f64 / elapsed / 1e6
    );
    let _ = EE_CLOCK;
    if let (Some(path), Some(c)) = (&card, ps2.bus.iop.sio2.cards[0].as_mut())
        && let Some(image) = c.take_written()
    {
        std::fs::write(path, image)?;
        println!("Memory card saved to {path}");
    }
    Ok(())
}

fn write_ppm(path: &std::path::Path, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<()> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, out)
}

/// A disc image file.
struct FileImage(std::fs::File);

impl texel_disc::FileReader for FileImage {
    fn read_at(&mut self, _file: usize, offset: u64, buf: &mut [u8]) -> Result<(), texel_disc::ReadError> {
        use std::io::{Read, Seek, SeekFrom};
        self.0
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.0.read_exact(buf))
            .map_err(|e| texel_disc::ReadError::Io(e.to_string()))
    }
}
