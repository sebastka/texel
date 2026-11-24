//! Shared server state: the library, the DATs, hashes and HTTP client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use sha2::Digest;
use tokio::sync::{Mutex, RwLock};

use crate::Config;
use crate::dat::{self, AllDats, Dats};
use crate::library::{Console, Game, Library};
use crate::metadata::Igdb;
use crate::verify::HashCache;

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub library: RwLock<Arc<Library>>,
    pub dats: RwLock<Arc<AllDats>>,
    pub hashes: RwLock<HashCache>,
    /// Saves, settings and hashes.
    pub db: crate::db::Db,
    pub igdb: Option<Igdb>,
    /// Open CHD/PBP containers, for serving their tracks.
    pub images: Arc<std::sync::Mutex<crate::images::Cache>>,
    pub ready: AtomicBool,
    /// Wakes the builder of zipped images' seek indexes (after a scan).
    zip_indexes: tokio::sync::Notify,
    /// Netplay rooms, in memory.
    pub rooms: crate::netplay::Rooms,
    /// Sign-in and sessions.
    pub auth: crate::auth::Auth,
    scan_lock: Mutex<()>,
}

/// What is known about a game, from the DATs and its hashes.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// The Redump name, e.g. "Rayman (Europe) (En,Fr,De)".
    pub name: Option<String>,
    /// The name without its tags, e.g. "Rayman".
    pub title: String,
    /// Tags from the Redump name: region, languages, disc, revision.
    pub tags: Vec<String>,
    /// `Some(true)`: every track matches Redump; `Some(false)`: hashed but
    /// unknown to Redump; `None`: not hashed yet.
    pub verified: Option<bool>,
    /// Disc number of a multi-disc game ("Disc 2" in the Redump name).
    pub disc: Option<u32>,
}

/// One disc of a multi-disc game.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscRef {
    pub id: String,
    pub number: u32,
}

/// The discs of a multi-disc game.
#[derive(Clone)]
pub struct DiscGroup {
    pub discs: Vec<DiscRef>,
}

/// "Final Fantasy VII (Europe) (Disc 2)" → ("Final Fantasy VII (Europe)", 2).
pub fn split_disc(name: &str) -> Option<(String, u32)> {
    let start = name.find(" (Disc ")?;
    let rest = &name[start + 7..];
    let end = rest.find(')')?;
    let number = rest[..end].trim().parse().ok()?;
    Some((format!("{}{}", &name[..start], &rest[end + 1..]), number))
}

pub async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, path).await
}

fn dat_dir(config: &Config) -> PathBuf {
    config.state.join("cache").join("dats")
}

impl AppState {
    pub fn new(config: Config, db: crate::db::Db, hashes: HashCache) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("texel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .context("building the HTTP client")?;
        // The sign-in provider answers quickly, or the sign-in fails.
        let auth_http = reqwest::Client::builder()
            .user_agent(concat!("texel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .context("building the HTTP client")?;
        let auth = crate::auth::Auth::new(
            crate::auth::AuthConfig {
                public_url: config.public_url.clone(),
                oidc: config.oidc(),
                session_days: config.session_days,
                invite_days: config.invite_days,
            },
            auth_http,
        );
        let igdb = match (&config.igdb_client_id, &config.igdb_client_secret) {
            (Some(id), Some(secret)) if config.fetch_metadata => Some(Igdb::new(id.clone(), secret.clone())),
            _ => None,
        };
        Ok(Self {
            hashes: RwLock::new(hashes),
            db,
            config,
            http,
            library: RwLock::new(Arc::new(Library::default())),
            dats: RwLock::new(Arc::new(AllDats::default())),
            igdb,
            images: Arc::default(),
            ready: AtomicBool::new(false),
            zip_indexes: tokio::sync::Notify::new(),
            rooms: crate::netplay::Rooms::default(),
            auth,
            scan_lock: Mutex::new(()),
        })
    }

    /// Where the open-source BIOSes are downloaded (`openbios`).
    pub fn open_bios_dir(&self) -> PathBuf {
        self.config.state.join("cache").join("openbios")
    }

    pub async fn rescan(&self) {
        let _guard = self.scan_lock.lock().await;
        let mut roots = self.config.library_roots();
        // The open-source BIOSes the server downloads (see `openbios`).
        for root in &mut roots {
            let sub = match root.console {
                Console::Ps1 => "ps1",
                Console::Ps2 => "ps2",
            };
            let dir = self.open_bios_dir().join(sub);
            // There before the first download: no "no BIOS directory".
            let _ = std::fs::create_dir_all(&dir);
            root.bios.push(dir);
        }
        let started = std::time::Instant::now();
        let index_dir = self.config.state.join("cache").join("zip-index");
        // Redump's CUE sheets, for BIN files without one.
        let dats = self.dats.read().await.clone();
        match tokio::task::spawn_blocking(move || crate::library::scan(&roots, &index_dir, &dats)).await {
            Ok(lib) => {
                let count = |c| {
                    (
                        lib.bios.iter().filter(|b| b.console == c).count(),
                        lib.games.iter().filter(|g| g.console == c).count(),
                    )
                };
                let (ps1, ps2) = (count(Console::Ps1), count(Console::Ps2));
                tracing::info!(
                    bios = ps1.0,
                    games = ps1.1,
                    ps2_bios = ps2.0,
                    ps2_games = ps2.1,
                    ms = started.elapsed().as_millis() as u64,
                    "library scanned"
                );
                *self.library.write().await = Arc::new(lib);
                self.ready.store(true, Ordering::Relaxed);
                self.zip_indexes.notify_one();
            }
            Err(e) => tracing::error!(error = %e, "library scan failed"),
        }
    }

    /// Builds the DAT indexes from the cached files.
    pub async fn load_cached_dats(&self) {
        let ps1 = self.load_console_dats(Console::Ps1).await;
        let ps2 = self.load_console_dats(Console::Ps2).await;
        *self.dats.write().await = Arc::new(AllDats { ps1, ps2 });
    }

    async fn load_console_dats(&self, console: Console) -> Dats {
        let dir = dat_dir(&self.config);
        let src = dat::sources(console);
        let read = |name: &str| {
            let path = dir.join(name);
            async move { tokio::fs::read_to_string(path).await.ok() }
        };
        let (games, serials, bios) = (
            read(src.games_file).await,
            read(src.serials_file).await,
            read(src.bios_file).await,
        );
        let sbi = match src.sbi {
            Some((_, file)) => tokio::fs::read(dir.join(file)).await.ok(),
            None => None,
        };
        let cues = tokio::fs::read(dir.join(src.cues_file)).await.ok();
        let dats = tokio::task::spawn_blocking(move || {
            let games = games.map(|x| dat::parse_xml(&x)).transpose().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "invalid cached Redump DAT");
                None
            });
            // XML; redump.org's were clrmamepro (a cache from before).
            let bios = bios.map(|s| dat::parse_xml(&s).unwrap_or_else(|_| dat::parse_clrmamepro(&s)));
            let mut dats = Dats::new(
                games.unwrap_or_default(),
                serials.map(|s| dat::parse_clrmamepro(&s)).unwrap_or_default(),
                bios.unwrap_or_default(),
            );
            if let Some(zip) = sbi {
                match dat::unzip_sbis(&zip) {
                    Ok(sbi) => dats.sbi = sbi,
                    Err(e) => tracing::warn!(error = %e, "invalid cached Redump SBI files"),
                }
            }
            if let Some(zip) = cues {
                match dat::unzip_cues(&zip) {
                    Ok(cues) => dats.cues = cues,
                    Err(e) => tracing::warn!(error = %e, "invalid cached Redump CUE sheets"),
                }
            }
            dats
        })
        .await
        .unwrap_or_default();
        tracing::info!(
            console = console.name(),
            games = dats.games.len(),
            bios = dats.bios.len(),
            sbi = dats.sbi.len(),
            cues = dats.cues.len(),
            "DATs loaded"
        );
        dats
    }

    /// Brings the cached Redump files (game and BIOS DATs, CUE sheets, SBI
    /// files) and libretro's DATs up to date, then reloads the indexes. A
    /// file is downloaded only when it is missing or its published version
    /// changed: Redump names each file after its version (the download's
    /// file name), libretro's are told by their ETag; a HEAD request asks.
    /// `force`: download them all. Returns whether any changed.
    pub async fn refresh_dats(&self, force: bool) -> bool {
        let dir = dat_dir(&self.config);
        let versions_path = dir.join("versions.json");
        let mut versions: HashMap<String, String> = tokio::fs::read(&versions_path)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let unzip = |b: &[u8]| Ok(dat::unzip_dat(b)?.into_bytes());
        let as_is = |b: &[u8]| Ok(b.to_vec());
        let mut changed = false;
        for console in [Console::Ps1, Console::Ps2] {
            let src = dat::sources(console);
            let mut files: Vec<(&str, &str, &Process)> = vec![
                (src.games_file, src.games_url, &unzip),
                (src.serials_file, src.serials_url, &as_is),
                (src.bios_file, src.bios_url, &as_is),
                (src.cues_file, src.cues_url, &as_is),
            ];
            if let Some((url, file)) = src.sbi {
                files.push((file, url, &as_is));
            }
            for (file, url, process) in files {
                let have = dir.join(file).is_file().then(|| versions.get(file)).flatten();
                if !force && let Some(have) = have {
                    match self.published_version(url).await {
                        Some(now) if &now == have => continue,
                        Some(_) => {}
                        // Unreachable: the copy we have will do.
                        None => continue,
                    }
                }
                if let Some(version) = self.download(file, url, process).await {
                    versions.insert(file.to_owned(), version);
                    changed = true;
                }
            }
        }
        if changed {
            if let Ok(json) = serde_json::to_vec_pretty(&versions)
                && let Err(e) = write_atomic(&versions_path, &json).await
            {
                tracing::warn!(error = %e, "saving the DATs' versions failed");
            }
            self.load_cached_dats().await;
        }
        changed
    }

    /// A download's version, from a HEAD request: the file name it is
    /// served as (Redump's carries its date), else its ETag.
    async fn published_version(&self, url: &str) -> Option<String> {
        let resp = self.http.head(url).send().await.ok()?.error_for_status().ok()?;
        version_of(resp.headers())
    }

    /// Downloads `url` into the DAT cache as `name`, through `process`:
    /// its version (see `published_version`), or None if it failed.
    async fn download(&self, name: &str, url: &str, process: &Process) -> Option<String> {
        tracing::info!(url, "downloading");
        let result = async {
            let resp = self.http.get(url).send().await?.error_for_status()?;
            let version = version_of(resp.headers()).unwrap_or_default();
            let bytes = resp.bytes().await?;
            let data = process(&bytes)?;
            write_atomic(&dat_dir(&self.config).join(name), &data).await?;
            anyhow::Ok(version)
        }
        .await;
        match result {
            Ok(version) => Some(version),
            Err(e) => {
                tracing::warn!(url, error = %e, "download failed");
                None
            }
        }
    }

    /// The SBI file of a LibCrypt-protected PS1 disc: its own (next to its
    /// image, or in its zip); else Redump's for its Redump name, once its
    /// hashes identify it. Before that (not hashed yet, or a dump Redump
    /// does not list), only its serial is known, which revisions share and
    /// they may need different files (Spyro - Year of the Dragon (Europe),
    /// Rev 0 and Rev 1): Redump's is then used only when every one of that
    /// serial's discs has the same.
    pub async fn sbi(&self, game: &Game) -> Option<Sbi> {
        if let Some(own) = &game.sbi {
            return Some(Sbi::Own(own.clone()));
        }
        if game.console != Console::Ps1 {
            return None;
        }
        let all = self.dats.read().await.clone();
        let redump = &all.get(game.console).sbi;
        if redump.is_empty() {
            return None;
        }
        let identity = self.identify(game).await;
        if identity.verified == Some(true) {
            return identity
                .name
                .and_then(|name| redump.get(&name).cloned())
                .map(Sbi::Redump);
        }
        let serial = game.serial.as_deref()?;
        let mut files = all
            .get(game.console)
            .serial_names(serial)
            .filter_map(|name| redump.get(name));
        let first = files.next()?;
        files.all(|f| f == first).then(|| Sbi::Redump(first.clone()))
    }

    pub async fn identify(&self, game: &Game) -> Identity {
        let all = self.dats.read().await.clone();
        let dats = all.get(game.console);
        let hashes = self.hashes.read().await;
        let sha1s: Option<Vec<String>> = game
            .files
            .iter()
            .map(|f| hashes.get(&f.rel, f.size, f.mtime).map(str::to_owned))
            .collect();
        let by_hash = sha1s.as_ref().and_then(|h| dats.by_hashes(h));
        let name = by_hash.map(|g| g.name.clone()).or_else(|| {
            let sizes: Vec<u64> = game.files.iter().map(|f| f.size).collect();
            dats.identify(game.serial.as_deref(), &sizes)
        });
        let verified = match (&sha1s, dats.games.is_empty()) {
            (Some(_), false) => Some(by_hash.is_some()),
            _ => None,
        };
        let (title, tags) = match &name {
            Some(n) => split_name(n),
            None => (game.fallback_title(), Vec::new()),
        };
        let disc = name.as_deref().and_then(split_disc).map(|(_, n)| n);
        Identity {
            name,
            title,
            tags,
            verified,
            disc,
        }
    }

    /// Multi-disc games: game id → its group.
    pub async fn disc_groups(&self) -> HashMap<String, DiscGroup> {
        let lib = self.library.read().await.clone();
        let mut groups: HashMap<String, Vec<DiscRef>> = HashMap::new();
        for g in &lib.games {
            if let Some((key, number)) = self.identify(g).await.name.as_deref().and_then(split_disc) {
                groups.entry(key).or_default().push(DiscRef {
                    id: g.id.clone(),
                    number,
                });
            }
        }
        let mut out = HashMap::new();
        for mut discs in groups.into_values() {
            discs.sort_by_key(|d| d.number);
            for d in &discs {
                out.insert(d.id.clone(), DiscGroup { discs: discs.clone() });
            }
        }
        out
    }

    /// Each image file of a game, checked against the matching Redump
    /// track (by position: CUE sheets list tracks in Redump's order).
    pub async fn file_checks(&self, game: &Game, redump_name: Option<&str>) -> Vec<FileCheck> {
        let all = self.dats.read().await.clone();
        let dats = all.get(game.console);
        let hashes = self.hashes.read().await;
        let bins: Vec<_> = redump_name
            .and_then(|n| dats.game(n))
            .map(|g| g.bins().collect())
            .unwrap_or_default();
        game.files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let sha1 = hashes.get(&f.rel, f.size, f.mtime).map(str::to_owned);
                let expected = bins.get(i);
                FileCheck {
                    name: f.name.clone(),
                    size: f.size,
                    matches: match (&sha1, expected) {
                        (Some(h), Some(e)) => Some(*h == e.sha1 && f.size == e.size),
                        (None, Some(e)) if f.size != e.size => Some(false),
                        (Some(_), None) if !dats.games.is_empty() => Some(false),
                        _ => None,
                    },
                    sha1,
                    redump_name: expected.map(|e| e.name.clone()),
                    redump_sha1: expected.map(|e| e.sha1.clone()),
                }
            })
            .collect()
    }

    /// The Redump description of a BIOS, and whether the DAT is loaded at all.
    /// The description of a known-good BIOS dump and who vouches for it,
    /// and whether the Redump BIOS DAT is loaded at all.
    pub async fn bios_model(&self, console: Console, sha1: &str) -> (Option<(String, &'static str)>, bool) {
        if let Some(&(_, desc)) = EXTRA_BIOS.iter().find(|(s, _)| *s == sha1) {
            return (Some((desc.to_owned(), "libretro")), true);
        }
        let all = self.dats.read().await;
        let dats = all.get(console);
        (
            dats.bios.get(sha1).map(|d| (d.clone(), "Redump")),
            !dats.bios.is_empty(),
        )
    }
}

/// Known-good BIOS images that Redump does not list, by SHA-1 (from
/// libretro's System.dat).
const EXTRA_BIOS: &[(&str, &str)] = &[(
    "96880d1ca92a016ff054be5159bb06fe03cb4e14",
    "PSP firmware 6.60 PS1 emulator (based on Version 4.5 05/25/00 J)",
)];

/// An image file checked against Redump.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCheck {
    pub name: String,
    pub size: u64,
    /// `None` until hashed.
    pub sha1: Option<String>,
    /// The Redump track at the same position.
    pub redump_name: Option<String>,
    pub redump_sha1: Option<String>,
    /// `None` when unknown (not hashed yet, or no Redump entry).
    pub matches: Option<bool>,
}

/// "Rayman (Europe) (En,Fr,De)" → ("Rayman", ["Europe", "En,Fr,De"]).
pub fn split_name(name: &str) -> (String, Vec<String>) {
    let title_end = name.find(" (").unwrap_or(name.len());
    let tags = name[title_end..]
        .split('(')
        .filter_map(|t| t.split(')').next())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    (name[..title_end].to_owned(), tags)
}

/// Builds the seek indexes of the zipped images the last scan found without
/// one, one at a time, after each scan (`AppState::rescan` wakes it). A
/// player reading an image before its turn builds it then (`zipimage`).
pub async fn index_zips(state: Arc<AppState>) {
    // The indexes that could not be built (a damaged or unfinished zip):
    // not tried again until the zip changes, which renames its index.
    let mut failed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    loop {
        state.zip_indexes.notified().await;
        let lib = state.library.read().await.clone();
        let mut todo: Vec<(PathBuf, crate::zipimage::ZipEntry)> = lib
            .games
            .iter()
            .flat_map(|g| &g.files)
            .filter_map(|f| match &f.virt {
                Some(crate::images::Virtual::Zip(e)) if e.index.as_ref().is_some_and(|i| !i.is_file()) => {
                    Some((f.path.clone(), e.clone()))
                }
                _ => None,
            })
            .collect();
        todo.sort_by(|a, b| a.0.cmp(&b.0));
        todo.dedup_by(|a, b| a.1.index == b.1.index);
        todo.retain(|(_, e)| e.index.as_ref().is_none_or(|i| !failed.contains(i)));
        if todo.is_empty() {
            continue;
        }
        tracing::info!(images = todo.len(), "building zipped images' seek indexes");
        for (zip, entry) in todo {
            let index = entry.index.clone();
            match tokio::task::spawn_blocking(move || crate::zipimage::ensure_index(&zip, &entry).map_err(|e| (zip, e)))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err((zip, e))) => {
                    tracing::warn!(zip = %zip.display(), error = format!("{e:#}"), "indexing a zipped image failed");
                    failed.extend(index);
                }
                Err(e) => tracing::warn!(error = %e, "indexing task failed"),
            }
        }
        tracing::info!("zipped images' seek indexes built");
    }
}

/// What a download goes through before it is cached (unzipped, or kept).
type Process = dyn Fn(&[u8]) -> anyhow::Result<Vec<u8>> + Sync;

/// Where a disc's SBI file comes from: its own, or Redump's set.
pub enum Sbi {
    Own(crate::library::SbiFile),
    Redump(Arc<Vec<u8>>),
}

/// A download's version: the file name it is served as, else its ETag.
fn version_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split("filename=").nth(1))
        .map(|f| f.trim_matches('"').to_owned());
    disposition.or_else(|| {
        headers
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    })
}

/// A stable file name for a cache key.
pub fn cache_key(s: &str) -> String {
    hex::encode(&sha2::Sha256::digest(s.as_bytes())[..16])
}

#[cfg(test)]
mod tests {
    #[test]
    fn split_disc() {
        assert_eq!(
            super::split_disc("Final Fantasy VII (Europe) (Disc 2)"),
            Some(("Final Fantasy VII (Europe)".to_owned(), 2))
        );
        assert_eq!(super::split_disc("Rayman (Europe)"), None);
    }

    #[test]
    fn split_name() {
        let (t, tags) = super::split_name("Final Fantasy VII (Europe) (Disc 1)");
        assert_eq!(t, "Final Fantasy VII");
        assert_eq!(tags, ["Europe", "Disc 1"]);
    }
}
