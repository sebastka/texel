//! Texel server: the web app, the BIOS and game library, memory cards, and
//! game metadata from Redump, libretro and (optionally) IGDB.

mod api;
mod auth;
mod cards;
mod dat;
mod db;
mod images;
mod infobox;
mod library;
mod metadata;
mod netplay;
mod overrides;
mod state;
mod verify;
mod zipimage;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(version, about)]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "TEXEL_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// PlayStation BIOS images (read-only; any layout of subdirectories,
    /// zipped or not). Several directories: separated by `:`, or the option
    /// repeated.
    #[arg(
        long,
        env = "TEXEL_PS1_BIOS",
        value_delimiter = ':',
        default_value = "/library/ps1/bios"
    )]
    pub ps1_bios: Vec<PathBuf>,

    /// PlayStation games (read-only): CUE/BIN, ISO, CHD, PBP, zipped images.
    /// Several directories: as for `--ps1-bios`.
    #[arg(
        long,
        env = "TEXEL_PS1_GAMES",
        value_delimiter = ':',
        default_value = "/library/ps1/games"
    )]
    pub ps1_games: Vec<PathBuf>,

    /// Redump's SBI files of LibCrypt-protected PlayStation discs
    /// (read-only), named after the discs (`Ape Escape (France).sbi`): used
    /// for discs without one next to their image. Optional; several
    /// directories: as for `--ps1-bios`.
    #[arg(
        long,
        env = "TEXEL_PS1_SBI",
        value_delimiter = ':',
        default_value = "/library/ps1/sbi"
    )]
    pub ps1_sbi: Vec<PathBuf>,

    /// PlayStation 2 BIOS images (read-only; any layout of subdirectories,
    /// zipped or not). Several directories: as for `--ps1-bios`.
    #[arg(
        long,
        env = "TEXEL_PS2_BIOS",
        value_delimiter = ':',
        default_value = "/library/ps2/bios"
    )]
    pub ps2_bios: Vec<PathBuf>,

    /// PlayStation 2 games (read-only): ISO, CHD, zipped images. Several
    /// directories: as for `--ps1-bios`.
    #[arg(
        long,
        env = "TEXEL_PS2_GAMES",
        value_delimiter = ':',
        default_value = "/library/ps2/games"
    )]
    pub ps2_games: Vec<PathBuf>,

    /// Writable state: memory cards and the metadata cache.
    #[arg(long, env = "TEXEL_STATE", default_value = "/state")]
    pub state: PathBuf,

    /// The built web app.
    #[arg(long, env = "TEXEL_WEB", default_value = "/usr/share/texel/web")]
    pub web: PathBuf,

    /// Fetch metadata (DATs, artwork, IGDB) from third-party websites.
    #[arg(long, env = "TEXEL_FETCH_METADATA", default_value_t = true, action = clap::ArgAction::Set)]
    pub fetch_metadata: bool,

    /// Hash game files in the background to verify them against Redump.
    #[arg(long, env = "TEXEL_VERIFY", default_value_t = true, action = clap::ArgAction::Set)]
    pub verify: bool,

    /// Refresh the game DATs when older than this many days.
    #[arg(long, env = "TEXEL_DAT_MAX_AGE_DAYS", default_value_t = 7)]
    pub dat_max_age_days: u64,

    /// Rescan the library this often, in minutes (0: only at start and on
    /// request).
    #[arg(long, env = "TEXEL_RESCAN_MINUTES", default_value_t = 30)]
    pub rescan_minutes: u64,

    /// IGDB (Twitch) client ID, for descriptions, genres and companies.
    #[arg(long, env = "TEXEL_IGDB_CLIENT_ID")]
    pub igdb_client_id: Option<String>,

    /// IGDB (Twitch) client secret.
    #[arg(long, env = "TEXEL_IGDB_CLIENT_SECRET", hide_env_values = true)]
    pub igdb_client_secret: Option<String>,

    /// Check that a server listening on `--listen` is healthy, and exit.
    #[arg(long)]
    pub healthcheck: bool,

    /// MariaDB (or MySQL) database for saves, settings and hashes:
    /// `mysql://user:password@host:3306/database`.
    #[arg(long, env = "TEXEL_DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// The address users reach Texel at, e.g. `https://texel.example.com`:
    /// for OpenID Connect's redirect URI (`<this>/auth/oidc/callback`),
    /// secure cookies (https), and the check that requests come from
    /// Texel's own pages. Without it, the request's Host is used, over http.
    #[arg(long, env = "TEXEL_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// OpenID Connect: the provider's issuer URL (sign-in with it is offered
    /// when set, with the client id).
    #[arg(long, env = "TEXEL_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,

    #[arg(long, env = "TEXEL_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    /// Absent for a public client (PKCE only).
    #[arg(long, env = "TEXEL_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,

    #[arg(long, env = "TEXEL_OIDC_SCOPES", default_value = "openid profile email")]
    pub oidc_scopes: String,

    /// The claim listing a user's groups (in the ID token or userinfo).
    #[arg(long, env = "TEXEL_OIDC_GROUPS_CLAIM", default_value = "groups")]
    pub oidc_groups_claim: String,

    /// Who may sign in with OpenID Connect: members of any of these groups
    /// (comma-separated). Empty: anyone the provider accepts.
    #[arg(long, env = "TEXEL_OIDC_USER_GROUPS", value_delimiter = ',')]
    pub oidc_user_groups: Vec<String>,

    /// Administrators: members of any of these groups (comma-separated).
    #[arg(long, env = "TEXEL_OIDC_ADMIN_GROUPS", value_delimiter = ',')]
    pub oidc_admin_groups: Vec<String>,

    /// Administrators by subject, e-mail address or user name
    /// (comma-separated).
    #[arg(long, env = "TEXEL_OIDC_ADMIN_USERS", value_delimiter = ',')]
    pub oidc_admin_users: Vec<String>,

    /// The provider's name, on the sign-in button.
    #[arg(long, env = "TEXEL_OIDC_NAME", default_value = "single sign-on")]
    pub oidc_name: String,

    /// Sessions last this many days.
    #[arg(long, env = "TEXEL_SESSION_DAYS", default_value_t = 30)]
    pub session_days: u64,

    /// Invitation and reset tokens expire after this many days.
    #[arg(long, env = "TEXEL_INVITE_DAYS", default_value_t = 7)]
    pub invite_days: u64,

    /// Prints a new invitation token (noted with this text) and exits: to
    /// invite someone without an administrator (no OpenID Connect).
    #[arg(long, value_name = "NOTE")]
    pub create_invite: Option<String>,

    /// Log format.
    #[arg(long, env = "TEXEL_LOG_FORMAT", default_value = "text", value_parser = ["text", "json"])]
    pub log_format: String,
}

impl Config {
    /// OpenID Connect's settings, when configured.
    pub fn oidc(&self) -> Option<auth::oidc::OidcConfig> {
        let list = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Some(auth::oidc::OidcConfig {
            issuer: self.oidc_issuer.clone().filter(|s| !s.is_empty())?,
            client_id: self.oidc_client_id.clone().filter(|s| !s.is_empty())?,
            client_secret: self.oidc_client_secret.clone().filter(|s| !s.is_empty()),
            scopes: self.oidc_scopes.clone(),
            groups_claim: self.oidc_groups_claim.clone(),
            user_groups: list(&self.oidc_user_groups),
            admin_groups: list(&self.oidc_admin_groups),
            admin_users: list(&self.oidc_admin_users),
            name: self.oidc_name.clone(),
        })
    }

    /// Each console's BIOS and game directories (empty paths, as in
    /// `a::b`, skipped).
    pub fn library_roots(&self) -> Vec<library::Root> {
        use library::{Console, Root};
        let dirs = |given: &[PathBuf]| given.iter().filter(|p| !p.as_os_str().is_empty()).cloned().collect();
        vec![
            Root {
                console: Console::Ps1,
                bios: dirs(&self.ps1_bios),
                games: dirs(&self.ps1_games),
                sbi: dirs(&self.ps1_sbi),
            },
            Root {
                console: Console::Ps2,
                bios: dirs(&self.ps2_bios),
                games: dirs(&self.ps2_games),
                sbi: Vec::new(),
            },
        ]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    if config.healthcheck {
        return healthcheck(config.listen).await;
    }
    let provider = rustls::crypto::ring::default_provider().install_default();
    if let Some(note) = &config.create_invite {
        // Only the invitation on the output.
        let db = db::Db::connect(&config.database_url).await?;
        let token = auth::create_invite_cli(&db, note, config.invite_days).await?;
        let base = config
            .public_url
            .as_deref()
            .unwrap_or("http://<texel>")
            .trim_end_matches('/');
        println!(
            "{token}\n{base}/register?token={token}\n(valid {} days)",
            config.invite_days
        );
        return Ok(());
    }
    init_logging(&config.log_format);
    if provider.is_err() {
        tracing::warn!("a rustls crypto provider was already installed");
    }

    let cache = config.state.join("cache");
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;

    for root in config.library_roots() {
        let list = |dirs: &[PathBuf]| {
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        tracing::info!(
            console = root.console.name(),
            bios = %list(&root.bios),
            games = %list(&root.games),
            sbi = %list(&root.sbi),
            "library"
        );
    }

    let db = db::Db::connect(&config.database_url).await?;
    if config.oidc().is_none() && (config.oidc_issuer.is_some() || config.oidc_client_id.is_some()) {
        anyhow::bail!("OpenID Connect needs both TEXEL_OIDC_ISSUER and TEXEL_OIDC_CLIENT_ID");
    }
    let hashes = db.hashes().await.context("loading hashes")?;
    let state = Arc::new(state::AppState::new(config.clone(), db, hashes)?);
    state.load_cached_dats().await;

    // Serving at once: the first scan runs in the background (`/readyz`
    // says so until it is done), and zipped images' seek indexes after it.
    tokio::spawn(background(state.clone()));
    tokio::spawn(state::index_zips(state.clone()));

    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, "serving");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

/// The first scan, DAT refreshes, verification and periodic rescans.
async fn background(state: Arc<state::AppState>) {
    state.rescan().await;
    if state.config.fetch_metadata {
        state.refresh_dats(false).await;
    }
    let mut last_scan = tokio::time::Instant::now();
    loop {
        if let Err(e) = state.db.clean_auth().await {
            tracing::warn!(error = %e, "cleaning sessions up failed");
        }
        metadata::prefetch(&state).await;
        if state.config.verify {
            verify::run(&state).await;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
        let every = state.config.rescan_minutes;
        if every > 0 && last_scan.elapsed() >= Duration::from_secs(every * 60) {
            state.rescan().await;
            if state.config.fetch_metadata {
                state.refresh_dats(false).await;
            }
            last_scan = tokio::time::Instant::now();
        }
    }
}

/// A minimal HTTP probe of /healthz, for container health checks (the
/// runtime image has no shell or curl).
async fn healthcheck(mut addr: SocketAddr) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if addr.ip().is_unspecified() {
        addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        anyhow::ensure!(response.starts_with(b"HTTP/1.1 200"), "unhealthy");
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(5), probe).await?
}

fn init_logging(format: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if format == "json" {
        builder.json().init();
    } else {
        builder.init();
    }
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}
