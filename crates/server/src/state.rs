//! Shared server state: the library, the DATs, hashes and HTTP client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

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
            rooms: crate::netplay::Rooms::default(),
            auth,
            scan_lock: Mutex::new(()),
        })
    }

    pub async fn rescan(&self) {
        let _guard = self.scan_lock.lock().await;
        let roots = self.config.library_roots();
        let started = std::time::Instant::now();
        match tokio::task::spawn_blocking(move || crate::library::scan(&roots)).await {
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
        let dats = tokio::task::spawn_blocking(move || {
            let games = games.map(|x| dat::parse_xml(&x)).transpose().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "invalid cached Redump DAT");
                None
            });
            Dats::new(
                games.unwrap_or_default(),
                serials.map(|s| dat::parse_clrmamepro(&s)).unwrap_or_default(),
                bios.map(|s| dat::parse_clrmamepro(&s)).unwrap_or_default(),
            )
        })
        .await
        .unwrap_or_default();
        tracing::info!(
            console = console.name(),
            games = dats.games.len(),
            bios = dats.bios.len(),
            "DATs loaded"
        );
        dats
    }

    /// Downloads DATs that are missing or stale, then reloads the indexes.
    pub async fn refresh_dats(&self, force: bool) {
        let dir = dat_dir(&self.config);
        let max_age = Duration::from_secs(self.config.dat_max_age_days * 86_400);
        let stale = |name: &str| {
            let meta = std::fs::metadata(dir.join(name)).ok();
            let age = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| SystemTime::now().duration_since(t).ok());
            force || age.is_none_or(|a| a > max_age)
        };
        let mut changed = false;
        for console in [Console::Ps1, Console::Ps2] {
            let src = dat::sources(console);
            if stale(src.games_file) {
                changed |= self
                    .download(src.games_file, src.games_url, |b| {
                        let text = dat::unzip_dat(b)?;
                        Ok(text.into_bytes())
                    })
                    .await;
            }
            if stale(src.serials_file) {
                changed |= self
                    .download(src.serials_file, src.serials_url, |b| Ok(b.to_vec()))
                    .await;
            }
            // Pinned by hash: downloaded once.
            if !dir.join(src.bios_file).exists() {
                changed |= self
                    .download(src.bios_file, src.bios_url, |b| {
                        let sum = hex::encode(sha2::Sha256::digest(b));
                        anyhow::ensure!(sum == src.bios_sha256, "unexpected SHA-256 {sum}");
                        Ok(b.to_vec())
                    })
                    .await;
            }
        }
        if changed {
            self.load_cached_dats().await;
        }
    }

    async fn download(&self, name: &str, url: &str, process: impl Fn(&[u8]) -> anyhow::Result<Vec<u8>>) -> bool {
        tracing::info!(url, "downloading DAT");
        let result = async {
            let resp = self.http.get(url).send().await?.error_for_status()?;
            let bytes = resp.bytes().await?;
            let data = process(&bytes)?;
            write_atomic(&dat_dir(&self.config).join(name), &data).await?;
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(url, error = %e, "DAT download failed");
                false
            }
        }
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
