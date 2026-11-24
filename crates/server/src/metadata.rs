//! Artwork (libretro-thumbnails) and optional game details (IGDB), cached
//! on disk.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::library::Console;
use crate::overrides::Overrides;
use crate::state::{AppState, cache_key, write_atomic};

/// libretro-thumbnails, for a console; and the cache directory of its images
/// (PlayStation images keep the one they had before other consoles).
fn thumbnails(console: Console) -> (&'static str, &'static str) {
    match console {
        Console::Ps1 => (
            "https://raw.githubusercontent.com/libretro-thumbnails/Sony_-_PlayStation/master",
            "",
        ),
        Console::Ps2 => (
            "https://raw.githubusercontent.com/libretro-thumbnails/Sony_-_PlayStation_2/master",
            "ps2",
        ),
    }
}
/// Retry missing artwork and details after this long.
const NEGATIVE_TTL: Duration = Duration::from_secs(7 * 86_400);
const DETAILS_TTL: Duration = Duration::from_secs(30 * 86_400);
/// Lookups that found nothing are retried sooner: articles get written,
/// and the lookup improves.
const NOTHING_TTL: Duration = Duration::from_secs(86_400);
/// How the metadata sources name a console.
#[derive(Clone, Copy)]
pub struct Platform {
    /// Its Wikidata item: games list it as a platform (P400).
    wikidata: &'static str,
    /// Its IGDB platform ID.
    igdb: u32,
    /// Its name in English articles.
    name: &'static str,
}

pub fn platform(console: Console) -> Platform {
    match console {
        Console::Ps1 => Platform {
            wikidata: "Q10677",
            igdb: 7,
            name: "PlayStation",
        },
        Console::Ps2 => Platform {
            wikidata: "Q10680",
            igdb: 8,
            name: "PlayStation 2",
        },
    }
}

#[derive(Clone, Copy)]
pub enum ImageKind {
    Boxart,
    Title,
    Snap,
}

impl ImageKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "boxart" => Some(Self::Boxart),
            "title" => Some(Self::Title),
            "snap" => Some(Self::Snap),
            _ => None,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Boxart => "Named_Boxarts",
            Self::Title => "Named_Titles",
            Self::Snap => "Named_Snaps",
        }
    }
}

/// Whether cached details are recent enough; a cached "nothing found"
/// expires sooner.
fn details_fresh(path: &std::path::Path) -> bool {
    let nothing = std::fs::read(path).is_ok_and(|b| b == b"null");
    fresh(path, if nothing { NOTHING_TTL } else { DETAILS_TTL })
}

fn fresh(path: &std::path::Path, ttl: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < ttl)
}

/// What a "missing" marker holds: the version of the artwork lookup that
/// wrote it. A lookup that tries more names gets a new one, and the markers
/// of older ones lapse (an empty marker is from before the other cases; 2,
/// before other punctuation).
const MISSING: &[u8] = b"3\n";

/// Whether a "missing" marker still holds: fresh, written by this version
/// of the lookup (`MISSING`), and after the DATs last changed
/// (`dats_changed`, written then). New DATs can rename a game, and free
/// names it used to have that libretro-thumbnails still files its images
/// under (`older_names`, which skips names another game has): its artwork
/// is looked for again.
fn still_missing(marker: &std::path::Path, dats_changed: &std::path::Path, ttl: Duration) -> bool {
    let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    fresh(marker, ttl)
        && std::fs::read(marker).is_ok_and(|v| v == MISSING)
        && match (modified(marker), modified(dats_changed)) {
            (Some(marker), Some(dats)) => marker >= dats,
            _ => true,
        }
}

/// libretro-thumbnails names files after the DAT name, with these
/// characters replaced by underscores.
fn thumbnail_name(name: &str) -> String {
    name.chars()
        .map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c })
        .collect()
}

fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The cached artwork of a game (by Redump name), downloading it if needed.
/// `pinned`: the libretro-thumbnails name the game's YAML file gives
/// (`thumbnails:`), used instead of any name derived from the Redump one.
pub async fn image(
    state: &AppState,
    console: Console,
    name: &str,
    pinned: Option<&str>,
    kind: ImageKind,
) -> Option<PathBuf> {
    let (thumbnails, sub) = thumbnails(console);
    let dir = state
        .config
        .state
        .join("cache")
        .join("images")
        .join(sub)
        .join(kind.dir());
    let key = match pinned {
        Some(p) => cache_key(&format!("{name}\n{p}")),
        None => cache_key(name),
    };
    let path = dir.join(format!("{key}.png"));
    if path.exists() {
        return Some(path);
    }
    let missing = dir.join(format!("{key}.missing"));
    let dats_changed = crate::state::dat_dir(&state.config).join("versions.json");
    if !state.config.fetch_metadata || still_missing(&missing, &dats_changed, NEGATIVE_TTL) {
        return None;
    }
    // The YAML file's name; or the DAT name, then names libretro-thumbnails
    // may still use for this disc, from before Redump renamed it: only those
    // no other game has.
    let names: Vec<String> = match pinned {
        Some(p) => vec![thumbnail_name(p)],
        None => {
            let dats = state.dats.read().await.clone();
            std::iter::once(name.to_owned())
                .chain(
                    older_names(name)
                        .into_iter()
                        .filter(|n| dats.get(console).game(n).is_none()),
                )
                .map(|n| thumbnail_name(&n))
                .collect()
        }
    };
    for candidate in &names {
        match fetch_thumbnail(state, thumbnails, kind, candidate).await {
            Ok(Some(bytes)) => {
                write_atomic(&path, &bytes).await.ok()?;
                return Some(path);
            }
            Ok(None) => {}
            Err(()) => return None,
        }
    }
    // The same names, in another case or punctuation: libretro-thumbnails
    // keeps some older ones ("WipEout 3 - Special Edition", Redump's "Wipeout
    // 3"; "Mission Pack 1", Redump's "Mission Pack #1"). Only a name that
    // matches one file, or one in another case only.
    if let Some(index) = thumbnail_index(state, console, kind).await {
        for candidate in &names {
            let Some(actual) = index
                .get(&loose_name(candidate))
                .and_then(|files| match files.as_slice() {
                    [one] => Some(one),
                    many => many.iter().find(|f| f.to_lowercase() == candidate.to_lowercase()),
                })
            else {
                continue;
            };
            if actual == candidate {
                continue;
            }
            match fetch_thumbnail(state, thumbnails, kind, actual).await {
                Ok(Some(bytes)) => {
                    write_atomic(&path, &bytes).await.ok()?;
                    return Some(path);
                }
                Ok(None) => {}
                Err(()) => return None,
            }
        }
    }
    let _ = write_atomic(&missing, MISSING).await;
    None
}

/// One image of libretro-thumbnails, by file name (without `.png`):
/// `Ok(None)` when there is none, `Err` when it cannot be told (logged).
async fn fetch_thumbnail(
    state: &AppState,
    thumbnails: &str,
    kind: ImageKind,
    file: &str,
) -> Result<Option<Vec<u8>>, ()> {
    let mut url = format!("{thumbnails}/{}/{}.png", kind.dir(), encode_path_segment(file));
    let result = async {
        // libretro-thumbnails stores some images as git symbolic links,
        // which GitHub serves as their target's name: follow one.
        for _ in 0..2 {
            let resp = state.http.get(&url).send().await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return anyhow::Ok(None);
            }
            let bytes = resp.error_for_status()?.bytes().await?;
            if bytes.starts_with(b"\x89PNG") {
                return Ok(Some(bytes.to_vec()));
            }
            match std::str::from_utf8(&bytes).map(str::trim) {
                Ok(target) if target.len() < 512 && target.ends_with(".png") && !target.contains(['/', '\\']) => {
                    url = format!("{thumbnails}/{}/{}", kind.dir(), encode_path_segment(target));
                }
                // Not an image: remembered as missing, not fetched again and again.
                _ => return Ok(None),
            }
        }
        Ok(None)
    }
    .await;
    result.map_err(|e| tracing::warn!(url, error = %e, "artwork download failed"))
}

/// A name as `loose_name` compares it: letters and digits, in lower case,
/// in words.
fn loose_name(name: &str) -> String {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The file names (without `.png`) of a kind of libretro-thumbnails image,
/// by `loose_name`: loose name → actual names. Listed with GitHub's API (two
/// requests: the repository's top directory, then this one), cached a week
/// in the state directory.
async fn thumbnail_index(
    state: &AppState,
    console: Console,
    kind: ImageKind,
) -> Option<std::collections::HashMap<String, Vec<String>>> {
    let (thumbnails, sub) = thumbnails(console);
    let repo = thumbnails
        .strip_prefix("https://raw.githubusercontent.com/")?
        .strip_suffix("/master")?;
    let path = state
        .config
        .state
        .join("cache")
        .join("images")
        .join(sub)
        .join(format!("{}.names.json", kind.dir()));
    if state.config.fetch_metadata && !fresh(&path, NEGATIVE_TTL) {
        match list_thumbnails(state, repo, kind.dir()).await {
            Ok(names) => {
                let _ = write_atomic(&path, &serde_json::to_vec(&names).unwrap_or_default()).await;
            }
            // An older list, if any, is still used.
            Err(e) => tracing::warn!(repo, dir = kind.dir(), error = %e, "listing libretro-thumbnails failed"),
        }
    }
    let names: Vec<String> = serde_json::from_slice(&tokio::fs::read(&path).await.ok()?).ok()?;
    let mut index: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for n in names {
        index.entry(loose_name(&n)).or_default().push(n);
    }
    Some(index)
}

async fn list_thumbnails(state: &AppState, repo: &str, dir: &str) -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;
    let tree = |sha: String| {
        let url = format!("https://api.github.com/repos/{repo}/git/trees/{sha}");
        async move {
            let value: serde_json::Value = state
                .http
                .get(&url)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(
                !value["truncated"].as_bool().unwrap_or(false),
                "{url}: listing truncated"
            );
            anyhow::Ok(value)
        }
    };
    let root = tree("master".to_owned()).await?;
    let sha = root["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["path"] == dir && e["type"] == "tree")
        .and_then(|e| e["sha"].as_str())
        .with_context(|| format!("no {dir} directory"))?
        .to_owned();
    let listing = tree(sha).await?;
    Ok(listing["tree"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["path"].as_str()?.strip_suffix(".png").map(str::to_owned))
        .collect())
}

/// Names a disc may have had in older Redump DATs, which libretro-thumbnails
/// still files some images under: without its language tag ("World Cup 98
/// (USA) (En,Fr,De,Es,Nl,Sv,Da)" → "World Cup 98 (USA)"), with only its first
/// region ("Destruction Derby 2 (USA, Canada)" → "Destruction Derby 2
/// (USA)"), and both. The caller keeps those no other game is named.
fn older_names(name: &str) -> Vec<String> {
    let is_languages = |group: &str| {
        group.split(',').all(|l| {
            let b = l.as_bytes();
            (b.len() == 2 || (b.len() == 7 && b[2] == b'-')) && b[0].is_ascii_uppercase() && b[1].is_ascii_lowercase()
        })
    };
    let parts: Vec<&str> = name.split(" (").collect();
    let variant = |no_languages: bool, first_region: bool| {
        let mut out = parts[0].to_owned();
        for (i, part) in parts.iter().enumerate().skip(1) {
            let group = part.strip_suffix(')');
            if no_languages && group.is_some_and(is_languages) {
                continue;
            }
            out.push_str(" (");
            // Redump's first group is the region list.
            match group
                .and_then(|g| g.split_once(", "))
                .filter(|_| first_region && i == 1)
            {
                Some((first, _)) => {
                    out.push_str(first);
                    out.push(')');
                }
                None => out.push_str(part),
            }
        }
        out
    };
    let mut names: Vec<String> = Vec::new();
    for v in [variant(true, false), variant(false, true), variant(true, true)] {
        if v != name && !names.contains(&v) {
            names.push(v);
        }
    }
    names
}

/// What is known about a game, merged from Wikipedia/Wikidata and IGDB.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Details {
    pub summary: Option<String>,
    /// Where the summary comes from: "Wikipedia" or "IGDB".
    pub summary_source: Option<String>,
    /// Wikipedia's short description, e.g. "1997 sim racing video game".
    pub tagline: Option<String>,
    pub genres: Vec<String>,
    pub developers: Vec<String>,
    pub publishers: Vec<String>,
    /// Single-player, multiplayer...
    pub modes: Vec<String>,
    /// YYYY-MM-DD, first release.
    pub release_date: Option<String>,
    /// IGDB's aggregated rating, out of 100.
    pub rating: Option<f64>,
    pub wikipedia_url: Option<String>,
    pub igdb_url: Option<String>,
    /// A source failed (Wikidata rate-limits): shown, but not cached, so
    /// that the next request tries again.
    #[serde(skip)]
    pub partial: bool,
}

impl Details {
    /// Fills what `self` lacks from `other`.
    fn merge(mut self, other: Self) -> Self {
        if self.summary.is_none() {
            self.summary = other.summary;
            self.summary_source = other.summary_source;
        }
        self.tagline = self.tagline.or(other.tagline);
        for (mine, theirs) in [
            (&mut self.genres, other.genres),
            (&mut self.developers, other.developers),
            (&mut self.publishers, other.publishers),
            (&mut self.modes, other.modes),
        ] {
            if mine.is_empty() {
                *mine = theirs;
            }
        }
        self.release_date = self.release_date.or(other.release_date);
        self.rating = self.rating.or(other.rating);
        self.wikipedia_url = self.wikipedia_url.or(other.wikipedia_url);
        self.igdb_url = self.igdb_url.or(other.igdb_url);
        self.partial |= other.partial;
        self
    }
}

pub struct Igdb {
    client_id: String,
    client_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct IgdbGame {
    summary: Option<String>,
    first_release_date: Option<i64>,
    total_rating: Option<f64>,
    url: Option<String>,
    #[serde(default)]
    genres: Vec<Named>,
    #[serde(default)]
    involved_companies: Vec<Involved>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Involved {
    company: Named,
    #[serde(default)]
    developer: bool,
    #[serde(default)]
    publisher: bool,
}

impl Igdb {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            token: Mutex::new(None),
        }
    }

    async fn token(&self, http: &reqwest::Client) -> anyhow::Result<String> {
        let mut token = self.token.lock().await;
        if let Some((t, expires)) = token.as_ref()
            && Instant::now() < *expires
        {
            return Ok(t.clone());
        }
        let t: Token = http
            .post("https://id.twitch.tv/oauth2/token")
            .query(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let expires = Instant::now() + Duration::from_secs(t.expires_in.saturating_sub(300));
        *token = Some((t.access_token.clone(), expires));
        Ok(t.access_token)
    }

    async fn search(&self, http: &reqwest::Client, platform: Platform, title: &str) -> anyhow::Result<Option<Details>> {
        let escaped = title.replace('\\', "").replace('"', "\\\"");
        self.query(
            http,
            &format!("search \"{escaped}\"; where platforms = ({});", platform.igdb),
        )
        .await
    }

    /// A game given by its slug (a game's YAML file).
    async fn by_slug(&self, http: &reqwest::Client, slug: &str) -> anyhow::Result<Option<Details>> {
        // Slugs are checked: lowercase letters, digits and dashes.
        self.query(http, &format!("where slug = \"{slug}\";")).await
    }

    async fn query(&self, http: &reqwest::Client, filter: &str) -> anyhow::Result<Option<Details>> {
        let token = self.token(http).await?;
        let query = format!(
            "{filter} fields summary,first_release_date,total_rating,url,genres.name,\
             involved_companies.company.name,involved_companies.developer,involved_companies.publisher; \
             limit 1;"
        );
        let games: Vec<IgdbGame> = http
            .post("https://api.igdb.com/v4/games")
            .header("Client-ID", &self.client_id)
            .bearer_auth(token)
            .body(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(games.into_iter().next().map(|g| {
            let companies = |f: fn(&Involved) -> bool| {
                g.involved_companies
                    .iter()
                    .filter(|c| f(c))
                    .map(|c| c.company.name.clone())
                    .collect()
            };
            Details {
                summary_source: g.summary.as_ref().map(|_| "IGDB".to_owned()),
                summary: g.summary.clone(),
                genres: g.genres.iter().map(|n| n.name.clone()).collect(),
                developers: companies(|c| c.developer),
                publishers: companies(|c| c.publisher),
                release_date: g.first_release_date.map(format_date),
                rating: g.total_rating.map(|r| (r * 10.0).round() / 10.0),
                igdb_url: g.url.clone(),
                ..Details::default()
            }
        }))
    }
}

/// Unix seconds → YYYY-MM-DD (UTC).
fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The cache file of a game's details: by console and title, and by the
/// content of its YAML file, so that editing that file takes effect at once.
/// PlayStation games keep the keys they had before other consoles.
fn details_path(state: &AppState, console: Console, title: &str, overrides: Option<&Overrides>) -> PathBuf {
    let prefix = match console {
        Console::Ps1 => String::new(),
        Console::Ps2 => "ps2\n".to_owned(),
    };
    let key = match overrides {
        Some(o) if *o != Overrides::default() => cache_key(&format!("{prefix}{title}\n{o:?}")),
        _ => cache_key(&format!("{prefix}{title}")),
    };
    state
        .config
        .state
        .join("cache")
        .join("details")
        .join(format!("{key}.json"))
}

/// Details already in the cache, without fetching anything.
pub async fn cached_details(
    state: &AppState,
    console: Console,
    title: &str,
    overrides: Option<&Overrides>,
) -> Option<Details> {
    let bytes = tokio::fs::read(details_path(state, console, title, overrides))
        .await
        .ok()?;
    serde_json::from_slice::<Option<Details>>(&bytes).ok().flatten()
}

/// A title without the edition Redump names after a dash: "Dragon Ball Z -
/// Budokai 3 - Collector's Edition" → "Dragon Ball Z - Budokai 3" (the
/// articles are the games', not their editions').
fn without_edition(title: &str) -> &str {
    match title.rsplit_once(" - ") {
        Some((game, edition))
            if edition.ends_with(" Edition")
                || ["Greatest Hits", "Platinum", "Best", "Essentials", "Director's Cut"].contains(&edition) =>
        {
            without_edition(game)
        }
        _ => title,
    }
}

/// The titles to search a game's details by, in order: the full title, then
/// without its edition. "Bomberman - Party Edition" is a game of its own
/// (without it, "Bomberman" matches nothing); "… - Collector's Edition"
/// has no article, the game has.
fn search_titles(title: &str) -> Vec<&str> {
    let base = without_edition(title);
    if base == title { vec![title] } else { vec![title, base] }
}

/// Game details from Wikipedia/Wikidata and IGDB (when configured), cached.
/// `tags` (Redump's) tell which Wikipedias to search, besides English.
/// Titles whose last lookup failed (Wikimedia rate limits), and when: they
/// are not looked up again before `RETRY_AFTER`, whoever asks (game pages
/// or the background prefetch), so that retries do not prolong the limit.
static FAILED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);
const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

fn failed_recently(title: &str) -> bool {
    let failed = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    failed.get(title).is_some_and(|at| at.elapsed() < RETRY_AFTER)
}

fn set_failed(title: &str, failed: bool) {
    let mut map = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if failed {
        map.insert(title.to_owned(), Instant::now());
    } else {
        map.remove(title);
    }
}

/// `overrides`: the game's YAML file, whose pages replace the search.
pub async fn details(
    state: &AppState,
    console: Console,
    title: &str,
    tags: &[String],
    overrides: Option<&Overrides>,
) -> Option<Details> {
    let path = details_path(state, console, title, overrides);
    if details_fresh(&path) || !state.config.fetch_metadata || failed_recently(title) {
        return cached_details(state, console, title, overrides).await;
    }
    let platform = platform(console);
    let searches = search_titles(title);
    let wiki = match overrides.filter(|o| o.pins_wiki()) {
        Some(o) => wikipedia::pinned(&state.http, platform, o).await,
        None => {
            let languages = wikipedia::languages(tags);
            let mut found = Ok(None);
            for search in &searches {
                found = wikipedia::lookup(&state.http, platform, search, &languages).await;
                if !matches!(found, Ok(None)) {
                    break;
                }
            }
            found
        }
    };
    let igdb = match (&state.igdb, overrides.and_then(Overrides::igdb_slug)) {
        (Some(igdb), Some(slug)) => igdb.by_slug(&state.http, &slug).await,
        (Some(igdb), None) => {
            let mut found = Ok(None);
            for search in &searches {
                found = igdb.search(&state.http, platform, search).await;
                if !matches!(found, Ok(None)) {
                    break;
                }
            }
            found
        }
        (None, _) => Ok(None),
    };
    for e in [wiki.as_ref().err(), igdb.as_ref().err()].into_iter().flatten() {
        tracing::warn!(title, error = %e, "game details lookup failed");
    }
    if wiki.is_err() && !matches!(igdb, Ok(Some(_))) {
        // Rate-limited, or offline: keep what the cache has, retry later.
        set_failed(title, true);
        return cached_details(state, console, title, overrides).await;
    }
    let details = match (wiki.ok().flatten(), igdb.ok().flatten()) {
        (Some(w), Some(i)) => Some(w.merge(i)),
        (w, i) => w.or(i),
    };
    let partial = details.as_ref().is_some_and(|d| d.partial);
    set_failed(title, partial);
    if partial {
        // Keep what the cache has (if anything) for when this fails again.
        return details;
    }
    let _ = write_atomic(&path, &serde_json::to_vec(&details).unwrap_or_default()).await;
    details
}

/// Fetches the details of every game that has none cached, one at a time.
pub async fn prefetch(state: &AppState) {
    if !state.config.fetch_metadata {
        return;
    }
    let lib = state.library.read().await.clone();
    let mut fetched = 0;
    for game in &lib.games {
        let identity = state.identify(game).await;
        let overrides = game.overrides.as_ref();
        if !details_fresh(&details_path(state, game.console, &identity.title, overrides))
            && !failed_recently(&identity.title)
        {
            details(state, game.console, &identity.title, &identity.tags, overrides).await;
            fetched += 1;
            // Gently: each lookup is several requests, and Wikimedia
            // rate-limits bursts.
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    if fetched > 0 {
        tracing::info!(games = fetched, "game details fetched");
    }
}

/// Wikipedia (summary) and Wikidata (genres, companies, modes, date).
mod wikipedia {
    use std::collections::HashMap;

    use serde::Deserialize;
    use serde_json::Value;

    use super::{Details, Platform, encode_path_segment};

    #[derive(Deserialize)]
    struct Summary {
        title: String,
        description: Option<String>,
        extract: Option<String>,
        wikibase_item: Option<String>,
        content_urls: Option<Urls>,
    }

    #[derive(Deserialize)]
    struct Urls {
        desktop: Url,
    }

    #[derive(Deserialize)]
    struct Url {
        page: String,
    }

    /// The words of a title, to compare titles: lowercase, accents folded,
    /// punctuation dropped.
    pub(super) fn words(s: &str) -> Vec<String> {
        let folded: String = s
            .to_lowercase()
            .chars()
            .map(|c| match c {
                'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
                'ç' => 'c',
                'è' | 'é' | 'ê' | 'ë' => 'e',
                'ì' | 'í' | 'î' | 'ï' => 'i',
                'ñ' => 'n',
                'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
                'ù' | 'ú' | 'û' | 'ü' => 'u',
                'ý' | 'ÿ' => 'y',
                c => c,
            })
            .collect();
        folded
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// How well an article title matches the game's title: the words they
    /// share, out of all their words (without the article's "(video game)"
    /// qualifier), at least 3/4. The article "Crash Bandicoot: Warped"
    /// matches the game "Crash Bandicoot 3 - Warped", and "Dragon Ball GT:
    /// Final Bout" matches "Dragon Ball - Final Bout"; "Crash Bandicoot"
    /// does not match the former, nor "Destruction Derby" the game
    /// "Destruction Derby 2". A number the game's title lacks rules an
    /// article out (a sequel). A leading possessive brand may be left out:
    /// "Disney's Tarzan" is "Tarzan (1999 video game)".
    pub(super) fn coverage(wanted: &[String], article: &str) -> Option<f64> {
        use std::collections::HashSet;
        let found = words(article.split(" (").next().unwrap_or(article));
        let found: HashSet<&String> = found.iter().collect();
        let number = |w: &str| {
            w.bytes().all(|b| b.is_ascii_digit()) || ["ii", "iii", "iv", "vi", "vii", "viii", "ix"].contains(&w)
        };
        let score = |wanted: &[String]| -> Option<f64> {
            let wanted: HashSet<&String> = wanted.iter().collect();
            if found.is_empty() || found.iter().any(|w| number(w) && !wanted.contains(w)) {
                return None;
            }
            let share = wanted.intersection(&found).count() as f64 / wanted.union(&found).count() as f64;
            (share >= 0.75).then_some(share)
        };
        let full = score(wanted);
        // "disney" "s" "tarzan": without the possessive brand.
        let branded = (wanted.len() > 2 && wanted[1] == "s")
            .then(|| score(&wanted[2..]))
            .flatten();
        full.into_iter().chain(branded).reduce(f64::max)
    }

    /// "Disney's Action Game featuring Hercules" → "Disney's Action Game
    /// Featuring Hercules": redirects often use title case.
    fn title_case(s: &str) -> String {
        s.split(' ')
            .map(|w| {
                let mut c = w.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            })
            .collect::<Vec<String>>()
            .join(" ")
    }

    /// "video game" in the languages of the regions Redump names.
    fn game_word(lang: &str) -> &'static str {
        match lang {
            "fr" => "jeu vidéo",
            "de" => "Videospiel",
            "es" => "videojuego",
            "it" => "videogioco",
            "nl" => "computerspel",
            "sv" => "datorspel",
            "pt" => "jogo eletrônico",
            _ => "video game",
        }
    }

    /// Whether an article's short description is a game's (not a series').
    pub(super) fn is_game(description: &str) -> bool {
        let d = description.to_lowercase();
        // "1999 action role-playing game", "1997 platform game": in English,
        // a game of a genre is a video game, unless said otherwise.
        let english = (d.contains(" game") || d.starts_with("game"))
            && ![
                "board game",
                "card game",
                "tabletop",
                "game show",
                "dice game",
                "gamebook",
            ]
            .iter()
            .any(|w| d.contains(w));
        let game = english
            || [
                "video game",
                "jeu vidéo",
                "videospiel",
                "computerspiel",
                "videojuego",
                "videogioco",
                "videogame",
                "computerspel",
                "datorspel",
                "jogo eletrônico",
            ]
            .iter()
            .any(|w| d.contains(w));
        let series = [
            "game series",
            "series of",
            "franchise",
            "série de jeux",
            "spielereihe",
            "videospielreihe",
            "serie de videojuegos",
            "serie di videogiochi",
        ]
        .iter()
        .any(|w| d.contains(w));
        game && !series
    }

    /// The Wikipedia languages worth searching for a game of these Redump
    /// tags, besides English: those of single-country releases, when the
    /// disc has no English.
    pub fn languages(tags: &[String]) -> Vec<&'static str> {
        let english = tags.iter().any(|t| t.split(',').any(|l| l.trim() == "En"));
        if english {
            return Vec::new();
        }
        tags.iter()
            .flat_map(|t| t.split(", "))
            .filter_map(|region| match region {
                "France" => Some("fr"),
                "Germany" => Some("de"),
                "Spain" => Some("es"),
                "Italy" => Some("it"),
                "Netherlands" => Some("nl"),
                "Sweden" => Some("sv"),
                "Brazil" | "Portugal" => Some("pt"),
                _ => None,
            })
            .collect()
    }

    async fn summary(http: &reqwest::Client, lang: &str, page: &str) -> anyhow::Result<Option<Summary>> {
        // Follows redirects: "Crash Bandicoot 3: Warped" leads to its article.
        let url = format!(
            "https://{lang}.wikipedia.org/api/rest_v1/page/summary/{}",
            encode_path_segment(&page.replace(' ', "_"))
        );
        let resp = http.get(&url).send().await?;
        // No such article; anything else (rate limits: 429) is an error, so
        // that the lookup is retried rather than cached as "nothing found".
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(resp.error_for_status()?.json().await?))
    }

    /// The game's article on one Wikipedia: from a search, or the title
    /// itself as a page name (with the colon Redump writes as " - ").
    /// Whether an article mentions the console: "PlayStation", but not as
    /// part of "PlayStation 2" (or 3...).
    pub(super) fn mentions(text: &str, name: &str) -> bool {
        text.match_indices(name).any(|(at, _)| {
            let rest = text[at + name.len()..].trim_start_matches(' ');
            !rest.starts_with(|c: char| c.is_ascii_digit())
        })
    }

    async fn find(
        http: &reqwest::Client,
        platform: Platform,
        lang: &str,
        title: &str,
    ) -> anyhow::Result<Option<Summary>> {
        let search: Value = http
            .get(format!("https://{lang}.wikipedia.org/w/api.php"))
            .query(&[
                ("action", "query"),
                ("list", "search"),
                ("srsearch", &format!("{title} {}", game_word(lang))),
                ("srlimit", "6"),
                ("srnamespace", "0"),
                ("format", "json"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let wanted = words(title);
        // (page, is the game's own title): found by searching, or the title
        // itself, whose redirect is trusted ("Spyro 2: Gateway to Glimmer",
        // the European title, leads to "Spyro 2: Ripto's Rage!").
        let mut pages: Vec<(String, bool)> = search["query"]["search"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["title"].as_str())
            .filter(|t| coverage(&wanted, t).is_some())
            .take(4)
            .map(|t| (t.to_owned(), false))
            .collect();
        let colon = title.replace(" - ", ": ");
        for page in [title_case(&colon), title_case(title), colon, title.to_owned()] {
            pages.retain(|(p, _)| *p != page);
            pages.push((page, true));
        }
        let mut best: Option<(f64, Summary)> = None;
        for (page, own_title) in pages {
            let Some(summary) = summary(http, lang, &page).await? else {
                continue;
            };
            if !is_game(summary.description.as_deref().unwrap_or_default()) {
                continue;
            }
            let share = if own_title {
                1.0
            } else {
                match coverage(&wanted, &summary.title) {
                    Some(share) => share,
                    None => continue,
                }
            };
            let extract = summary.extract.as_deref().unwrap_or_default();
            // "Gran Turismo (1997 video game)" rather than a later remake.
            let score = share * 10.0
                + if mentions(extract, platform.name) { 2.0 } else { 0.0 }
                + if summary.title.contains('(') { 1.0 } else { 0.0 };
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, summary));
            }
        }
        Ok(best.map(|(_, s)| s))
    }

    /// A game Wikipedia has no article on (in English, or the language of
    /// its release) may still have a Wikidata item: "Disney's The Little
    /// Mermaid II" only has a Russian article. Searched by label and alias,
    /// also without a possessive brand; details without a summary.
    async fn wikidata_only(http: &reqwest::Client, platform: Platform, title: &str) -> anyhow::Result<Option<Details>> {
        let wanted = words(title);
        let colon = title.replace(" - ", ": ");
        // "Disney's The Little Mermaid II" → "The Little Mermaid II".
        let unbranded = title
            .split_once(' ')
            .filter(|(first, _)| first.ends_with("'s"))
            .map(|(_, rest)| rest.to_owned());
        let mut queries = vec![title.to_owned()];
        for q in [Some(colon), unbranded].into_iter().flatten() {
            if !queries.contains(&q) {
                queries.push(q);
            }
        }
        let mut best: Option<(f64, String, String)> = None;
        for query in queries {
            let resp: Value = http
                .get("https://www.wikidata.org/w/api.php")
                .query(&[
                    ("action", "wbsearchentities"),
                    ("search", query.as_str()),
                    ("language", "en"),
                    ("uselang", "en"),
                    ("type", "item"),
                    // Games come after series, films, episodes… of the same
                    // name ("Sitting Ducks": the eighth).
                    ("limit", "20"),
                    ("format", "json"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for hit in resp["search"].as_array().into_iter().flatten() {
                let description = hit["description"].as_str().unwrap_or_default();
                if !is_game(description) {
                    continue;
                }
                // The label, or the alias the search matched.
                let names = [hit["label"].as_str(), hit["match"]["text"].as_str()];
                let Some(share) = names
                    .into_iter()
                    .flatten()
                    .filter_map(|n| coverage(&wanted, n))
                    .reduce(f64::max)
                else {
                    continue;
                };
                let Some(id) = hit["id"].as_str() else { continue };
                if best.as_ref().is_none_or(|(s, _, _)| share > *s) {
                    best = Some((share, id.to_owned(), description.to_owned()));
                }
            }
        }
        let Some((_, item, description)) = best else {
            return Ok(None);
        };
        // None when the item is for other platforms only.
        Ok(wikidata(http, platform, &item, false).await?.map(|d| Details {
            tagline: Some(description),
            ..d
        }))
    }

    /// A Wikidata item's English Wikipedia article and description.
    async fn entity_info(http: &reqwest::Client, item: &str) -> anyhow::Result<(Option<String>, Option<String>)> {
        let entity: Value = http
            .get(format!("https://www.wikidata.org/wiki/Special:EntityData/{item}.json"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let e = &entity["entities"][item];
        let article = e["sitelinks"]["enwiki"]["title"].as_str().map(str::to_owned);
        let description = e["descriptions"]["en"]["value"].as_str().map(str::to_owned);
        Ok((article, description))
    }

    /// The details from the pages a game's YAML file gives: its Wikipedia
    /// article, else its Wikidata item (and that item's English article,
    /// if any). Nothing is searched.
    pub async fn pinned(
        http: &reqwest::Client,
        platform: Platform,
        o: &crate::overrides::Overrides,
    ) -> anyhow::Result<Option<Details>> {
        if let Some((lang, title)) = o.wikipedia_page() {
            let Some(page) = summary(http, &lang, &title).await? else {
                tracing::warn!(
                    lang,
                    title,
                    "the Wikipedia article a game's YAML file gives does not exist"
                );
                return Ok(None);
            };
            return finish(http, platform, page, (lang != "en").then_some(lang), true).await;
        }
        let Some(item) = o.wikidata_item() else { return Ok(None) };
        let (article, description) = entity_info(http, &item).await?;
        if let Some(title) = article
            && let Some(page) = summary(http, "en", &title).await?
        {
            return finish(http, platform, page, None, true).await;
        }
        Ok(wikidata(http, platform, &item, true).await?.map(|d| Details {
            tagline: description,
            ..d
        }))
    }

    /// Searches English Wikipedia for the game's article, then those of the
    /// game's other `languages`, going back to the English article of what
    /// they find when there is one.
    pub async fn lookup(
        http: &reqwest::Client,
        platform: Platform,
        title: &str,
        languages: &[&str],
    ) -> anyhow::Result<Option<Details>> {
        let mut found = find(http, platform, "en", title).await?.map(|s| (s, None));
        for lang in languages {
            if found.is_some() {
                break;
            }
            let Some(local) = find(http, platform, lang, title).await? else {
                continue;
            };
            let english = match &local.wikibase_item {
                Some(item) => match entity_info(http, item).await?.0 {
                    Some(en) => summary(http, "en", &en).await?,
                    None => None,
                },
                None => None,
            };
            found = Some(match english {
                Some(en) => (en, None),
                None => (local, Some((*lang).to_owned())),
            });
        }
        let Some((summary, lang)) = found else {
            // No article: Wikidata may still know the game.
            return wikidata_only(http, platform, title).await;
        };
        finish(http, platform, summary, lang, false).await
    }

    /// The details of an article: its summary, its Wikidata item's data,
    /// and what that lacks from its infobox (English articles). `lang`:
    /// the article's language, when not English. `any_platform`: the
    /// article was given, not searched (see `wikidata`).
    async fn finish(
        http: &reqwest::Client,
        platform: Platform,
        summary: Summary,
        lang: Option<String>,
        any_platform: bool,
    ) -> anyhow::Result<Option<Details>> {
        let page = summary.title.clone();
        let source = match &lang {
            Some(lang) => format!("Wikipedia ({lang})"),
            None => "Wikipedia".to_owned(),
        };
        let mut details = Details {
            summary_source: summary.extract.as_ref().map(|_| source),
            summary: summary.extract,
            tagline: summary.description,
            wikipedia_url: summary.content_urls.map(|u| u.desktop.page),
            ..Details::default()
        };
        if let Some(item) = summary.wikibase_item {
            // Wikidata is a bonus: a failure there keeps the summary.
            match wikidata(http, platform, &item, any_platform).await {
                Ok(Some(d)) => details = details.merge(d),
                Ok(None) => return Ok(None),
                Err(e) => {
                    tracing::warn!(item, error = %e, "Wikidata lookup failed");
                    details.partial = true;
                }
            }
        }
        // What Wikidata lacks, from the English article's infobox.
        let lacking = details.developers.is_empty()
            || details.publishers.is_empty()
            || details.genres.is_empty()
            || details.modes.is_empty();
        if lacking && lang.is_none() {
            match wikitext(http, &page).await {
                Ok(Some(text)) => details = details.merge(from_infobox(&text)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(page, error = %e, "reading the article's infobox failed");
                    details.partial = true;
                }
            }
        }
        Ok(Some(details))
    }

    /// The wikitext of an English article's lead section (with its infobox).
    async fn wikitext(http: &reqwest::Client, page: &str) -> anyhow::Result<Option<String>> {
        let resp: Value = http
            .get("https://en.wikipedia.org/w/api.php")
            .query(&[
                ("action", "query"),
                ("prop", "revisions"),
                ("rvprop", "content"),
                ("rvslots", "main"),
                ("rvsection", "0"),
                ("titles", page),
                ("redirects", "1"),
                ("format", "json"),
                ("formatversion", "2"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp["query"]["pages"][0]["revisions"][0]["slots"]["main"]["content"]
            .as_str()
            .map(str::to_owned))
    }

    fn from_infobox(text: &str) -> Details {
        let get = |name: &str| {
            crate::infobox::field(text, name)
                .map(|v| crate::infobox::values(&v))
                .unwrap_or_default()
        };
        Details {
            developers: get("developer"),
            publishers: get("publisher"),
            genres: get("genre"),
            modes: get("modes"),
            ..Details::default()
        }
    }

    /// Reads genres, companies, modes and the release date of an item.
    /// Returns `None` when the item is for other platforms only, unless
    /// `any_platform` (an item given by the game's YAML file).
    async fn wikidata(
        http: &reqwest::Client,
        platform: Platform,
        item: &str,
        any_platform: bool,
    ) -> anyhow::Result<Option<Details>> {
        let entity: Value = http
            .get(format!("https://www.wikidata.org/wiki/Special:EntityData/{item}.json"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let claims = &entity["entities"][item]["claims"];
        let values = |prop: &str| -> Vec<Value> {
            claims[prop]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    c["mainsnak"]["datavalue"]["value"]
                        .as_object()
                        .cloned()
                        .map(Value::Object)
                })
                .collect()
        };
        let ids = |prop: &str| -> Vec<String> {
            values(prop)
                .iter()
                .filter_map(|v| v["id"].as_str().map(str::to_owned))
                .collect()
        };
        let platforms = ids("P400");
        if !any_platform && !platforms.is_empty() && !platforms.iter().any(|p| p == platform.wikidata) {
            return Ok(None);
        }
        let (genres, developers, publishers, modes) = (ids("P136"), ids("P178"), ids("P123"), ids("P404"));
        let all: Vec<&String> = genres
            .iter()
            .chain(&developers)
            .chain(&publishers)
            .chain(&modes)
            .collect();
        let mut labels: HashMap<String, String> = HashMap::new();
        for chunk in all.chunks(50) {
            let joined = chunk.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("|");
            let resp: Value = http
                .get("https://www.wikidata.org/w/api.php")
                .query(&[
                    ("action", "wbgetentities"),
                    ("ids", joined.as_str()),
                    ("props", "labels"),
                    // Names (companies) often only have a language-neutral label.
                    ("languages", "en|mul"),
                    ("format", "json"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for id in chunk {
                let labels_of = &resp["entities"][id.as_str()]["labels"];
                if let Some(label) = labels_of["en"]["value"].as_str().or(labels_of["mul"]["value"].as_str()) {
                    labels.insert((*id).clone(), label.to_owned());
                }
            }
        }
        let names = |ids: &[String], tidy: fn(&str) -> String| -> Vec<String> {
            let mut out: Vec<String> = ids.iter().filter_map(|i| labels.get(i)).map(|l| tidy(l)).collect();
            out.dedup();
            out
        };
        let genre = |l: &str| capitalize(l.trim_end_matches(" video game").trim_end_matches(" game"));
        let mode = |l: &str| capitalize(l.trim_end_matches(" video game"));
        let release_date = values("P577")
            .iter()
            .filter_map(|v| v["time"].as_str())
            // "+1997-12-23T00:00:00Z"; precision below a day shows as -00.
            .filter_map(|t| t.get(1..11))
            .map(|d| d.replace("-00", ""))
            .min();
        Ok(Some(Details {
            genres: names(&genres, genre),
            developers: names(&developers, str::to_owned),
            publishers: names(&publishers, str::to_owned),
            modes: names(&modes, mode),
            release_date,
            ..Details::default()
        }))
    }

    fn capitalize(s: &str) -> String {
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loose_names() {
        use super::loose_name;
        assert_eq!(
            loose_name("Grand Theft Auto - Mission Pack #1 - London 1969 (Europe) (En,Fr,De,It) (Rev 1)"),
            loose_name("Grand Theft Auto - Mission Pack 1 - London 1969 (Europe) (En,Fr,De,It) (Rev 1)")
        );
        assert_eq!(loose_name("WipEout 3 (USA)"), "wipeout 3 usa");
        assert_ne!(loose_name("Wipeout 3 (USA)"), loose_name("Wipeout 3 (USA) (Demo)"));
    }

    #[test]
    fn missing_markers_lapse_with_new_dats() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("texel-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (marker, dats) = (dir.join("x.missing"), dir.join("versions.json"));
        let at = |path: &std::path::Path, ago: u64| {
            std::fs::write(path, super::MISSING).unwrap();
            let f = std::fs::File::options().write(true).open(path).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(ago)).unwrap();
        };
        let week = Duration::from_secs(7 * 86_400);
        // Two days old, the DATs older: holds.
        at(&marker, 2 * 86_400);
        at(&dats, 3 * 86_400);
        assert!(super::still_missing(&marker, &dats, week));
        // The DATs changed since: looked for again.
        at(&dats, 3600);
        assert!(!super::still_missing(&marker, &dats, week));
        // No DATs' date: as before, by age only.
        std::fs::remove_file(&dats).unwrap();
        assert!(super::still_missing(&marker, &dats, week));
        at(&marker, 8 * 86_400);
        assert!(!super::still_missing(&marker, &dats, week));
        // Written by an older lookup: looked for again.
        at(&marker, 3600);
        assert!(super::still_missing(&marker, &dats, week));
        std::fs::write(&marker, b"").unwrap();
        assert!(!super::still_missing(&marker, &dats, week));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::*;

    #[test]
    fn editions_are_left_out_of_searches() {
        assert_eq!(
            without_edition("Dragon Ball Z - Budokai 3 - Collector's Edition"),
            "Dragon Ball Z - Budokai 3"
        );
        assert_eq!(without_edition("Gran Turismo - Platinum"), "Gran Turismo");
        assert_eq!(
            without_edition("Crash Bandicoot 3 - Warped"),
            "Crash Bandicoot 3 - Warped"
        );
        assert_eq!(without_edition("Sitting Ducks"), "Sitting Ducks");
        assert_eq!(
            search_titles("Bomberman - Party Edition"),
            ["Bomberman - Party Edition", "Bomberman"]
        );
        assert_eq!(search_titles("Sitting Ducks"), ["Sitting Ducks"]);
    }

    #[test]
    fn names_and_dates() {
        assert_eq!(thumbnail_name("Tom & Jerry: The Movie?"), "Tom _ Jerry_ The Movie_");
        assert_eq!(
            older_names("World Cup 98 (USA) (En,Fr,De,Es,Nl,Sv,Da)"),
            ["World Cup 98 (USA)"]
        );
        assert_eq!(
            older_names("Destruction Derby 2 (USA, Canada)"),
            ["Destruction Derby 2 (USA)"]
        );
        assert_eq!(
            older_names("Spyro the Dragon (Europe, Australia) (En,Fr,De,Es,It)"),
            [
                "Spyro the Dragon (Europe, Australia)",
                "Spyro the Dragon (Europe) (En,Fr,De,Es,It)",
                "Spyro the Dragon (Europe)"
            ]
        );
        assert!(older_names("Driver 2 (USA) (Disc 1) (Rev 1)").is_empty());
        assert_eq!(
            older_names("Some Game (Europe) (En,Fr) (Rev 1)"),
            ["Some Game (Europe) (Rev 1)"]
        );
        assert_eq!(
            encode_path_segment("Rayman (Europe) (En,Fr,De)"),
            "Rayman%20%28Europe%29%20%28En%2CFr%2CDe%29"
        );
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(810_000_000), "1995-09-02");
    }

    #[test]
    fn console_mentions() {
        assert!(wikipedia::mentions(
            "released for the PlayStation and Sega Saturn",
            "PlayStation"
        ));
        assert!(wikipedia::mentions("for the PlayStation.", "PlayStation"));
        assert!(!wikipedia::mentions(
            "released for the PlayStation 2 in 2003",
            "PlayStation"
        ));
        assert!(wikipedia::mentions(
            "released for the PlayStation 2 in 2003",
            "PlayStation 2"
        ));
    }

    #[test]
    fn title_matching() {
        let wanted = wikipedia::words("Crash Bandicoot 3 - Warped");
        assert!(wikipedia::coverage(&wanted, "Crash Bandicoot: Warped").is_some());
        assert!(wikipedia::coverage(&wanted, "Crash Bandicoot").is_none());
        let wanted = wikipedia::words("Destruction Derby 2");
        assert!(wikipedia::coverage(&wanted, "Destruction Derby (video game)").is_none());
        assert_eq!(wikipedia::coverage(&wanted, "Destruction Derby 2"), Some(1.0));
        let wanted = wikipedia::words("Nickelodeon Les Razmoket a Paris - Le Film");
        assert!(wikipedia::coverage(&wanted, "Les Razmoket à Paris, le film (jeu vidéo)").is_some());
        let wanted = wikipedia::words("Gran Turismo");
        assert_eq!(
            wikipedia::coverage(&wanted, "Gran Turismo (1997 video game)"),
            Some(1.0)
        );
        assert!(wikipedia::coverage(&wanted, "Gran Turismo 2").is_none());
        let wanted = wikipedia::words("Disney's Tarzan");
        assert_eq!(wikipedia::coverage(&wanted, "Tarzan (1999 video game)"), Some(1.0));
        let wanted = wikipedia::words("Dragon Ball - Final Bout");
        assert!(wikipedia::coverage(&wanted, "Dragon Ball GT: Final Bout").is_some());
        assert!(wikipedia::coverage(&wanted, "Dragon Ball Z: Ultimate Battle 22").is_none());
        let wanted = wikipedia::words("Crash Bandicoot");
        assert!(wikipedia::coverage(&wanted, "Crash Bandicoot 2: Cortex Strikes Back").is_none());
    }

    #[test]
    fn game_descriptions() {
        for d in [
            "1997 video game",
            "1999 action role-playing game",
            "1997 platform game",
            "jeu vidéo sorti en 2000",
        ] {
            assert!(wikipedia::is_game(d), "{d}");
        }
        for d in [
            "video game series",
            "1993 board game",
            "American game show",
            "media franchise",
            "2000 film",
        ] {
            assert!(!wikipedia::is_game(d), "{d}");
        }
    }

    #[test]
    fn languages() {
        let tags = |t: &[&str]| t.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(wikipedia::languages(&tags(&["France"])), ["fr"]);
        assert!(wikipedia::languages(&tags(&["Europe", "En,Fr,De"])).is_empty());
        assert!(wikipedia::languages(&tags(&["France", "En,Fr"])).is_empty());
    }
}
