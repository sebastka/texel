//! Scanning the library directory for BIOS images and games.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use texel_disc::{Disc, FileReader, Layout, ReadError, parse_cue};

use crate::images::{self, Cache, Virtual};
use sha1::Digest;

/// The console a game or a BIOS belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Console {
    Ps1,
    Ps2,
}

impl Console {
    /// What the library paths of this console start with: PS1 paths keep
    /// the bare `games/...` and `bios/...` of the single-console library,
    /// so that game ids and hashes stay the same.
    fn prefix(self) -> &'static str {
        match self {
            Self::Ps1 => "",
            Self::Ps2 => "ps2/",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ps1 => "PlayStation",
            Self::Ps2 => "PlayStation 2",
        }
    }
}

/// Where a console's BIOSes and games are.
#[derive(Clone, Debug)]
pub struct Root {
    pub console: Console,
    pub bios: PathBuf,
    pub games: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Bios {
    pub id: String,
    pub console: Console,
    pub path: PathBuf,
    /// Path relative to the library, for display.
    pub file: String,
    pub sha1: String,
    pub version: Option<String>,
    pub date: Option<String>,
    pub region: &'static str,
    /// What a non-console BIOS comes from, e.g. "PSP firmware 6.60".
    pub variant: Option<&'static str>,
    /// For OpenBIOS: the PCSX-Redux commit it was built from, when known.
    pub commit: Option<String>,
}

/// Build details of an OpenBIOS image, from the `SOURCE.md` that
/// `fetch-openbios.sh` writes next to it: build number, publication date
/// and commit. Only trusted when the SHA-256 it lists for the image matches.
fn openbios_source(path: &Path, rom: &[u8]) -> Option<(String, String, String)> {
    let text = std::fs::read_to_string(path.parent()?.join("SOURCE.md")).ok()?;
    let name = path.file_name()?.to_str()?;
    let sha256 = hex::encode(sha2::Sha256::digest(rom));
    let listed = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("- {name}: ")))
        .map(str::trim)?;
    if listed != sha256 {
        tracing::warn!(file = %path.display(), "SOURCE.md does not describe this OpenBIOS image");
        return None;
    }
    let after = |marker: &str| -> Option<&str> {
        let at = text.find(marker)? + marker.len();
        text[at..]
            .split(|c: char| c.is_whitespace() || c == ',' || c == ')')
            .next()
    };
    let build = after("dev build ")?.to_owned();
    let date = after("published on distrib.app on ")?.trim_end_matches('.').to_owned();
    let commit = after("commit: ")?.to_owned();
    let valid = build.bytes().all(|b| b.is_ascii_digit())
        && date.len() == 10
        && commit.len() == 40
        && commit.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then_some((build, date, commit))
}

#[derive(Clone, Debug)]
pub struct GameFile {
    pub name: String,
    /// The file, or the CHD/PBP container of a virtual file.
    pub path: PathBuf,
    /// Relative path; `container#name` for virtual files.
    pub rel: String,
    pub size: u64,
    pub mtime: u64,
    /// A track or disc inside a compressed container.
    pub virt: Option<Virtual>,
}

/// Reads bytes from a game file, plain or virtual. Blocking.
pub fn read_file(cache: &mut Cache, f: &GameFile, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
    match &f.virt {
        Some(v) => cache.read(&f.path, v, offset, buf),
        None => {
            let mut file = File::open(&f.path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(buf)?;
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
pub struct Game {
    pub id: String,
    pub console: Console,
    /// The CUE sheet or image, relative to the library.
    pub rel: String,
    pub files: Vec<GameFile>,
    pub cue: Option<String>,
    pub serial: Option<String>,
    pub volume_id: Option<String>,
    /// Why the disc could not be read, if it could not.
    pub error: Option<String>,
    /// The SBI file of a disc protected by LibCrypt: its subchannel data.
    pub sbi: Option<PathBuf>,
    /// Pointers to the game's details, from its YAML file (see overrides.rs).
    pub overrides: Option<crate::overrides::Overrides>,
    /// Why that file was not used, when it is wrong.
    pub overrides_error: Option<String>,
}

impl Game {
    /// A name from the file system, used when no DAT knows the game.
    pub fn fallback_title(&self) -> String {
        let path = Path::new(&self.rel);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(&self.rel);
        // `games/SLES-00049/disc.cue`: the directory is more telling.
        if matches!(stem.to_ascii_lowercase().as_str(), "disc" | "game" | "image")
            && let Some(dir) = path.parent().and_then(Path::file_name).and_then(|d| d.to_str())
        {
            return dir.to_owned();
        }
        stem.to_owned()
    }
}

#[derive(Default)]
pub struct Library {
    pub bios: Vec<Bios>,
    pub games: Vec<Game>,
}

impl Library {
    pub fn game(&self, id: &str) -> Option<&Game> {
        self.games.iter().find(|g| g.id == id)
    }

    pub fn bios(&self, id: &str) -> Option<&Bios> {
        self.bios.iter().find(|b| b.id == id)
    }
}

fn short_id(s: &str) -> String {
    hex::encode(&sha2::Sha256::digest(s.as_bytes())[..8])
}

/// A file's path as the library shows it: `games/SLES-00049/disc.chd`,
/// `bios/original/scph1002.bin`, `ps2/games/SLES-51999/disc.chd`, whatever
/// directory holds the games or the BIOSes (`kind`: "games", "bios",
/// "ps2/games"...). Game ids and verification hashes are keyed by it: they
/// do not change when the library moves.
fn rel_path(kind: &str, dir: &Path, path: &Path) -> String {
    let inner = path
        .strip_prefix(dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    format!("{kind}/{inner}")
}

fn mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Scans the BIOS and game directories of each console. Blocking.
pub fn scan(roots: &[Root]) -> Library {
    let mut lib = Library {
        bios: Vec::new(),
        games: Vec::new(),
    };
    for root in roots {
        lib.bios.extend(scan_bios(root.console, &root.bios));
        lib.games.extend(scan_games(root.console, &root.games));
    }
    lib.bios.sort_by(|a, b| a.file.cmp(&b.file));
    lib.games.sort_by(|a, b| a.rel.cmp(&b.rel));
    lib
}

fn scan_bios(console: Console, dir: &Path) -> Vec<Bios> {
    if !dir.is_dir() {
        tracing::warn!(console = console.name(), dir = %dir.display(), "no BIOS directory");
        return Vec::new();
    }
    if console == Console::Ps2 {
        return scan_ps2_bios(dir);
    }
    // Any layout of subdirectories (original/, psxonpsp/, openbios/...):
    // BIOSes are identified by their content.
    let mut files = Vec::new();
    walk(dir, &mut files, 0);
    let mut out = Vec::new();
    for path in files {
        // `fs::metadata` follows symbolic links, unlike `DirEntry::metadata`.
        if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(512 * 1024) {
            continue;
        }
        let Ok(rom) = std::fs::read(&path) else { continue };
        let Some(info) = texel_ps1::bios::identify(&rom) else {
            tracing::warn!(file = %path.display(), "not a PlayStation BIOS, ignored");
            continue;
        };
        let sha1 = hex::encode(sha1::Sha1::digest(&rom));
        let mut info = info;
        let mut commit = None;
        if info.variant == Some("OpenBIOS")
            && let Some((build, date, hash)) = openbios_source(&path, &rom)
        {
            info.version = Some(format!("build {build}"));
            info.date = Some(date);
            commit = Some(hash);
        }
        out.push(Bios {
            console: Console::Ps1,
            id: sha1[..16].to_owned(),
            file: rel_path("bios", dir, &path),
            path,
            sha1,
            version: info.version,
            date: info.date,
            region: info.region.as_str(),
            variant: info.variant,
            commit,
        });
    }
    out
}

fn scan_ps2_bios(dir: &Path) -> Vec<Bios> {
    let mut files = Vec::new();
    walk(dir, &mut files, 0);
    let mut out = Vec::new();
    for path in files {
        if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(texel_ps2::bios::BIOS_SIZE as u64) {
            continue;
        }
        let Ok(rom) = std::fs::read(&path) else { continue };
        let Some(info) = texel_ps2::bios::identify(&rom) else {
            tracing::warn!(file = %path.display(), "not a PlayStation 2 BIOS, ignored");
            continue;
        };
        let sha1 = hex::encode(sha1::Sha1::digest(&rom));
        let open = openps2bios_source(&path, &rom);
        out.push(Bios {
            console: Console::Ps2,
            id: sha1[..16].to_owned(),
            file: rel_path("ps2/bios", dir, &path),
            path,
            sha1,
            version: Some(info.version),
            date: Some(info.date),
            region: info.region.as_str(),
            variant: open.is_some().then_some("OpenPS2BIOS"),
            commit: open.flatten(),
        });
    }
    out
}

/// Whether an image is the OpenPS2BIOS that `fetch-openbios.sh` installed,
/// from the `SOURCE.md` it writes next to it (only trusted when the SHA-256
/// it lists matches), and then its commit, when known. The ROM itself does
/// not name it.
fn openps2bios_source(path: &Path, rom: &[u8]) -> Option<Option<String>> {
    let text = std::fs::read_to_string(path.parent()?.join("SOURCE.md")).ok()?;
    if text.lines().next()?.trim() != "# OpenPS2BIOS" {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let sha256 = hex::encode(sha2::Sha256::digest(rom));
    let listed = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("- {name}: ")))?
        .trim();
    if listed != sha256 {
        tracing::warn!(file = %path.display(), "SOURCE.md does not describe this OpenPS2BIOS image");
        return None;
    }
    let commit = text
        .lines()
        .find_map(|l| l.strip_prefix("- Commit: "))
        .map(|c| c.trim().to_owned());
    Some(commit)
}

/// Directory levels followed, which also stops symbolic link cycles.
const MAX_DEPTH: usize = 16;

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        // Follows symbolic links (to files and directories).
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => walk(&path, out, depth + 1),
            Ok(m) if m.is_file() => out.push(path),
            _ => {}
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Finds a file named in a CUE sheet, case-insensitively.
fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let lower = name.to_ascii_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().to_ascii_lowercase() == lower)
        .map(|e| e.path())
}

fn scan_games(console: Console, dir: &Path) -> Vec<Game> {
    if !dir.is_dir() {
        tracing::warn!(console = console.name(), dir = %dir.display(), "no game directory");
        return Vec::new();
    }
    let kind = format!("{}games", console.prefix());
    let kind = kind.as_str();
    let mut files = Vec::new();
    walk(dir, &mut files, 0);
    let mut used: HashSet<PathBuf> = HashSet::new();
    let mut games = Vec::new();
    for cue in files.iter().filter(|p| extension(p) == "cue") {
        let Ok(text) = std::fs::read_to_string(cue) else {
            continue;
        };
        let base = cue.parent().unwrap_or(dir);
        let layout = parse_cue(&text, |name| {
            resolve(base, name).and_then(|p| p.metadata().ok()).map(|m| m.len())
        });
        let rel = rel_path(kind, dir, cue);
        match layout {
            Ok(layout) => {
                let game_files: Vec<GameFile> = layout
                    .files
                    .iter()
                    .filter_map(|name| {
                        let path = resolve(base, name)?;
                        let meta = path.metadata().ok()?;
                        used.insert(path.clone());
                        Some(GameFile {
                            name: name.clone(),
                            rel: rel_path(kind, dir, &path),
                            path,
                            size: meta.len(),
                            mtime: mtime(&meta),
                            virt: None,
                        })
                    })
                    .collect();
                games.push(make_game(console, rel, Some(text), layout, game_files));
            }
            Err(e) => {
                tracing::warn!(cue = %cue.display(), error = %e, "invalid CUE sheet, ignored");
            }
        }
    }
    for path in files
        .iter()
        .filter(|p| matches!(extension(p).as_str(), "iso" | "bin" | "img"))
    {
        if used.contains(path) {
            continue;
        }
        let Ok(meta) = path.metadata() else { continue };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(layout) = Layout::single_file(&name, meta.len()) else {
            continue;
        };
        let rel = rel_path(kind, dir, path);
        let file = GameFile {
            name,
            rel: rel.clone(),
            path: path.clone(),
            size: meta.len(),
            mtime: mtime(&meta),
            virt: None,
        };
        games.push(make_game(console, rel, None, layout, vec![file]));
    }
    // Compressed containers: their tracks become virtual files.
    for path in files.iter().filter(|p| matches!(extension(p).as_str(), "chd" | "pbp")) {
        let Ok(meta) = path.metadata() else { continue };
        let rel = rel_path(kind, dir, path);
        let opened = if extension(path) == "chd" {
            images::open_chd(path).map(|o| vec![o])
        } else {
            images::open_pbp(path)
        };
        let discs = match opened {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "unreadable image, ignored");
                continue;
            }
        };
        let multi = discs.len() > 1;
        for (i, disc) in discs.into_iter().enumerate() {
            let game_rel = if multi {
                format!("{rel}#disc{}", i + 1)
            } else {
                rel.clone()
            };
            let game_files: Vec<GameFile> = disc
                .files
                .into_iter()
                .map(|(name, v)| GameFile {
                    rel: format!("{rel}#{name}"),
                    name,
                    path: path.clone(),
                    size: v.size(),
                    mtime: mtime(&meta),
                    virt: Some(v),
                })
                .collect();
            let sizes: std::collections::HashMap<String, u64> =
                game_files.iter().map(|f| (f.name.clone(), f.size)).collect();
            match parse_cue(&disc.cue, |n| sizes.get(n).copied()) {
                Ok(layout) => games.push(make_game(console, game_rel, Some(disc.cue), layout, game_files)),
                Err(e) => tracing::warn!(file = %path.display(), error = %e, "bad generated CUE sheet"),
            }
        }
    }
    // A protected disc's subchannel data, named like its image:
    // `disc.chd` → `disc.sbi`, `Game (Europe).cue` → `Game (Europe).sbi`.
    for game in games.iter_mut().filter(|g| !g.rel.contains('#')) {
        let image = dir.join(
            game.rel
                .strip_prefix(kind)
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(&game.rel),
        );
        let sbi = image.with_extension("sbi");
        if sbi.is_file() {
            game.sbi = Some(sbi);
        }
        // Pointers to its details, when they are not found automatically.
        let yaml = image.with_extension("yaml");
        if let Ok(text) = std::fs::read_to_string(&yaml) {
            match crate::overrides::Overrides::parse(&text) {
                Ok(o) => game.overrides = Some(o),
                Err(e) => {
                    tracing::warn!(file = %yaml.display(), error = %e, "ignoring a game's YAML file");
                    game.overrides_error = Some(e);
                }
            }
        }
    }
    games
}

/// Reads a game's files for the disc layer, plain or virtual.
struct Files<'a> {
    files: &'a [GameFile],
    cache: Cache,
}

impl FileReader for Files<'_> {
    fn read_at(&mut self, file: usize, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let f = self
            .files
            .get(file)
            .ok_or_else(|| ReadError::Io("no such file".into()))?;
        read_file(&mut self.cache, f, offset, buf).map_err(|e| ReadError::Io(e.to_string()))
    }
}

fn make_game(console: Console, rel: String, cue: Option<String>, layout: Layout, files: Vec<GameFile>) -> Game {
    let mut game = Game {
        id: short_id(&rel),
        console,
        rel,
        cue,
        serial: None,
        volume_id: None,
        error: None,
        sbi: None,
        overrides: None,
        overrides_error: None,
        files,
    };
    if game.files.len() != layout.files.len() {
        game.error = Some("some image files are missing".into());
        return game;
    }
    let files = game.files.clone();
    let mut disc = Disc::new(
        layout,
        Files {
            files: &files,
            cache: Cache::default(),
        },
    );
    match texel_disc::read_system_cnf(&mut disc) {
        Ok(cnf) => {
            game.serial = cnf.serial;
            game.volume_id = Some(cnf.volume_id).filter(|v| !v.is_empty());
            let found = if cnf.ps2 { Console::Ps2 } else { Console::Ps1 };
            if found != console {
                game.error = Some(format!("a {} disc, in the {} library", found.name(), console.name()));
            }
        }
        Err(e) => game.error = Some(format!("not a {} disc: {e}", console.name())),
    }
    game
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openbios_source_is_read_only_when_it_matches() {
        let dir = std::env::temp_dir().join(format!("texel-openbios-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rom = b"not really a ROM".to_vec();
        let path = dir.join("openbios.bin");
        let sha256 = hex::encode(sha2::Sha256::digest(&rom));
        let commit = "6e6317414b9a62d690e691b6073f61305fca1ba2";
        let source = |hash: &str| {
            format!(
                "# OpenBIOS\n\n- Extracted from the PCSX-Redux Linux x86_64 dev build 585 (AppImage, `x`), \
                 published on distrib.app on 2026-11-20.\n- PCSX-Redux commit: {commit}\n\nSHA-256:\n\n- openbios.bin: {hash}\n"
            )
        };
        std::fs::write(dir.join("SOURCE.md"), source(&sha256)).unwrap();
        assert_eq!(
            openbios_source(&path, &rom),
            Some(("585".into(), "2026-11-20".into(), commit.into()))
        );
        std::fs::write(dir.join("SOURCE.md"), source(&"0".repeat(64))).unwrap();
        assert_eq!(openbios_source(&path, &rom), None);
        std::fs::write(dir.join("SOURCE.md"), format!("- openbios.bin: {sha256}\n")).unwrap();
        assert_eq!(openbios_source(&path, &rom), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
