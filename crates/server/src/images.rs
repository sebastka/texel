//! Compressed disc images: CHD (MAME; CDs and DVDs), PBP (PS1 games
//! converted for the PSP), and images in zips (`zipimage`). The server
//! decompresses them and presents their tracks as plain BIN files (a DVD as
//! one ISO file) with a CUE sheet, so the browser handles every format the
//! same way.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};

/// A track (CHD), disc (PBP) or zipped image presented as a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Virtual {
    Chd(ChdTrack),
    Pbp(PbpDisc),
    Zip(crate::zipimage::ZipEntry),
}

impl Virtual {
    pub fn size(&self) -> u64 {
        match self {
            Self::Chd(t) => u64::from(t.frames) * u64::from(t.data_size),
            Self::Pbp(d) => u64::from(d.sectors) * 2352,
            Self::Zip(e) => e.size,
        }
    }
}

/// An image container, opened: its virtual files and its CUE sheet.
pub struct Opened {
    pub files: Vec<(String, Virtual)>,
    pub cue: String,
}

fn msf(frames: u32) -> String {
    format!("{:02}:{:02}:{:02}", frames / 4500, frames / 75 % 60, frames % 75)
}

// --- CHD ------------------------------------------------------------------

/// Bytes per CHD CD frame: a raw sector and its subcode.
const CHD_FRAME: u32 = 2352 + 96;
/// CHD pads every track to a multiple of this many frames.
const CHD_TRACK_PADDING: u32 = 4;
const CHT2: u32 = u32::from_be_bytes(*b"CHT2");
const CHTR: u32 = u32::from_be_bytes(*b"CHTR");
/// The metadata tag of DVD images (`chdman createdvd`).
const DVD: u32 = u32::from_be_bytes(*b"DVD ");
/// Bytes per CHD DVD unit: one 2048-byte sector, nothing else.
const DVD_SECTOR: u32 = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChdTrack {
    pub number: u8,
    /// First frame of the track in the CHD.
    pub start: u32,
    pub frames: u32,
    /// Bytes of data per frame: 2352 for raw and audio tracks.
    pub data_size: u32,
    /// Audio is stored big-endian in CHDs.
    pub audio: bool,
    /// Bytes per frame in the CHD: a raw sector and its subcode for CDs,
    /// a 2048-byte sector for DVDs.
    pub stride: u32,
}

/// One track from CHD metadata: "TRACK:2 TYPE:AUDIO SUBTYPE:NONE FRAMES:850
/// PREGAP:150 PGTYPE:VAUDIO PGSUB:NONE POSTGAP:0".
struct ChdMeta {
    number: u8,
    kind: String,
    frames: u32,
    pregap: u32,
    pregap_stored: bool,
}

fn parse_chd_meta(text: &str) -> Option<ChdMeta> {
    let field = |key: &str| {
        text.split_whitespace()
            .find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix(':')))
    };
    Some(ChdMeta {
        number: field("TRACK")?.parse().ok()?,
        kind: field("TYPE")?.to_owned(),
        frames: field("FRAMES")?.parse().ok()?,
        pregap: field("PREGAP").and_then(|v| v.parse().ok()).unwrap_or(0),
        pregap_stored: field("PGTYPE").is_some_and(|t| t.starts_with('V')),
    })
}

/// CUE mode and bytes per sector of a CHD track type.
fn chd_track_type(kind: &str) -> anyhow::Result<(&'static str, u32)> {
    Ok(match kind {
        "AUDIO" => ("AUDIO", 2352),
        "MODE1_RAW" => ("MODE1/2352", 2352),
        "MODE2_RAW" => ("MODE2/2352", 2352),
        "MODE1" => ("MODE1/2048", 2048),
        "MODE2" => ("MODE2/2336", 2336),
        other => bail!("unsupported CHD track type {other}"),
    })
}

pub fn open_chd(path: &Path) -> anyhow::Result<Opened> {
    let file = File::open(path)?;
    let mut chd = chd::Chd::open(file, None).context("not a CHD")?;
    let metadata = Vec::<chd::metadata::Metadata>::try_from(chd.metadata_refs())?;
    let mut files = Vec::new();
    let mut cue = String::new();
    let mut start = 0u32;
    for m in metadata.iter().filter(|m| m.metatag == CHT2 || m.metatag == CHTR) {
        let text = String::from_utf8_lossy(&m.value);
        let text = text.trim_end_matches('\0');
        let meta = parse_chd_meta(text).with_context(|| format!("bad track metadata {text:?}"))?;
        let (mode, data_size) = chd_track_type(&meta.kind)?;
        let name = format!("track{:02}.bin", meta.number);
        cue.push_str(&format!("FILE \"{name}\" BINARY\n  TRACK {:02} {mode}\n", meta.number));
        if meta.pregap > 0 && meta.pregap_stored {
            cue.push_str(&format!("    INDEX 00 00:00:00\n    INDEX 01 {}\n", msf(meta.pregap)));
        } else {
            if meta.pregap > 0 {
                cue.push_str(&format!("    PREGAP {}\n", msf(meta.pregap)));
            }
            cue.push_str("    INDEX 01 00:00:00\n");
        }
        files.push((
            name,
            Virtual::Chd(ChdTrack {
                number: meta.number,
                start,
                frames: meta.frames,
                data_size,
                audio: meta.kind == "AUDIO",
                stride: CHD_FRAME,
            }),
        ));
        start += meta.frames.div_ceil(CHD_TRACK_PADDING) * CHD_TRACK_PADDING;
    }
    if files.is_empty() && metadata.iter().any(|m| m.metatag == DVD) {
        return open_chd_dvd(chd.header());
    }
    ensure!(!files.is_empty(), "the CHD has neither CD track nor DVD metadata");
    Ok(Opened { files, cue })
}

/// A DVD image: a single run of 2048-byte sectors, presented as `disc.iso`.
fn open_chd_dvd(header: &chd::header::Header) -> anyhow::Result<Opened> {
    ensure!(
        header.unit_bytes() == DVD_SECTOR && header.hunk_size().is_multiple_of(DVD_SECTOR),
        "unexpected DVD CHD geometry: {} bytes per unit, {} per hunk",
        header.unit_bytes(),
        header.hunk_size()
    );
    let frames = u32::try_from(header.logical_bytes() / u64::from(DVD_SECTOR)).context("the DVD image is too large")?;
    let name = "disc.iso".to_owned();
    let cue = format!("FILE \"{name}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n");
    let track = ChdTrack {
        number: 1,
        start: 0,
        frames,
        data_size: DVD_SECTOR,
        audio: false,
        stride: DVD_SECTOR,
    };
    Ok(Opened {
        files: vec![(name, Virtual::Chd(track))],
        cue,
    })
}

struct ChdHandle {
    chd: chd::Chd<File>,
    compressed: Vec<u8>,
    hunk: Vec<u8>,
    hunk_index: Option<u32>,
}

impl ChdHandle {
    fn read(&mut self, track: &ChdTrack, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let frames_per_hunk = self.chd.header().hunk_size() / track.stride;
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let frame = u32::try_from(pos / u64::from(track.data_size))?;
            let within = (pos % u64::from(track.data_size)) as u32;
            ensure!(frame < track.frames, "read past the end of track {}", track.number);
            let index = track.start + frame;
            let hunk = index / frames_per_hunk;
            if self.hunk_index != Some(hunk) {
                self.hunk_index = None;
                self.chd
                    .hunk(hunk)?
                    .read_hunk_in(&mut self.compressed, &mut self.hunk)?;
                self.hunk_index = Some(hunk);
            }
            let frame_start = ((index % frames_per_hunk) * track.stride) as usize;
            let n = (track.data_size - within).min((buf.len() - done) as u32) as usize;
            let within = within as usize;
            if track.audio {
                // Samples are stored big-endian: swap the bytes of each pair.
                for (k, b) in buf[done..done + n].iter_mut().enumerate() {
                    *b = self.hunk[frame_start + ((within + k) ^ 1)];
                }
            } else {
                buf[done..done + n].copy_from_slice(&self.hunk[frame_start + within..frame_start + within + n]);
            }
            done += n;
        }
        Ok(())
    }
}

// --- PBP ------------------------------------------------------------------

/// Sectors per PBP block, and its size.
const PBP_BLOCK_SECTORS: u32 = 16;
const PBP_BLOCK: u32 = PBP_BLOCK_SECTORS * 2352;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PbpDisc {
    /// Offset of the disc's PSISOIMG header in the file.
    pub base: u64,
    pub sectors: u32,
}

fn read_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn bcd(v: u8) -> u32 {
    u32::from((v >> 4) * 10 + (v & 0xf))
}

/// The PSISOIMG headers of a PBP: one per disc.
fn pbp_discs(file: &mut File) -> anyhow::Result<Vec<u64>> {
    let mut header = [0u8; 40];
    file.read_exact(&mut header)?;
    ensure!(&header[..4] == b"\0PBP", "not a PBP file");
    let psar = u64::from(read_u32(&header, 36));
    let mut magic = [0u8; 16];
    file.seek(SeekFrom::Start(psar))?;
    file.read_exact(&mut magic)?;
    if &magic[..12] == b"PSISOIMG0000" {
        return Ok(vec![psar]);
    }
    ensure!(
        &magic == b"PSTITLEIMG000000",
        "unsupported PBP: encrypted (from the PlayStation Store), or not a PlayStation game"
    );
    let mut table = [0u8; 20];
    file.seek(SeekFrom::Start(psar + 0x200))?;
    file.read_exact(&mut table)?;
    Ok((0..5)
        .map(|i| read_u32(&table, i * 4))
        .filter(|&o| o != 0)
        .map(|o| psar + u64::from(o))
        .collect())
}

/// Reads a disc's TOC and returns its CUE sheet (one file, `name`) and size.
fn pbp_toc(file: &mut File, base: u64, name: &str) -> anyhow::Result<(String, u32)> {
    let mut toc = [0u8; 1020];
    file.seek(SeekFrom::Start(base + 0x800))?;
    file.read_exact(&mut toc)?;
    let mut tracks = Vec::new();
    let mut lead_out = 0;
    for e in toc.as_chunks::<10>().0 {
        let (ctrl, point) = (e[0], e[2]);
        let lba = ((bcd(e[7]) * 60 + bcd(e[8])) * 75 + bcd(e[9])).saturating_sub(150);
        match point {
            0xa2 => lead_out = lba,
            0x01..=0x99 => tracks.push((bcd(point), ctrl & 0x40 != 0, lba)),
            _ => {}
        }
    }
    ensure!(
        !tracks.is_empty() && lead_out > 0,
        "PBP disc without a table of contents"
    );
    tracks.sort_unstable();
    let mut cue = format!("FILE \"{name}\" BINARY\n");
    for (number, data, lba) in tracks {
        let mode = if data { "MODE2/2352" } else { "AUDIO" };
        cue.push_str(&format!("  TRACK {number:02} {mode}\n    INDEX 01 {}\n", msf(lba)));
    }
    Ok((cue, lead_out))
}

pub fn open_pbp(path: &Path) -> anyhow::Result<Vec<Opened>> {
    let mut file = File::open(path)?;
    let mut out = Vec::new();
    for (i, base) in pbp_discs(&mut file)?.into_iter().enumerate() {
        let name = format!("disc{}.bin", i + 1);
        let (cue, sectors) = pbp_toc(&mut file, base, &name)?;
        out.push(Opened {
            files: vec![(name, Virtual::Pbp(PbpDisc { base, sectors }))],
            cue,
        });
    }
    ensure!(!out.is_empty(), "the PBP has no disc");
    Ok(out)
}

struct PbpHandle {
    file: File,
    block: Vec<u8>,
    block_key: Option<(u64, u32)>,
}

impl PbpHandle {
    fn load_block(&mut self, disc: &PbpDisc, block: u32) -> anyhow::Result<()> {
        if self.block_key == Some((disc.base, block)) {
            return Ok(());
        }
        self.block_key = None;
        let mut entry = [0u8; 32];
        self.file
            .seek(SeekFrom::Start(disc.base + 0x4000 + u64::from(block) * 32))?;
        self.file.read_exact(&mut entry)?;
        let offset = u64::from(read_u32(&entry, 0));
        let length = u32::from(u16::from_le_bytes([entry[4], entry[5]]));
        let mut data = vec![0u8; length as usize];
        self.file.seek(SeekFrom::Start(disc.base + 0x10_0000 + offset))?;
        self.file.read_exact(&mut data)?;
        self.block.resize(PBP_BLOCK as usize, 0);
        if length == PBP_BLOCK {
            self.block.copy_from_slice(&data);
        } else {
            // Raw deflate, without zlib header.
            let mut inflate = flate2::Decompress::new(false);
            inflate.decompress(&data, &mut self.block, flate2::FlushDecompress::Finish)?;
        }
        self.block_key = Some((disc.base, block));
        Ok(())
    }

    fn read(&mut self, disc: &PbpDisc, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let block = u32::try_from(pos / u64::from(PBP_BLOCK))?;
            let within = (pos % u64::from(PBP_BLOCK)) as usize;
            ensure!(pos < u64::from(disc.sectors) * 2352, "read past the end of the disc");
            self.load_block(disc, block)?;
            let n = (PBP_BLOCK as usize - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&self.block[within..within + n]);
            done += n;
        }
        Ok(())
    }
}

// --- Reading ----------------------------------------------------------------

enum Handle {
    Chd(Box<ChdHandle>),
    Pbp(PbpHandle),
    Zip(crate::zipimage::ZipHandle),
}

/// Open containers, reused across reads (opening a CHD reads its whole map;
/// a zip's decompressor goes on from one read to the next).
#[derive(Default)]
pub struct Cache {
    open: HashMap<PathBuf, Handle>,
}

/// Containers kept open.
const MAX_OPEN: usize = 8;

impl Cache {
    pub fn read(&mut self, path: &Path, v: &Virtual, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        if !self.open.contains_key(path) {
            if self.open.len() >= MAX_OPEN {
                self.open.clear();
            }
            let handle = match v {
                Virtual::Chd(_) => {
                    let chd = chd::Chd::open(File::open(path)?, None)?;
                    Handle::Chd(Box::new(ChdHandle {
                        compressed: Vec::new(),
                        hunk: chd.get_hunksized_buffer(),
                        hunk_index: None,
                        chd,
                    }))
                }
                Virtual::Pbp(_) => Handle::Pbp(PbpHandle {
                    file: File::open(path)?,
                    block: Vec::new(),
                    block_key: None,
                }),
                Virtual::Zip(_) => Handle::Zip(crate::zipimage::ZipHandle::open(path)?),
            };
            self.open.insert(path.to_owned(), handle);
        }
        match (self.open.get_mut(path), v) {
            (Some(Handle::Chd(h)), Virtual::Chd(t)) => h.read(t, offset, buf),
            (Some(Handle::Pbp(h)), Virtual::Pbp(d)) => h.read(d, offset, buf),
            (Some(Handle::Zip(h)), Virtual::Zip(e)) => h.read(e, offset, buf),
            _ => bail!("{} changed format", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn chd_metadata() {
        let m =
            parse_chd_meta("TRACK:2 TYPE:AUDIO SUBTYPE:NONE FRAMES:850 PREGAP:150 PGTYPE:VAUDIO PGSUB:RW POSTGAP:0")
                .unwrap();
        assert_eq!(
            (m.number, m.kind.as_str(), m.frames, m.pregap, m.pregap_stored),
            (2, "AUDIO", 850, 150, true)
        );
        assert_eq!(msf(150), "00:02:00");
    }

    /// Writes a single-disc PBP of `sectors` raw sectors, one data track.
    fn write_pbp(path: &Path, sectors: u32, sector: impl Fn(u32) -> Vec<u8>) {
        let psar = 0x28u32;
        let mut f = File::create(path).unwrap();
        let mut header = vec![0u8; 0x28];
        header[..4].copy_from_slice(b"\0PBP");
        header[36..40].copy_from_slice(&psar.to_le_bytes());
        f.write_all(&header).unwrap();
        let mut img = vec![0u8; 0x10_0000];
        img[..12].copy_from_slice(b"PSISOIMG0000");
        let to_bcd = |v: u32| ((v / 10) << 4 | (v % 10)) as u8;
        let entry = |point: u8, lba: u32, ctrl: u8| {
            let a = lba + 150;
            [
                ctrl,
                0,
                point,
                0,
                0,
                0,
                0,
                to_bcd(a / 4500),
                to_bcd(a / 75 % 60),
                to_bcd(a % 75),
            ]
        };
        let toc = [
            entry(0xa0, 0, 0x41),
            entry(0xa1, 0, 0x41),
            entry(0xa2, sectors, 0x41),
            entry(1, 0, 0x41),
        ];
        for (i, e) in toc.iter().enumerate() {
            img[0x800 + i * 10..0x800 + i * 10 + 10].copy_from_slice(e);
        }
        let mut data = Vec::new();
        for b in 0..sectors.div_ceil(16) {
            let raw: Vec<u8> = (b * 16..(b + 1) * 16).flat_map(&sector).collect();
            let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&raw).unwrap();
            let comp = z.finish().unwrap();
            let e = 0x4000 + b as usize * 32;
            img[e..e + 4].copy_from_slice(&(data.len() as u32).to_le_bytes());
            img[e + 4..e + 6].copy_from_slice(&(comp.len() as u16).to_le_bytes());
            data.extend(comp);
        }
        f.write_all(&img).unwrap();
        f.write_all(&data).unwrap();
    }

    #[test]
    fn pbp_round_trip() {
        let dir = std::env::temp_dir().join(format!("texel-pbp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.pbp");
        let sector = |n: u32| (0..2352).map(|i| (n as usize * 7 + i) as u8).collect::<Vec<u8>>();
        write_pbp(&path, 40, sector);
        let discs = open_pbp(&path).unwrap();
        assert_eq!(discs.len(), 1);
        assert!(discs[0].cue.contains("TRACK 01 MODE2/2352"));
        let (_, v) = &discs[0].files[0];
        assert_eq!(v.size(), 40 * 2352);
        let mut cache = Cache::default();
        let mut buf = vec![0u8; 2352 * 3];
        cache.read(&path, v, 17 * 2352, &mut buf).unwrap();
        assert_eq!(&buf[..2352], &sector(17)[..]);
        assert_eq!(&buf[2352 * 2..], &sector(19)[..]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
