//! Background SHA-1 hashing of game files, to verify them against Redump.
//! Hashes are kept (in the database) by path, size and modification time.

use std::collections::HashMap;
use std::io::Read;

use serde::{Deserialize, Serialize};
use sha1::Digest;

use crate::images::Cache;
use crate::library::{GameFile, read_file};
use crate::state::AppState;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HashEntry {
    pub size: u64,
    pub mtime: u64,
    pub sha1: String,
}

/// Relative path → hash.
#[derive(Default, Serialize, Deserialize)]
pub struct HashCache {
    pub files: HashMap<String, HashEntry>,
}

impl HashCache {
    pub fn get(&self, rel: &str, size: u64, mtime: u64) -> Option<&str> {
        self.files
            .get(rel)
            .filter(|e| e.size == size && e.mtime == mtime)
            .map(|e| e.sha1.as_str())
    }
}

fn sha1_file(f: &GameFile) -> anyhow::Result<String> {
    let mut hasher = sha1::Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    if f.virt.is_none() {
        let mut file = std::fs::File::open(&f.path)?;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    } else {
        // Tracks inside CHD/PBP containers: hash the decompressed data.
        let mut cache = Cache::default();
        let mut offset = 0;
        while offset < f.size {
            let n = (f.size - offset).min(buf.len() as u64) as usize;
            read_file(&mut cache, f, offset, &mut buf[..n])?;
            hasher.update(&buf[..n]);
            offset += n as u64;
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Hashes every file that has no up-to-date hash, one at a time.
pub async fn run(state: &AppState) {
    let todo: Vec<GameFile> = {
        let lib = state.library.read().await;
        let cache = state.hashes.read().await;
        lib.games
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| cache.get(&f.rel, f.size, f.mtime).is_none())
            .cloned()
            .collect()
    };
    if todo.is_empty() {
        return;
    }
    tracing::info!(files = todo.len(), "hashing game files for verification");
    for file in todo {
        let (rel, path, size, mtime) = (file.rel.clone(), file.path.clone(), file.size, file.mtime);
        match tokio::task::spawn_blocking(move || sha1_file(&file)).await {
            Ok(Ok(sha1)) => {
                let entry = HashEntry { size, mtime, sha1 };
                // Saved after each file: hashing a library takes a while.
                if let Err(e) = state.db.put_hash(&rel, &entry).await {
                    tracing::warn!(error = %e, "saving a hash failed");
                }
                state.hashes.write().await.files.insert(rel, entry);
            }
            Ok(Err(e)) => tracing::warn!(file = %path.display(), error = %e, "hashing failed"),
            Err(e) => tracing::warn!(error = %e, "hashing task failed"),
        }
    }
    tracing::info!("hashing done");
}
