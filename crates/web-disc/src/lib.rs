//! Disc images in the browser, for the emulators' WebAssembly modules:
//! they are not loaded in memory, but read on demand through a synchronous
//! JavaScript callback (synchronous XHR with a Range header, which workers
//! allow), and cached here in chunks.

use std::collections::{HashMap, VecDeque};

use texel_disc::{Disc, FileReader, Layout, ReadError, parse_cue};
use wasm_bindgen::prelude::*;

/// A disc read through the files' callback.
pub type JsDisc = Disc<Box<dyn FileReader>>;

/// The disc made of files `names` (of `sizes` bytes), laid out by `cue` (or
/// a single image without one), read through `read`:
/// `(file: number, offset: number, length: number) => Uint8Array`, with
/// `file` an index in `names`.
pub fn open(cue: Option<String>, names: &[String], sizes: &[f64], read: js_sys::Function) -> Result<JsDisc, JsError> {
    let sizes_by_name: HashMap<&str, u64> = names
        .iter()
        .map(String::as_str)
        .zip(sizes.iter().map(|&s| s as u64))
        .collect();
    let layout = match cue {
        Some(text) => parse_cue(&text, |name| sizes_by_name.get(name).copied())?,
        None => {
            let (name, size) = names
                .first()
                .zip(sizes.first())
                .ok_or_else(|| JsError::new("no disc file"))?;
            Layout::single_file(name, *size as u64).ok_or_else(|| JsError::new("unrecognised disc image size"))?
        }
    };
    let file_sizes = layout
        .files
        .iter()
        .map(|f| sizes_by_name.get(f.as_str()).copied().unwrap_or(0))
        .collect();
    // Map the layout's file indices to the caller's.
    let index: Vec<u32> = layout
        .files
        .iter()
        .map(|f| names.iter().position(|n| n == f).unwrap_or(0) as u32)
        .collect();
    let reader = JsReader {
        read: remap_reader(read, index),
        sizes: file_sizes,
        cache: HashMap::new(),
        order: VecDeque::new(),
    };
    Ok(Disc::new(layout, Box::new(reader) as Box<dyn FileReader>))
}

/// Bytes fetched per read request: 112 raw sectors.
const CHUNK: u64 = 2352 * 112;
/// Chunks kept in the cache (~32 MiB).
const CACHE_CHUNKS: usize = 128;

struct JsReader {
    /// `(file: number, offset: number, length: number) => Uint8Array`
    read: js_sys::Function,
    sizes: Vec<u64>,
    cache: HashMap<(usize, u64), Vec<u8>>,
    order: VecDeque<(usize, u64)>,
}

impl JsReader {
    fn chunk(&mut self, file: usize, index: u64) -> Result<&[u8], ReadError> {
        let key = (file, index);
        if !self.cache.contains_key(&key) {
            let size = *self
                .sizes
                .get(file)
                .ok_or_else(|| ReadError::Io(format!("no file {file}")))?;
            let offset = index * CHUNK;
            let len = CHUNK.min(size.saturating_sub(offset));
            let value = self
                .read
                .call3(
                    &JsValue::NULL,
                    &JsValue::from(file as u32),
                    &JsValue::from(offset as f64),
                    &JsValue::from(len as f64),
                )
                .map_err(|e| ReadError::Io(format!("{e:?}")))?;
            let bytes = js_sys::Uint8Array::new(&value).to_vec();
            if bytes.len() as u64 != len {
                return Err(ReadError::Io(format!("short read: {} of {len} bytes", bytes.len())));
            }
            if self.order.len() >= CACHE_CHUNKS
                && let Some(old) = self.order.pop_front()
            {
                self.cache.remove(&old);
            }
            self.order.push_back(key);
            self.cache.insert(key, bytes);
        }
        Ok(&self.cache[&key])
    }
}

impl FileReader for JsReader {
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done as u64;
            let chunk = self.chunk(file, pos / CHUNK)?;
            let start = (pos % CHUNK) as usize;
            let n = (buf.len() - done).min(chunk.len().saturating_sub(start));
            if n == 0 {
                return Err(ReadError::Io("read past the end of the file".into()));
            }
            buf[done..done + n].copy_from_slice(&chunk[start..start + n]);
            done += n;
        }
        Ok(())
    }
}

/// Wraps `read` so that the layout's file indices map to the caller's.
fn remap_reader(read: js_sys::Function, index: Vec<u32>) -> js_sys::Function {
    let closure =
        Closure::<dyn Fn(u32, f64, f64) -> Result<JsValue, JsValue>>::new(move |file: u32, offset: f64, len: f64| {
            let real = index.get(file as usize).copied().unwrap_or(file);
            read.call3(
                &JsValue::NULL,
                &JsValue::from(real),
                &JsValue::from(offset),
                &JsValue::from(len),
            )
        });
    closure.into_js_value().unchecked_into()
}
