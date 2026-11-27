//! The GPU GS draws what the software GS draws: each test drives the
//! software GS in `Both` mode, replays its commands on the GPU, and compares
//! the whole GS memory. Without a GPU adapter, the tests pass and say so.

use texel_ps2::gs::mem::psm;
use texel_ps2::gs::{Gs, Rendering, reg};
use texel_ps2_gpu::GpuGs;

/// A 64×64 CT32 frame at page 0, no depth test, scissor over it all.
fn gs() -> Gs {
    let mut gs = Gs::default();
    gs.rendering = Rendering::Both;
    gs.write_reg(reg::FRAME_1, 1 << 16);
    gs.write_reg(reg::SCISSOR_1, (63 << 16) | (63 << 48));
    gs.write_reg(reg::TEST_1, 0);
    gs.write_reg(reg::PRMODECONT, 1);
    gs
}

fn xyz(x: u32, y: u32) -> u64 {
    u64::from(x * 16) | (u64::from(y * 16) << 16)
}

fn uv(u: u32, v: u32) -> u64 {
    u64::from(u * 16) | (u64::from(v * 16) << 16)
}

/// Uploads `data` (64-bit words) as a `w`×`h` image of `format` at block
/// `bp`, 64 pixels wide.
fn upload(gs: &mut Gs, bp: u64, format: u32, w: u64, h: u64, data: &[u64]) {
    gs.write_reg(reg::BITBLTBUF, (bp << 32) | (1 << 48) | (u64::from(format) << 56));
    gs.write_reg(reg::TRXPOS, 0);
    gs.write_reg(reg::TRXREG, w | (h << 32));
    gs.write_reg(reg::TRXDIR, 0);
    for &d in data {
        gs.write_reg(reg::HWREG, d);
    }
}

/// Replays the software GS's commands on the GPU and compares memories.
fn check(mut gs: Gs) {
    let mut gpu = match GpuGs::new_blocking() {
        Ok(gpu) => gpu,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    gpu.submit(&gs.take_commands());
    let vram = gpu.read_vram();
    let differ: Vec<usize> = (0..vram.len()).filter(|&i| vram[i] != gs.mem.words[i]).collect();
    let mut pages: Vec<usize> = differ.iter().map(|i| i / 2048).collect();
    pages.dedup();
    assert!(
        differ.is_empty(),
        "{} words differ, in pages {pages:?}, first at {:#x}: GPU {:#010x}, software {:#010x}",
        differ.len(),
        differ[0],
        vram[differ[0]],
        gs.mem.words[differ[0]]
    );
}

#[test]
fn flat_sprites_triangles_and_lines() {
    let mut gs = gs();
    gs.write_reg(reg::PRIM, 6);
    gs.write_reg(reg::RGBAQ, 0x8011_2233);
    gs.write_reg(reg::XYZ2, xyz(2, 2));
    gs.write_reg(reg::XYZ2, xyz(30, 14));
    gs.write_reg(reg::PRIM, 3);
    gs.write_reg(reg::RGBAQ, 0x80FF_0000);
    gs.write_reg(reg::XYZ2, xyz(10, 10));
    gs.write_reg(reg::XYZ2, xyz(10, 50));
    gs.write_reg(reg::XYZ2, xyz(60, 40));
    gs.write_reg(reg::PRIM, 1);
    gs.write_reg(reg::RGBAQ, 0x8000_FF00);
    gs.write_reg(reg::XYZ2, xyz(0, 63));
    gs.write_reg(reg::XYZ2, xyz(63, 0));
    check(gs);
}

#[test]
fn alpha_blending() {
    let mut gs = gs();
    gs.write_reg(reg::PRIM, 6);
    gs.write_reg(reg::RGBAQ, 0x8000_00C8);
    gs.write_reg(reg::XYZ2, xyz(0, 0));
    gs.write_reg(reg::XYZ2, xyz(32, 32));
    // (Cs - Cd) * As + Cd, over the first sprite and beyond.
    gs.write_reg(reg::ALPHA_1, 0b01_00_01_00);
    gs.write_reg(reg::PRIM, 6 | (1 << 6));
    gs.write_reg(reg::RGBAQ, 0x4030_6090);
    gs.write_reg(reg::XYZ2, xyz(16, 16));
    gs.write_reg(reg::XYZ2, xyz(48, 48));
    check(gs);
}

#[test]
fn textured_sprites() {
    let mut gs = gs();
    // An 8×8 CT32 texture at page 4, drawn 1:1 then stretched.
    let texels: Vec<u64> = (0..32u64)
        .map(|i| (0x8000_0000 | (i * 0x0102_0304)) | ((0x8000_0000 | (i * 0x0102_0304 + 0x10_2030)) << 32))
        .collect();
    upload(&mut gs, 128, psm::CT32, 8, 8, &texels);
    // TBP0 128, TBW 1, CT32, 8×8 (TW = TH = 3), TCC, decal.
    gs.write_reg(
        reg::TEX0_1,
        128 | (1 << 14) | (3 << 26) | (3 << 30) | (1 << 34) | (1 << 35),
    );
    // Sprite, textured (TME), UV coordinates (FST).
    gs.write_reg(reg::PRIM, 6 | (1 << 4) | (1 << 8));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(0, 0));
    gs.write_reg(reg::UV, uv(8, 8));
    gs.write_reg(reg::XYZ2, xyz(8, 8));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(16, 16));
    gs.write_reg(reg::UV, uv(8, 8));
    gs.write_reg(reg::XYZ2, xyz(48, 40));
    check(gs);
}

#[test]
fn paletted_texture() {
    let mut gs = gs();
    // A 256-colour CT32 palette at page 8, 16×16 entries.
    let clut: Vec<u64> = (0..128u64)
        .map(|i| (0x8000_0000 | (i * 0x0203_0405)) | ((0x8000_0000 | (i * 0x0203_0405 + 0x01_0101)) << 32))
        .collect();
    upload(&mut gs, 256, psm::CT32, 16, 16, &clut);
    // A 16×16 PSMT8 texture at page 12.
    let indices: Vec<u64> = (0..32u64)
        .map(|i| {
            u64::from_le_bytes(std::array::from_fn(|k| {
                (i as u8).wrapping_mul(8).wrapping_add((k as u8).wrapping_mul(37))
            }))
        })
        .collect();
    upload(&mut gs, 384, psm::T8, 16, 16, &indices);
    // TBP0 384, TBW 1, T8, 16×16, TCC, decal, CBP 256, CT32, load (CLD 1).
    let tex0 = 384
        | (1 << 14)
        | (u64::from(psm::T8) << 20)
        | (4 << 26)
        | (4 << 30)
        | (1 << 34)
        | (1 << 35)
        | (256 << 37)
        | (1 << 61);
    gs.write_reg(reg::TEX0_1, tex0);
    gs.write_reg(reg::PRIM, 6 | (1 << 4) | (1 << 8));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(4, 4));
    gs.write_reg(reg::UV, uv(16, 16));
    gs.write_reg(reg::XYZ2, xyz(36, 36));
    check(gs);
}

#[test]
fn local_to_local_copy() {
    let mut gs = gs();
    gs.write_reg(reg::PRIM, 6);
    gs.write_reg(reg::RGBAQ, 0x8044_5566);
    gs.write_reg(reg::XYZ2, xyz(0, 0));
    gs.write_reg(reg::XYZ2, xyz(16, 8));
    // Copy (0, 0)-(16, 8) to (20, 30), in the same buffer.
    gs.write_reg(reg::BITBLTBUF, (1 << 16) | (1 << 48));
    gs.write_reg(reg::TRXPOS, (20 << 32) | (30 << 48));
    gs.write_reg(reg::TRXREG, 16 | (8 << 32));
    gs.write_reg(reg::TRXDIR, 2);
    check(gs);
}

#[test]
fn a_state_reads_back_and_restores() {
    let Ok(mut gpu) = GpuGs::new_blocking() else {
        eprintln!("skipped: no GPU");
        return;
    };
    let mut gs = gs();
    let clut: Vec<u64> = (0..128u64)
        .map(|i| 0x8000_0000_8000_0000 | (i * 0x0003_0001_0005_0007))
        .collect();
    upload(&mut gs, 256, psm::CT32, 16, 16, &clut);
    let indices: Vec<u64> = (0..32u64).map(|i| i.wrapping_mul(0x0F1E_2D3C_4B5A_6978)).collect();
    upload(&mut gs, 384, psm::T8, 16, 16, &indices);
    // As `paletted_texture`, loading the CLUT; then drawing with it, unloaded.
    let tex0 =
        384 | (1 << 14) | (u64::from(psm::T8) << 20) | (4 << 26) | (4 << 30) | (1 << 34) | (1 << 35) | (256 << 37);
    gs.write_reg(reg::TEX0_1, tex0 | (1 << 61));
    gpu.submit(&gs.take_commands());
    let (vram, clut) = pollster::block_on(gpu.read_state());
    assert!(vram == gs.mem.words[..], "the GS memory differs");
    assert_eq!(clut, gs.clut()[..]);
    // Another GPU GS, restored, draws as the software GS goes on.
    let Ok(mut restored) = GpuGs::new_blocking() else {
        return;
    };
    restored.restore(&vram, &clut);
    gs.write_reg(reg::TEX0_1, tex0);
    gs.write_reg(reg::PRIM, 6 | (1 << 4) | (1 << 8));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(4, 4));
    gs.write_reg(reg::UV, uv(16, 16));
    gs.write_reg(reg::XYZ2, xyz(36, 36));
    restored.submit(&gs.take_commands());
    assert!(
        restored.read_vram() == gs.mem.words[..],
        "the restored GPU draws otherwise"
    );
}

/// A filtered (TEX1: MMAG and MMIN linear) 8×8 CT32 texture at page 4.
fn filtered_texture(gs: &mut Gs) {
    let texels: Vec<u64> = (0..32u64)
        .map(|i| (0x8000_0000 | (i * 0x0507_0B0D)) | ((0x8000_0000 | (i * 0x0D0B_0705 + 0x40_2010)) << 32))
        .collect();
    upload(gs, 128, psm::CT32, 8, 8, &texels);
    gs.write_reg(
        reg::TEX0_1,
        128 | (1 << 14) | (3 << 26) | (3 << 30) | (1 << 34) | (1 << 35),
    );
    gs.write_reg(reg::TEX1_1, (1 << 5) | (1 << 6));
}

#[test]
fn bilinear_sprites() {
    let mut gs = gs();
    filtered_texture(&mut gs);
    // Magnified (UV), then shrunk.
    gs.write_reg(reg::PRIM, 6 | (1 << 4) | (1 << 8));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(0, 0));
    gs.write_reg(reg::UV, uv(8, 8));
    gs.write_reg(reg::XYZ2, xyz(40, 30));
    gs.write_reg(reg::UV, uv(0, 0));
    gs.write_reg(reg::XYZ2, xyz(44, 34));
    gs.write_reg(reg::UV, uv(8, 8));
    gs.write_reg(reg::XYZ2, xyz(48, 38));
    check(gs);
}

#[test]
fn bilinear_triangle() {
    let mut gs = gs();
    filtered_texture(&mut gs);
    // STQ, its Q varying: both filters by level of detail.
    gs.write_reg(reg::PRIM, 3 | (1 << 4));
    for (x, y, s, t, q) in [
        (2u32, 40u32, 0.0f32, 0.0f32, 1.0f32),
        (60, 42, 2.0, 0.0, 2.0),
        (30, 63, 0.5, 4.0, 4.0),
    ] {
        gs.write_reg(reg::ST, u64::from(s.to_bits()) | (u64::from(t.to_bits()) << 32));
        gs.write_reg(reg::RGBAQ, 0x8080_8080 | (u64::from(q.to_bits()) << 32));
        gs.write_reg(reg::XYZ2, xyz(x, y));
    }
    check(gs);
}

/// A 16×16 CT32 texture at page 4 with mipmap levels after it (TEX1's
/// MTBA lays out levels 1 to 3), and the given MMIN; MXL 3, MMAG linear.
fn mipmapped_texture(gs: &mut Gs, mmin: u64) {
    for (bp, w) in [(128u64, 16u64), (132, 8), (133, 4)] {
        let texels: Vec<u64> = (0..w * w / 2)
            .map(|i| (0x8000_0000 | (i * 0x0507_0B0D + bp)) | ((0x8000_0000 | (i * 0x0D0B_0705 + 0x40_2010)) << 32))
            .collect();
        upload(gs, bp, psm::CT32, w, w, &texels);
    }
    gs.write_reg(reg::TEX1_1, (3 << 2) | (1 << 5) | (mmin << 6) | (1 << 9));
    gs.write_reg(
        reg::TEX0_1,
        128 | (1 << 14) | (4 << 26) | (4 << 30) | (1 << 34) | (1 << 35),
    );
}

#[test]
fn mipmapped_triangles() {
    for mmin in 2..=5 {
        let mut gs = gs();
        mipmapped_texture(&mut gs, mmin);
        // STQ, its Q falling from 1 to 1/8 and less: every level.
        gs.write_reg(reg::PRIM, 3 | (1 << 4));
        for (x, y, s, t, q) in [
            (0u32, 0u32, 0.0f32, 0.0f32, 1.0f32),
            (63, 4, 0.2, 0.0, 0.125),
            (10, 63, 0.0, 0.05, 0.05),
        ] {
            gs.write_reg(reg::ST, u64::from(s.to_bits()) | (u64::from(t.to_bits()) << 32));
            gs.write_reg(reg::RGBAQ, 0x8080_8080 | (u64::from(q.to_bits()) << 32));
            gs.write_reg(reg::XYZ2, xyz(x, y));
        }
        check(gs);
    }
}
