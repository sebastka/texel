//! The open-source BIOSes, downloaded into the server's cache when a newer
//! one is published: OpenBIOS (PS1, from PCSX-Redux, MIT) and OpenPS2BIOS
//! (PS2, MIT). With them, games start without a BIOS dumped from a console.
//!
//! Each goes in a directory of its own with a `SOURCE.md` naming where it
//! comes from (as `fetch-openbios.sh` writes it; the library scan reads
//! it), and a `VERSION` file: a version already there is not downloaded
//! again. The library scan takes these directories as BIOS directories.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use serde_json::Value;
use sha1::Digest;

use crate::state::write_atomic;

/// OpenBIOS has no release of its own: every PCSX-Redux build ships it.
/// Its Windows build is a plain zip (the Linux one, an AppImage).
const REDUX: &str = "https://distrib.app";
const REDUX_MANIFESTS: &str = "https://distrib.app/storage/manifests/pcsx-redux/dev-win-x64";
const OPENPS2BIOS_API: &str = "https://api.github.com/repos/TamaribaEmu/OpenPS2BIOS";

/// Brings both up to date in `dir` (`ps1/`, `ps2/`): whether one changed.
pub async fn refresh(http: &reqwest::Client, dir: &Path) -> bool {
    let mut changed = false;
    match openbios(http, &dir.join("ps1")).await {
        Ok(new) => changed |= new,
        Err(e) => tracing::warn!(error = format!("{e:#}"), "fetching OpenBIOS failed"),
    }
    match openps2bios(http, &dir.join("ps2")).await {
        Ok(new) => changed |= new,
        Err(e) => tracing::warn!(error = format!("{e:#}"), "fetching OpenPS2BIOS failed"),
    }
    changed
}

/// The version in `dir`, if its BIOS is there.
async fn current(dir: &Path, rom: &str) -> Option<String> {
    if !dir.join(rom).is_file() {
        return None;
    }
    let v = tokio::fs::read_to_string(dir.join("VERSION")).await.ok()?;
    Some(v.trim().to_owned())
}

async fn json(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    Ok(http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn bytes(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    Ok(http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec())
}

/// OpenBIOS, from the newest PCSX-Redux build.
async fn openbios(http: &reqwest::Client, dir: &Path) -> anyhow::Result<bool> {
    let catalog = json(http, &format!("{REDUX_MANIFESTS}/manifest.json")).await?;
    let build = catalog["builds"][0]["id"]
        .as_u64()
        .context("no build in the manifest")?;
    if current(dir, "openbios.bin").await.as_deref() == Some(build.to_string().as_str()) {
        return Ok(false);
    }
    let m = json(http, &format!("{REDUX_MANIFESTS}/manifest-{build}.json")).await?;
    let path = m["path"].as_str().context("no download path")?;
    ensure!(
        path.starts_with("/storage/assets/") && path.ends_with(".zip") && !path.contains(".."),
        "unexpected download path {path}"
    );
    let sha1 = m["hashes"]["sha1"].as_str().context("no SHA-1")?.to_ascii_lowercase();
    let commit = m["manifest"]["changeset"].as_str().unwrap_or_default().to_owned();
    let created = m["createdAt"].as_i64().map(date).unwrap_or_default();
    tracing::info!(build, "downloading OpenBIOS (PCSX-Redux's build, once)");
    let zip = bytes(http, &format!("{REDUX}{path}")).await?;
    ensure!(
        hex::encode(sha1::Sha1::digest(&zip)) == sha1,
        "the build's SHA-1 is not its manifest's"
    );
    let rom = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip))?;
        let mut f = archive
            .by_name("openbios.bin")
            .context("no openbios.bin in the build")?;
        let mut rom = Vec::new();
        f.read_to_end(&mut rom)?;
        Ok(rom)
    })
    .await??;
    ensure!(
        rom.len() == 512 * 1024 && rom.get(0x78..0x80) == Some(&b"OpenBIOS"[..]),
        "openbios.bin is not an OpenBIOS ROM"
    );
    let sha256 = hex::encode(sha2::Sha256::digest(&rom));
    let source = format!(
        "# OpenBIOS\n\nOpen-source PlayStation BIOS replacement from PCSX-Redux, MIT licensed: \
         https://github.com/pcsx-redux/nugget/tree/main/openbios\n\n\
         Downloaded by the Texel server, do not edit.\n\n\
         - Extracted from the PCSX-Redux Windows x64 dev build {build}, published on distrib.app on {created}.\n\
         - PCSX-Redux commit: {commit}\n\
         - Build zip: {REDUX}{path} (SHA-1 {sha1})\n\n\
         SHA-256:\n\n- openbios.bin: {sha256}\n"
    );
    install(dir, "openbios.bin", &rom, &source, &build.to_string()).await?;
    tracing::info!(build, "OpenBIOS installed");
    Ok(true)
}

/// OpenPS2BIOS, from its newest GitHub release.
async fn openps2bios(http: &reqwest::Client, dir: &Path) -> anyhow::Result<bool> {
    let release = json(http, &format!("{OPENPS2BIOS_API}/releases/latest")).await?;
    let tag = release["tag_name"].as_str().context("no release tag")?.to_owned();
    ensure!(
        tag.starts_with('v') && tag.len() > 1 && tag[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "unexpected release tag {tag}"
    );
    if current(dir, "openps2bios.bin").await.as_deref() == Some(tag.as_str()) {
        return Ok(false);
    }
    let assets = release["assets"].as_array().context("no assets")?;
    let named = |n: &str| assets.iter().find(|a| a["name"].as_str() == Some(n));
    let Some(rom_asset) = assets.iter().find(|a| {
        a["name"]
            .as_str()
            .and_then(|n| n.strip_prefix("OpenPS2BIOS-"))
            .and_then(|n| n.strip_suffix(".bin"))
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
    }) else {
        bail!("no OpenPS2BIOS-<version>.bin in release {tag}");
    };
    let name = rom_asset["name"].as_str().unwrap_or_default().to_owned();
    let url = rom_asset["browser_download_url"]
        .as_str()
        .context("no download URL")?
        .to_owned();
    let published = release["published_at"]
        .as_str()
        .unwrap_or_default()
        .get(..10)
        .unwrap_or_default()
        .to_owned();
    tracing::info!(%tag, "downloading OpenPS2BIOS (its release, once)");
    let rom = bytes(http, &url).await?;
    ensure!(rom.len() == texel_ps2::bios::BIOS_SIZE, "{name} is not 4 MiB");
    let sha256 = hex::encode(sha2::Sha256::digest(&rom));
    // GitHub's digest of the asset, and the release's own .sha256 file.
    if let Some(digest) = rom_asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")) {
        ensure!(digest == sha256, "{name}'s SHA-256 is not GitHub's digest");
    }
    if let Some(sums) = named(&format!("{name}.sha256")).and_then(|a| a["browser_download_url"].as_str()) {
        let text = String::from_utf8(bytes(http, sums).await?)?;
        ensure!(
            text.split_whitespace().next() == Some(sha256.as_str()),
            "{name}'s SHA-256 is not its .sha256 file's"
        );
    }
    ensure!(
        texel_ps2::bios::identify(&rom).is_some(),
        "{name} is not a PlayStation 2 BIOS"
    );
    let commit = json(http, &format!("{OPENPS2BIOS_API}/commits/{tag}"))
        .await
        .ok()
        .and_then(|c| c["sha"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let source = format!(
        "# OpenPS2BIOS\n\nExperimental open-source PlayStation 2 BIOS replacement for emulators, MIT licensed: \
         https://github.com/TamaribaEmu/OpenPS2BIOS\n\n\
         Downloaded by the Texel server, do not edit.\n\n\
         - Release {tag}, published on GitHub on {published}: {url}\n\
         - Commit: {commit}\n\n\
         SHA-256:\n\n- openps2bios.bin: {sha256}\n"
    );
    install(dir, "openps2bios.bin", &rom, &source, &tag).await?;
    tracing::info!(%tag, "OpenPS2BIOS installed");
    Ok(true)
}

/// Writes the ROM, its SOURCE.md and VERSION (the ROM first: a VERSION
/// never names a ROM that is not there).
async fn install(dir: &Path, rom_name: &str, rom: &[u8], source: &str, version: &str) -> anyhow::Result<()> {
    write_atomic(&dir.join(rom_name), rom).await?;
    write_atomic(&dir.join("SOURCE.md"), source.as_bytes()).await?;
    write_atomic(&dir.join("VERSION"), format!("{version}\n").as_bytes()).await?;
    Ok(())
}

/// A Unix time's date, `YYYY-MM-DD` (UTC).
fn date(secs: i64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(951_782_400), "2000-02-29");
        assert_eq!(super::date(1_795_491_081), "2026-11-24");
    }
}
