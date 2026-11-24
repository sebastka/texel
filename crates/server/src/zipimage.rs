//! Disc images inside zip files, read without unpacking them.
//!
//! A stored (uncompressed) entry is read where it lies. A deflated one is
//! a single stream that can only be decompressed from its start, so the
//! server indexes it once (zlib's zran method): every `SPAN` bytes of
//! output, at the end of a deflate block, it records where the compressed
//! data is (to the bit) and the 32 KiB of output before it, which later
//! blocks may refer back to. A read starts at the closest point before it:
//! a fresh decompressor primed with the bits left in that byte and given
//! the 32 KiB as its dictionary. The indexes are kept in the server's cache
//! directory, named after the zip and the entry, and built when the library
//! scan finds an entry without one.
//!
//! Resuming in the middle of a stream takes zlib's `inflatePrime` and
//! `Z_BLOCK`, which only its C interface has: `libz-rs-sys` (the same Rust
//! implementation as `flate2`'s, with zlib's API), hence the unsafe calls,
//! each on a stream this module owns.

#![allow(unsafe_code)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use libz_rs_sys as z;

/// Output between two resume points.
const SPAN: u64 = 4 << 20;
/// What deflate may refer back to.
const WINDOW: usize = 32 * 1024;
/// Compressed input read at a time.
const CHUNK: usize = 64 * 1024;
const MAGIC: &[u8; 8] = b"TXZIDX01";

/// A disc image inside a zip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipEntry {
    /// Where its data starts in the zip, and its compressed length.
    pub data_start: u64,
    pub compressed: u64,
    /// Its length, decompressed.
    pub size: u64,
    /// Deflated: its seek index (in the server's cache). Stored: none.
    pub index: Option<PathBuf>,
}

/// What a zip holds: its disc images (name, entry), and its CUE sheets
/// (name, text). Names are without the directories inside the zip.
pub struct Listing {
    pub images: Vec<(String, ZipEntry)>,
    pub cues: Vec<(String, String)>,
}

/// Lists a zip's images and CUE sheets, building the seek index of each
/// deflated image in `index_dir` if it has none yet.
pub fn list(path: &Path, index_dir: &Path) -> anyhow::Result<Listing> {
    let meta = path.metadata()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut archive = zip::ZipArchive::new(File::open(path)?).context("reading the zip")?;
    let mut listing = Listing {
        images: Vec::new(),
        cues: Vec::new(),
    };
    for i in 0..archive.len() {
        let (name, method, data_start, compressed, size, crc) = {
            let f = archive.by_index_raw(i)?;
            if f.is_dir() {
                continue;
            }
            let full = f.name()?.into_owned();
            let name = full.rsplit('/').next().unwrap_or(&full).to_owned();
            let start = f.data_start().context("an entry without its data's offset")?;
            (name, f.compression(), start, f.compressed_size(), f.size(), f.crc32())
        };
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "cue" => {
                ensure!(size < 1 << 20, "{name}: a CUE sheet of {size} bytes");
                let mut text = String::new();
                archive.by_index(i)?.read_to_string(&mut text)?;
                listing.cues.push((name, text));
            }
            "iso" | "bin" | "img" => {
                let index = match method {
                    zip::CompressionMethod::Stored => None,
                    zip::CompressionMethod::Deflated => {
                        let key = format!("{}\n{}\n{}\n{mtime}\n{name}\n{crc:08x}", path.display(), meta.len(), i);
                        let index = index_dir.join(format!("{}.idx", crate::state::cache_key(&key)));
                        if !index.is_file() {
                            tracing::info!(zip = %path.display(), entry = %name, "building the zip's seek index (once)");
                            let started = std::time::Instant::now();
                            let mut file = File::open(path)?;
                            build_index(&mut file, data_start, compressed, size, crc, &index)
                                .with_context(|| format!("indexing {name}"))?;
                            tracing::info!(entry = %name, seconds = started.elapsed().as_secs(), "seek index built");
                        }
                        Some(index)
                    }
                    other => {
                        tracing::warn!(zip = %path.display(), entry = %name, method = ?other, "unsupported compression, ignored");
                        continue;
                    }
                };
                let entry = ZipEntry {
                    data_start,
                    compressed,
                    size,
                    index,
                };
                listing.images.push((name, entry));
            }
            _ => {}
        }
    }
    Ok(listing)
}

// --- zlib ---------------------------------------------------------------------

/// A raw deflate decompressor (no zlib header), on its own heap allocation:
/// the stream's state points back to it.
struct Inflater {
    strm: Box<z::z_stream>,
}

impl Inflater {
    fn new() -> anyhow::Result<Self> {
        let mut strm = Box::new(z::z_stream::default());
        let size = i32::try_from(size_of::<z::z_stream>())?;
        // SAFETY: `strm` is a default stream (allocator set by `default`),
        // the version string is the library's own.
        let r = unsafe { z::inflateInit2_(&mut *strm, -15, z::zlibVersion(), size) };
        ensure!(r == z::Z_OK, "inflateInit2 failed ({r})");
        Ok(Self { strm })
    }

    /// Feeds the decompressor the `bits` low bits of `value` first.
    fn prime(&mut self, bits: i32, value: i32) -> anyhow::Result<()> {
        // SAFETY: an initialized stream.
        let r = unsafe { z::inflatePrime(&mut *self.strm, bits, value) };
        ensure!(r == z::Z_OK, "inflatePrime failed ({r})");
        Ok(())
    }

    fn set_dictionary(&mut self, dictionary: &[u8]) -> anyhow::Result<()> {
        let len = u32::try_from(dictionary.len())?;
        // SAFETY: an initialized stream; the dictionary is read only during
        // the call.
        let r = unsafe { z::inflateSetDictionary(&mut *self.strm, dictionary.as_ptr(), len) };
        ensure!(r == z::Z_OK, "inflateSetDictionary failed ({r})");
        Ok(())
    }

    /// Decompresses from `input` into `output`: the bytes consumed and
    /// produced, and zlib's return code.
    fn inflate(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> (usize, usize, i32) {
        let (in_len, out_len) = (input.len().min(u32::MAX as usize), output.len().min(u32::MAX as usize));
        self.strm.next_in = input.as_ptr();
        self.strm.avail_in = in_len as u32;
        self.strm.next_out = output.as_mut_ptr();
        self.strm.avail_out = out_len as u32;
        // SAFETY: an initialized stream, its input and output the slices
        // above, which outlive the call.
        let r = unsafe { z::inflate(&mut *self.strm, flush) };
        let used = in_len - self.strm.avail_in as usize;
        let made = out_len - self.strm.avail_out as usize;
        self.strm.next_in = std::ptr::null();
        self.strm.avail_in = 0;
        self.strm.next_out = std::ptr::null_mut();
        self.strm.avail_out = 0;
        (used, made, r)
    }

    /// After `Z_BLOCK`: 128 at the end of a block (64 more if it is the
    /// last), and the bits of the last byte not used yet.
    fn data_type(&self) -> i32 {
        self.strm.data_type
    }
}

// SAFETY: the stream and the state it points to belong to this value
// alone (nothing else points into them, the input and output pointers are
// cleared after each call), and zlib lets a stream be used from any thread,
// one at a time: `&mut self` (and the image cache's lock) sees to that.
unsafe impl Send for Inflater {}

impl Drop for Inflater {
    fn drop(&mut self) {
        // SAFETY: an initialized stream, ended once.
        unsafe { z::inflateEnd(&mut *self.strm) };
    }
}

fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut crc = u64::from(crc);
    for part in data.chunks(1 << 30) {
        // SAFETY: reads `part`, nothing else.
        crc = unsafe { z::crc32(crc as _, part.as_ptr(), part.len() as u32) } as u64;
    }
    crc as u32
}

// --- The index ----------------------------------------------------------------

/// A resume point: output offset, compressed offset (from the entry's
/// data), the bits of the byte before it still to read, and where its
/// window is in the index file.
#[derive(Clone, Copy, Debug)]
struct Point {
    out: u64,
    input: u64,
    bits: u8,
    window_at: u64,
    window_len: u32,
    window_compressed: u32,
}

/// Decompresses the entry once, recording resume points, and writes them
/// to `path` (through a temporary file). Checks the size and CRC.
fn build_index(file: &mut File, start: u64, compressed: u64, size: u64, crc: u32, path: &Path) -> anyhow::Result<()> {
    file.seek(SeekFrom::Start(start))?;
    let mut inf = Inflater::new()?;
    let mut input = vec![0u8; CHUNK];
    let (mut in_pos, mut in_len, mut left) = (0usize, 0usize, compressed);
    // The output goes round a 32 KiB window, as zran's does.
    let mut window = vec![0u8; WINDOW];
    let mut at = 0usize;
    let (mut total_in, mut total_out, mut last) = (0u64, 0u64, 0u64);
    let mut check = 0u32;
    // The start: a fresh decompressor (a raw stream's first block gives
    // `Z_BLOCK` no stop before it).
    let mut points: Vec<(u64, u64, u8, Vec<u8>)> = vec![(0, 0, 0, Vec::new())];
    loop {
        if in_pos == in_len && left > 0 {
            let n = (CHUNK as u64).min(left) as usize;
            file.read_exact(&mut input[..n])?;
            (in_pos, in_len) = (0, n);
            left -= n as u64;
        }
        if at == WINDOW {
            at = 0;
        }
        let (used, made, r) = inf.inflate(&input[in_pos..in_len], &mut window[at..], z::Z_BLOCK);
        check = crc32(check, &window[at..at + made]);
        in_pos += used;
        at += made;
        total_in += used as u64;
        total_out += made as u64;
        match r {
            z::Z_STREAM_END => break,
            z::Z_OK => {}
            z::Z_BUF_ERROR if used == 0 && made == 0 && in_pos == in_len && left == 0 => {
                bail!("the compressed data ends early")
            }
            z::Z_BUF_ERROR => {}
            other => bail!("corrupt deflate data ({other})"),
        }
        let kind = inf.data_type();
        if kind & 128 != 0 && kind & 64 == 0 && total_out - last >= SPAN {
            // The 32 KiB before this point, oldest first.
            let before = if total_out >= WINDOW as u64 {
                [&window[at..], &window[..at]].concat()
            } else {
                window[..at].to_vec()
            };
            points.push((total_out, total_in, (kind & 7) as u8, before));
            last = total_out;
        }
    }
    ensure!(total_out == size, "{total_out} bytes decompressed, the zip says {size}");
    ensure!(check == crc, "CRC {check:08x}, the zip says {crc:08x}");

    // MAGIC, size, the points' count, the points (40 bytes each), then
    // their windows, compressed.
    let mut head = Vec::with_capacity(20 + points.len() * 33);
    head.extend_from_slice(MAGIC);
    head.extend_from_slice(&size.to_le_bytes());
    head.extend_from_slice(&u32::try_from(points.len())?.to_le_bytes());
    let mut windows = Vec::new();
    let mut table = Vec::new();
    for (out, input, bits, before) in &points {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(before)?;
        let packed = e.finish()?;
        table.push((
            *out,
            *input,
            *bits,
            windows.len() as u64,
            before.len() as u32,
            packed.len() as u32,
        ));
        windows.extend_from_slice(&packed);
    }
    let windows_at = (head.len() + table.len() * POINT_BYTES) as u64;
    for (out, input, bits, w_at, w_len, w_packed) in table {
        head.extend_from_slice(&out.to_le_bytes());
        head.extend_from_slice(&input.to_le_bytes());
        head.push(bits);
        head.extend_from_slice(&(windows_at + w_at).to_le_bytes());
        head.extend_from_slice(&w_len.to_le_bytes());
        head.extend_from_slice(&w_packed.to_le_bytes());
    }
    head.extend_from_slice(&windows);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &head)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Bytes per point in the index: out, input, bits, window offset, length,
/// compressed length.
const POINT_BYTES: usize = 8 + 8 + 1 + 8 + 4 + 4;

fn load_index(path: &Path, size: u64) -> anyhow::Result<Vec<Point>> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 20];
    f.read_exact(&mut head)?;
    ensure!(&head[..8] == MAGIC, "not a seek index");
    ensure!(
        u64::from_le_bytes(head[8..16].try_into()?) == size,
        "a seek index for another size"
    );
    let count = u32::from_le_bytes(head[16..20].try_into()?) as usize;
    let mut table = vec![0u8; count * POINT_BYTES];
    f.read_exact(&mut table)?;
    let points: Vec<Point> = table
        .as_chunks::<POINT_BYTES>()
        .0
        .iter()
        .map(|p| Point {
            out: u64::from_le_bytes(p[0..8].try_into().unwrap_or_default()),
            input: u64::from_le_bytes(p[8..16].try_into().unwrap_or_default()),
            bits: p[16],
            window_at: u64::from_le_bytes(p[17..25].try_into().unwrap_or_default()),
            window_len: u32::from_le_bytes(p[25..29].try_into().unwrap_or_default()),
            window_compressed: u32::from_le_bytes(p[29..33].try_into().unwrap_or_default()),
        })
        .collect();
    ensure!(
        points.first().is_some_and(|p| p.out == 0),
        "a seek index without its start"
    );
    Ok(points)
}

// --- Reading ------------------------------------------------------------------

/// A decompressor going on through an entry: the next output offset, and
/// the input read ahead.
struct Cursor {
    inf: Inflater,
    out: u64,
    /// The next compressed byte to read, from the entry's data.
    next_in: u64,
    input: Vec<u8>,
    in_pos: usize,
}

/// A zip's file, and per deflated entry its points and decompressor.
pub struct ZipHandle {
    file: File,
    entries: std::collections::HashMap<u64, EntryState>,
}

struct EntryState {
    points: Vec<Point>,
    index: File,
    cursor: Option<Cursor>,
}

impl ZipHandle {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            entries: std::collections::HashMap::new(),
        })
    }

    pub fn read(&mut self, entry: &ZipEntry, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        ensure!(
            offset + buf.len() as u64 <= entry.size,
            "read past the end of a zipped image"
        );
        let Some(index) = &entry.index else {
            self.file.seek(SeekFrom::Start(entry.data_start + offset))?;
            self.file.read_exact(buf)?;
            return Ok(());
        };
        let state = match self.entries.entry(entry.data_start) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(EntryState {
                points: load_index(index, entry.size)?,
                index: File::open(index)?,
                cursor: None,
            }),
        };
        // The closest point before the read; the decompressor already
        // under way goes on if it is between that point and the read.
        let k = state.points.partition_point(|p| p.out <= offset) - 1;
        let point = state.points[k];
        let resume = match &state.cursor {
            Some(c) => c.out > offset || c.out < point.out,
            None => true,
        };
        if resume {
            state.cursor = Some(start_at(&mut self.file, &mut state.index, entry, &point)?);
        }
        let Some(cursor) = state.cursor.as_mut() else {
            bail!("no decompressor");
        };
        // Up to the read, discarded; then into `buf`.
        let mut skip = vec![0u8; (offset - cursor.out).min(1 << 20) as usize];
        while cursor.out < offset {
            let n = (offset - cursor.out).min(skip.len() as u64) as usize;
            inflate_into(&mut self.file, entry, cursor, &mut skip[..n])?;
        }
        inflate_into(&mut self.file, entry, cursor, buf)
    }
}

/// A decompressor at `point`: its byte's remaining bits primed, its window
/// as dictionary.
fn start_at(file: &mut File, index: &mut File, entry: &ZipEntry, point: &Point) -> anyhow::Result<Cursor> {
    let mut inf = Inflater::new()?;
    let mut next_in = point.input;
    if point.bits > 0 {
        next_in -= 1;
        let mut byte = [0u8];
        file.seek(SeekFrom::Start(entry.data_start + next_in))?;
        file.read_exact(&mut byte)?;
        next_in += 1;
        inf.prime(i32::from(point.bits), i32::from(byte[0] >> (8 - point.bits)))?;
    }
    if point.window_len > 0 {
        let mut packed = vec![0u8; point.window_compressed as usize];
        index.seek(SeekFrom::Start(point.window_at))?;
        index.read_exact(&mut packed)?;
        let mut window = Vec::with_capacity(point.window_len as usize);
        flate2::read::DeflateDecoder::new(&packed[..]).read_to_end(&mut window)?;
        ensure!(window.len() == point.window_len as usize, "a damaged seek index");
        inf.set_dictionary(&window)?;
    }
    Ok(Cursor {
        inf,
        out: point.out,
        next_in,
        input: Vec::new(),
        in_pos: 0,
    })
}

/// Fills `out` from the cursor's decompressor.
fn inflate_into(file: &mut File, entry: &ZipEntry, c: &mut Cursor, out: &mut [u8]) -> anyhow::Result<()> {
    let mut done = 0;
    while done < out.len() {
        if c.in_pos == c.input.len() {
            let left = entry.compressed.saturating_sub(c.next_in);
            ensure!(left > 0, "the compressed data ends early");
            let n = (CHUNK as u64).min(left) as usize;
            c.input.resize(n, 0);
            file.seek(SeekFrom::Start(entry.data_start + c.next_in))?;
            file.read_exact(&mut c.input)?;
            c.next_in += n as u64;
            c.in_pos = 0;
        }
        let (used, made, r) = c.inf.inflate(&c.input[c.in_pos..], &mut out[done..], z::Z_NO_FLUSH);
        c.in_pos += used;
        done += made;
        c.out += made as u64;
        match r {
            z::Z_OK | z::Z_BUF_ERROR => {}
            z::Z_STREAM_END if done == out.len() => {}
            z::Z_STREAM_END => bail!("the stream ends before the read"),
            other => bail!("corrupt deflate data ({other})"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pseudo-random but compressible data: runs and noise.
    fn data(len: usize) -> Vec<u8> {
        let mut x = 0x1234_5678u32;
        (0..len)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                if (i / 4096) % 3 == 0 {
                    (i / 4096) as u8
                } else {
                    (x % 7) as u8
                }
            })
            .collect()
    }

    fn write_zip(path: &Path, files: &[(&str, &[u8], bool)]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, content, deflate) in files {
            let method = if *deflate {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            };
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(method)
                .large_file(true);
            zip.start_file(*name, options).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn zipped_images_read_anywhere() {
        let dir = std::env::temp_dir().join(format!("texel-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.zip");
        let image = data(19 << 20);
        let small = data(100_000);
        write_zip(
            &path,
            &[
                ("Game/game.cue", b"FILE \"game.bin\" BINARY\n", true),
                ("Game/game.bin", &image, true),
                ("stored.iso", &small, false),
            ],
        );
        let listing = list(&path, &dir.join("index")).unwrap();
        assert_eq!(listing.cues.len(), 1);
        assert_eq!(listing.cues[0].0, "game.cue");
        let names: Vec<&str> = listing.images.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["game.bin", "stored.iso"]);
        let (deflated, stored) = (&listing.images[0].1, &listing.images[1].1);
        assert!(deflated.index.as_ref().is_some_and(|p| p.is_file()));
        assert!(stored.index.is_none());

        let mut h = ZipHandle::open(&path).unwrap();
        // Anywhere, in any order: forwards, backwards, across points, at
        // the end.
        for &(offset, len) in &[
            (0u64, 4096usize),
            (5 << 20, 70_000),
            (1_000_001, 3),
            (13 << 20, 2 << 20),
            (13 << 20, 100),
            ((19 << 20) - 10, 10),
            (4 << 20, 1),
        ] {
            let mut buf = vec![0u8; len];
            h.read(deflated, offset, &mut buf).unwrap();
            assert!(
                buf[..] == image[offset as usize..offset as usize + len],
                "deflated at {offset}"
            );
        }
        let mut buf = vec![0u8; 1000];
        h.read(stored, 5000, &mut buf).unwrap();
        assert!(buf[..] == small[5000..6000]);
        assert!(h.read(stored, 99_500, &mut buf).is_err());

        // Indexed once: listing again finds it.
        let again = list(&path, &dir.join("index")).unwrap();
        assert_eq!(again.images[0].1, *deflated);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// On a real zipped image, checked against the image itself:
/// `TEXEL_ZIP=game.zip TEXEL_ZIP_IMAGE=game.iso cargo test -p texel-server
/// --release -- --ignored real_zip --nocapture`.
#[cfg(test)]
mod real {
    use super::*;

    #[test]
    #[ignore = "needs a zipped image: TEXEL_ZIP and TEXEL_ZIP_IMAGE"]
    fn real_zip() {
        let (Ok(zip), Ok(image)) = (std::env::var("TEXEL_ZIP"), std::env::var("TEXEL_ZIP_IMAGE")) else {
            panic!("set TEXEL_ZIP and TEXEL_ZIP_IMAGE");
        };
        let dir = std::env::temp_dir().join(format!("texel-real-zip-{}", std::process::id()));
        let started = std::time::Instant::now();
        let listing = list(Path::new(&zip), &dir).unwrap();
        println!("listed and indexed in {:.1} s", started.elapsed().as_secs_f64());
        let (name, entry) = &listing.images[0];
        let index_size = entry
            .index
            .as_ref()
            .and_then(|p| p.metadata().ok())
            .map_or(0, |m| m.len());
        println!("{name}: {} MB, index {} KB", entry.size >> 20, index_size >> 10);
        let mut original = File::open(&image).unwrap();
        let mut h = ZipHandle::open(Path::new(&zip)).unwrap();
        // Random 64 KiB reads, then a sequential stretch.
        let mut x = 0x9E37_79B9u64;
        let mut reads = Vec::new();
        for _ in 0..200 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            reads.push((x % (entry.size - 65536)) & !2047);
        }
        let start = reads[0];
        reads.extend((0..400).map(|k| start + k * 65536).filter(|&o| o + 65536 <= entry.size));
        let (mut got, mut want) = (vec![0u8; 65536], vec![0u8; 65536]);
        let mut times = Vec::new();
        for (k, &offset) in reads.iter().enumerate() {
            let t = std::time::Instant::now();
            h.read(entry, offset, &mut got).unwrap();
            times.push((k < 200, t.elapsed().as_secs_f64() * 1000.0));
            original.seek(SeekFrom::Start(offset)).unwrap();
            original.read_exact(&mut want).unwrap();
            assert!(got == want, "differs at {offset}");
        }
        let avg = |random: bool| {
            let t: Vec<f64> = times.iter().filter(|(r, _)| *r == random).map(|(_, t)| *t).collect();
            (
                t.iter().sum::<f64>() / t.len() as f64,
                t.iter().copied().fold(0.0, f64::max),
            )
        };
        let (ra, rm) = avg(true);
        let (sa, sm) = avg(false);
        println!(
            "random 64 KiB reads: {ra:.2} ms on average, {rm:.1} at most; sequential: {sa:.3} ms, {sm:.1} at most"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
