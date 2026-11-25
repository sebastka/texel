//! Cheat and patch codes for PlayStation games, from DuckStation's database
//! (github.com/duckstation/chtdb): a file per game, named after its serial,
//! in `cheats/` and `patches/` (draw distance, 60 fps, widescreen...), or
//! after its serial and the hash of its build, where builds differ. The
//! codes are their authors' (the database grants no licence to pass them
//! on): Texel ships none, the server downloads the database into its cache
//! (with the DATs, see `refresh`) and serves a game's codes to its player.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use texel_ps1::cheats::file::{self, FileCode};

use crate::library::{Console, Game};
use crate::state::{AppState, write_atomic};

const URL: &str = "https://codeload.github.com/duckstation/chtdb/zip/refs/heads/master";

/// Texel's own patches (`patches/`, a file per game, under Texel's
/// licence), by the serials they are for (see `build.rs`): listed with the
/// database's.
const OWN: &[(&[&str], &str)] = include!(concat!(env!("OUT_DIR"), "/own_patches.rs"));

/// Texel's own patches for a serial.
fn own_patches(serial: &str) -> Vec<FileCode> {
    OWN.iter()
        .filter(|(serials, _)| serials.iter().any(|s| s.eq_ignore_ascii_case(serial)))
        .flat_map(|(_, text)| file::parse(text))
        .collect()
}

/// The database, as downloaded (the zip) and indexed.
#[derive(Default)]
pub struct CodeDb {
    zip: Vec<u8>,
    /// By serial: the files' entries in the zip (patches or cheats, and the
    /// build's hash when the file names one).
    index: HashMap<String, Vec<Entry>>,
}

#[derive(Clone)]
struct Entry {
    patches: bool,
    hash: Option<String>,
    name: String,
}

/// A game's codes, for its player.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GameCodes {
    /// Whether the database is here (downloaded with the DATs).
    pub available: bool,
    pub patches: Vec<FileCode>,
    pub cheats: Vec<FileCode>,
}

fn dir(state: &AppState) -> PathBuf {
    state.config.state.join("cache").join("codes")
}

impl CodeDb {
    /// The database in the cache, indexed (empty when there is none).
    pub fn load(state: &AppState) -> Self {
        let Ok(zip) = std::fs::read(dir(state).join("chtdb.zip")) else {
            return Self::default();
        };
        match index(&zip) {
            Ok(index) => Self { zip, index },
            Err(e) => {
                tracing::warn!(error = %e, "reading the cheat database failed");
                Self::default()
            }
        }
    }

    fn file(&self, name: &str) -> Option<String> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&self.zip)).ok()?;
        let mut f = archive.by_name(name).ok()?;
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        Some(text)
    }
}

/// The zip's code files, by serial.
fn index(zip: &[u8]) -> anyhow::Result<HashMap<String, Vec<Entry>>> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(zip))?;
    let mut index: HashMap<String, Vec<Entry>> = HashMap::new();
    for name in archive.file_names() {
        let Ok(name) = name else { continue };
        // chtdb-master/patches/SCES-02380.cht, .../SCUS-94488_6C4C043E1B0B34C6.cht
        let mut parts = name.rsplit('/');
        let (Some(file), Some(kind)) = (parts.next(), parts.next()) else {
            continue;
        };
        let patches = match kind {
            "patches" => true,
            "cheats" => false,
            _ => continue,
        };
        let Some(stem) = file.strip_suffix(".cht") else {
            continue;
        };
        // SERIAL_HASH: 16 hex digits after the serial (which has a dash).
        let (serial, hash) = match stem.rsplit_once('_') {
            Some((serial, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
                (serial, Some(hash.to_ascii_uppercase()))
            }
            _ => (stem, None),
        };
        index.entry(serial.to_ascii_uppercase()).or_default().push(Entry {
            patches,
            hash,
            name: name.into_owned(),
        });
    }
    Ok(index)
}

/// Keeps the database in the cache up to date: downloaded when it is
/// missing or its ETag changed (the copy we have will do when GitHub
/// cannot be reached). Returns whether it changed.
pub async fn refresh(state: &AppState) -> bool {
    let d = dir(state);
    let version_path = d.join("version");
    let have = tokio::fs::read_to_string(&version_path).await.ok();
    let zip_path = d.join("chtdb.zip");
    if have.is_some() && zip_path.is_file() {
        let now = state
            .http
            .head(URL)
            .send()
            .await
            .ok()
            .and_then(|r| r.error_for_status().ok())
            .and_then(|r| r.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned));
        if now.is_none() || now == have {
            return false;
        }
    }
    tracing::info!(url = URL, "downloading");
    let result = async {
        let resp = state.http.get(URL).send().await?.error_for_status()?;
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = resp.bytes().await?;
        // A zip we can read, before replacing the one we have.
        index(&bytes)?;
        write_atomic(&zip_path, &bytes).await?;
        write_atomic(&version_path, etag.as_bytes()).await?;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            let db = CodeDb::load(state);
            *state.codes.write().await = Arc::new(db);
            true
        }
        Err(e) => {
            tracing::warn!(url = URL, error = %e, "download failed");
            false
        }
    }
}

/// DuckStation's hash of a disc's build: XXH64 (seed 0x4242D00C) of the
/// boot executable's path, the executable, the primary volume descriptor
/// and track 1's length; as file names write it (16 hex digits).
fn build_hash(game: &Game) -> anyhow::Result<String> {
    let id = crate::library::executable_identity(game)?;
    let mut h = xxhash_rust::xxh64::Xxh64::new(0x4242_D00C);
    h.update(id.name.as_bytes());
    h.update(&id.executable);
    h.update(&id.descriptor);
    h.update(&id.track1_sectors.to_le_bytes());
    Ok(format!("{:016X}", h.digest()))
}

/// A game's codes: its serial's files, and of those for a build, the one
/// whose hash is the disc's.
pub async fn for_game(state: &AppState, game: &Game) -> GameCodes {
    let db = state.codes.read().await.clone();
    let Some(serial) = game.serial.as_deref().filter(|_| game.console == Console::Ps1) else {
        return GameCodes::default();
    };
    // Texel's own first, then the database's.
    let own = own_patches(serial);
    let mut out = GameCodes {
        available: !db.zip.is_empty() || !own.is_empty(),
        patches: own,
        ..GameCodes::default()
    };
    let Some(entries) = db.index.get(&serial.to_ascii_uppercase()) else {
        return out;
    };
    // Hashing reads the executable: only when some file names a build.
    let hash = if entries.iter().any(|e| e.hash.is_some()) {
        let g = game.clone();
        tokio::task::spawn_blocking(move || build_hash(&g))
            .await
            .ok()
            .and_then(|r| {
                r.map_err(|e| tracing::warn!(error = %e, "hashing a disc's build failed"))
                    .ok()
            })
    } else {
        None
    };
    for patches in [true, false] {
        let of_kind: Vec<&Entry> = entries.iter().filter(|e| e.patches == patches).collect();
        // The build's own file, else the serial's.
        let chosen = of_kind
            .iter()
            .find(|e| e.hash.is_some() && e.hash == hash)
            .or_else(|| of_kind.iter().find(|e| e.hash.is_none()));
        let Some(entry) = chosen else { continue };
        let Some(text) = db.file(&entry.name) else { continue };
        let codes = file::parse(&text);
        if patches {
            out.patches.extend(codes);
        } else {
            out.cheats = codes;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn our_patches_parse_and_run() {
        let codes = super::own_patches("SLES-02993");
        assert_eq!(codes.len(), 1);
        let code = &codes[0];
        assert_eq!(code.name, "Increased Draw Distance");
        assert_eq!(code.overrides.cpu_overclock, Some(300));
        texel_ps1::cheats::Code::parse(&code.name, &code.body, code.activation()).expect("a code the core runs");
        assert_eq!(super::own_patches("sles-12993").len(), 1);
        assert!(super::own_patches("SLUS-01161").is_empty());
    }

    #[test]
    fn file_names_give_serials_and_hashes() {
        let mut zip = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip));
            for name in [
                "chtdb-master/patches/SCES-02380.cht",
                "chtdb-master/patches/SCUS-94488_6C4C043E1B0B34C6.cht",
                "chtdb-master/cheats/SLUS-00001.cht",
                "chtdb-master/README.md",
            ] {
                w.start_file(name, zip::write::SimpleFileOptions::default())
                    .expect("a file");
            }
            w.finish().expect("a zip");
        }
        let index = super::index(&zip).expect("an index");
        assert!(index["SCES-02380"][0].patches);
        assert_eq!(index["SCUS-94488"][0].hash.as_deref(), Some("6C4C043E1B0B34C6"));
        assert!(!index["SLUS-00001"][0].patches);
        assert_eq!(index.len(), 3);
    }
}
