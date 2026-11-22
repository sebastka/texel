//! HTTP routes: the JSON API, file downloads and the web app.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::{self, Admin, CurrentUser, Viewer};
use crate::library::{Console, Game};
use crate::metadata::{self, ImageKind};
use std::collections::HashMap;

use crate::state::{AppState, DiscGroup, Identity};

type AppStateRef = State<Arc<AppState>>;

const MEMCARD_SIZE: usize = texel_ps1::MEMCARD_SIZE;
/// A PlayStation 2 memory card: 16384 pages of 512 bytes and 16 of ECC
/// (`texel_ps2::memcard::SIZE`).
const PS2_MEMCARD_SIZE: usize = 16_384 * (512 + 16);
/// Save states are compressed by the browser; their content is opaque here.
const MAX_STATE: usize = 32 << 20;
const MAX_SCREENSHOT: usize = 8 << 20;
const STATE_SLOTS: u8 = 4;

pub fn router(state: Arc<AppState>) -> Router {
    let web = &state.config.web;
    let spa = ServeDir::new(web).fallback(ServeFile::new(web.join("index.html")));
    let api = Router::new()
        .route("/auth/config", get(auth::config))
        .route("/auth/login", post(auth::password_login))
        .route("/auth/register", post(auth::register))
        .route("/auth/invites/{token}", get(auth::invite))
        .route("/auth/logout", post(auth::logout))
        .route("/me", get(auth::me).patch(auth::update_me))
        .route("/users/{id}/avatar", get(auth::user_avatar))
        .route("/admin/users", get(auth::list_users))
        .route("/admin/users/{id}", axum::routing::delete(auth::delete_user))
        .route("/admin/users/{id}/reset", post(auth::reset_user))
        .route("/admin/invites", get(auth::list_invites).post(auth::create_invite))
        .route("/admin/invites/{id}", axum::routing::delete(auth::delete_invite))
        .route("/cards", get(list_cards))
        .route("/cards/{id}/{slot}", axum::routing::delete(delete_card))
        .route("/cards/{id}/{slot}/saves/{save}", axum::routing::delete(delete_save))
        .route(
            "/cards/{id}/{slot}/saves/{save}/copy",
            post(copy_save).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/bios", get(list_bios))
        .route("/bios/{id}/data", get(bios_data))
        .route("/games", get(list_games))
        .route("/games/{id}", get(game_detail))
        .route("/games/{id}/files/{index}", get(game_file))
        .route("/games/{id}/sbi", get(game_sbi))
        .route("/games/{id}/images/{kind}", get(game_image))
        .route("/memcards", get(list_memcards))
        .route(
            "/games/{id}/memcards/{slot}",
            get(get_memcard)
                .put(put_memcard)
                .layer(DefaultBodyLimit::max(PS2_MEMCARD_SIZE + 1024)),
        )
        .route("/games/{id}/states", get(list_states))
        .route("/games/{id}/bios", axum::routing::put(put_bios))
        .route(
            "/games/{id}/states/{slot}",
            get(get_state)
                .put(put_state)
                .delete(delete_state)
                .layer(DefaultBodyLimit::max(MAX_STATE)),
        )
        .route(
            "/games/{id}/states/{slot}/screenshot",
            get(get_screenshot)
                .put(put_screenshot)
                .layer(DefaultBodyLimit::max(MAX_SCREENSHOT)),
        )
        .route("/library/rescan", post(rescan))
        .route("/netplay/host", get(crate::netplay::host))
        .route("/netplay/rooms/{code}", get(crate::netplay::room))
        .route("/netplay/join/{code}", get(crate::netplay::join));
    let headers = [
        // Cross-origin isolation, for SharedArrayBuffer (audio ring buffer).
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-embedder-policy", "require-corp"),
        ("cross-origin-resource-policy", "same-origin"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; connect-src 'self'; worker-src 'self' blob:; object-src 'none'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
    ];
    let mut app = Router::new()
        .nest("/api", api)
        .route("/auth/oidc/login", get(auth::oidc_login))
        .route("/auth/oidc/callback", get(auth::oidc_callback))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .fallback_service(spa)
        .layer(axum::middleware::from_fn(cache_control))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::same_origin))
        .with_state(state);
    for (name, value) in headers {
        let (name, value) = (HeaderName::from_static(name), HeaderValue::from_static(value));
        // A response may set a stricter policy of its own (users' pictures).
        app = if name == header::CONTENT_SECURITY_POLICY {
            app.layer(SetResponseHeaderLayer::if_not_present(name, value))
        } else {
            app.layer(SetResponseHeaderLayer::overriding(name, value))
        };
    }
    app.layer(tower_http::trace::TraceLayer::new_for_http())
}

/// The web app's caching: its built files (`/assets/`) are named by their
/// content's hash, so browsers keep them for good (the WebAssembly cores
/// are megabytes); the page itself (`index.html`, served for every other
/// path) is checked again each time, to pick up a new build. An unknown
/// asset is answered with the page: that answer is not kept. The API sets
/// its own.
async fn cache_control(req: Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path();
    let web = !path.starts_with("/api/") && !path.starts_with("/auth/") && !matches!(path, "/healthz" | "/readyz");
    let asset = path.starts_with("/assets/");
    let mut resp = next.run(req).await;
    if !web || resp.headers().contains_key(header::CACHE_CONTROL) {
        return resp;
    }
    let html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/html"));
    let value = if asset && !html && (resp.status().is_success() || resp.status() == StatusCode::NOT_MODIFIED) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    resp
}

fn forbidden() -> Response {
    (StatusCode::UNAUTHORIZED, "sign in first").into_response()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("{what} not found")).into_response()
}

async fn readyz(State(state): AppStateRef) -> Response {
    if !state.ready.load(Ordering::Relaxed) {
        (StatusCode::SERVICE_UNAVAILABLE, "scanning").into_response()
    } else if !state.db.ping().await {
        (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
    } else {
        "ready".into_response()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BiosJson {
    id: String,
    console: crate::library::Console,
    file: String,
    sha1: String,
    version: Option<String>,
    date: Option<String>,
    region: &'static str,
    /// Redump's description, e.g. "SCPH-1002/DTL-H1002 (Version 2.0 05/10/95 E)",
    /// when the image is a known-good dump.
    model: Option<String>,
    /// Whether the SHA-1 is a known-good dump; `None` without the DAT.
    verified: Option<bool>,
    /// Who lists it as good: "Redump", or "libretro" for BIOSes Redump
    /// does not cover.
    verified_by: Option<&'static str>,
    /// What a non-console BIOS comes from, e.g. "PSP firmware 6.60".
    variant: Option<&'static str>,
    /// For OpenBIOS: the PCSX-Redux commit it was built from.
    commit: Option<String>,
}

async fn list_bios(State(state): AppStateRef, _: CurrentUser) -> Json<Vec<BiosJson>> {
    let lib = state.library.read().await.clone();
    let mut out = Vec::with_capacity(lib.bios.len());
    for b in &lib.bios {
        let (model, dat_loaded) = state.bios_model(b.console, &b.sha1).await;
        // OpenBIOS is built from source: no list of checksums can vouch for it.
        let open_source = matches!(b.variant, Some("OpenBIOS" | "OpenPS2BIOS"));
        let checked = dat_loaded && !open_source;
        out.push(BiosJson {
            verified: checked.then_some(model.is_some()),
            verified_by: model.as_ref().map(|(_, by)| *by),
            model: model.map(|(m, _)| m),
            variant: b.variant,
            commit: b.commit.clone(),
            id: b.id.clone(),
            console: b.console,
            file: b.file.clone(),
            sha1: b.sha1.clone(),
            version: b.version.clone(),
            date: b.date.clone(),
            region: b.region,
        });
    }
    Json(out)
}

async fn serve_path(path: std::path::PathBuf, req: Request, content_type: &'static str) -> Response {
    // ServeFile handles Range and conditional requests.
    let mut resp = match ServeFile::new(path).oneshot(req).await {
        Ok(resp) => resp.into_response(),
        Err(e) => match e {},
    };
    if resp.status().is_success() {
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    resp
}

async fn bios_data(State(state): AppStateRef, viewer: Viewer, Path(id): Path<String>, req: Request) -> Response {
    if !viewer.may_read(&state, None, Some(&id)) {
        return forbidden();
    }
    let lib = state.library.read().await.clone();
    match lib.bios(&id) {
        Some(b) => serve_path(b.path.clone(), req, "application/octet-stream").await,
        None => not_found("BIOS"),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameJson {
    id: String,
    console: crate::library::Console,
    path: String,
    serial: Option<String>,
    #[serde(flatten)]
    identity: Identity,
    error: Option<String>,
    /// Every disc of a multi-disc game, this one included; empty otherwise.
    discs: Vec<crate::state::DiscRef>,
}

async fn game_json(state: &AppState, g: &Game, groups: &HashMap<String, DiscGroup>) -> GameJson {
    GameJson {
        id: g.id.clone(),
        console: g.console,
        path: g.rel.clone(),
        serial: g.serial.clone(),
        identity: state.identify(g).await,
        error: g.error.clone(),
        discs: groups.get(&g.id).map(|gr| gr.discs.clone()).unwrap_or_default(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameListJson {
    #[serde(flatten)]
    game: GameJson,
    /// From the cache only: the list never waits for third parties.
    details: Option<metadata::Details>,
    size: u64,
}

async fn list_games(State(state): AppStateRef, _: CurrentUser) -> Json<Vec<GameListJson>> {
    let lib = state.library.read().await.clone();
    let groups = state.disc_groups().await;
    let mut out = Vec::with_capacity(lib.games.len());
    for g in &lib.games {
        let game = game_json(&state, g, &groups).await;
        let details = metadata::cached_details(&state, g.console, &game.identity.title, g.overrides.as_ref()).await;
        out.push(GameListJson {
            game,
            details,
            size: g.files.iter().map(|f| f.size).sum(),
        });
    }
    out.sort_by_key(|a| a.game.identity.title.to_lowercase());
    Json(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameDetailJson {
    #[serde(flatten)]
    game: GameJson,
    volume_id: Option<String>,
    /// The CUE sheet; absent for single-file images.
    cue: Option<String>,
    /// The image files, checked against Redump.
    files: Vec<crate::state::FileCheck>,
    /// How many tracks the matching Redump entry has.
    redump_tracks: Option<usize>,
    details: Option<metadata::Details>,
    /// The BIOS the game was last started with, or its save states use.
    last_bios: Option<String>,
    /// Whether the disc has an SBI file (LibCrypt protection).
    sbi: bool,
    /// Other pages about the game, from its YAML file: name → URL.
    links: std::collections::BTreeMap<String, String>,
    /// Why the game's YAML file was not used, when it is wrong.
    overrides_error: Option<String>,
}

async fn game_detail(State(state): AppStateRef, viewer: Viewer, Path(id): Path<String>) -> Response {
    if !viewer.may_read(&state, Some(&id), None) {
        return forbidden();
    }
    let lib = state.library.read().await.clone();
    let Some(g) = lib.game(&id) else {
        return not_found("game");
    };
    let game = game_json(&state, g, &state.disc_groups().await).await;
    let details = metadata::details(
        &state,
        g.console,
        &game.identity.title,
        &game.identity.tags,
        g.overrides.as_ref(),
    )
    .await;
    let files = state.file_checks(g, game.identity.name.as_deref()).await;
    let redump_tracks = match &game.identity.name {
        Some(name) => state
            .dats
            .read()
            .await
            .get(g.console)
            .game(name)
            .map(|r| r.bins().count()),
        None => None,
    };
    let last_bios = match (save_keys(&state, &id).await, &viewer.user) {
        (Some(keys), Some(user)) => state
            .db
            .last_bios(user.id, &keys.cards, &keys.states)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "reading the BIOS of a game failed");
                None
            })
            // Gone from the library since.
            .filter(|b| lib.bios(b).is_some()),
        _ => None,
    };
    Json(GameDetailJson {
        game,
        volume_id: g.volume_id.clone(),
        cue: g.cue.clone(),
        files,
        redump_tracks,
        details,
        last_bios,
        sbi: g.sbi.is_some(),
        links: g.overrides.as_ref().map(|o| o.links.clone()).unwrap_or_default(),
        overrides_error: g.overrides_error.clone(),
    })
    .into_response()
}

/// The SBI file of a protected disc, for the emulator.
async fn game_sbi(State(state): AppStateRef, viewer: Viewer, Path(id): Path<String>, req: Request) -> Response {
    if !viewer.may_read(&state, Some(&id), None) {
        return forbidden();
    }
    let lib = state.library.read().await.clone();
    match lib.game(&id).and_then(|g| g.sbi.clone()) {
        Some(path) => serve_path(path, req, "application/octet-stream").await,
        None => not_found("SBI file"),
    }
}

async fn game_file(
    State(state): AppStateRef,
    viewer: Viewer,
    Path((id, index)): Path<(String, usize)>,
    req: Request,
) -> Response {
    if !viewer.may_read(&state, Some(&id), None) {
        return forbidden();
    }
    let lib = state.library.read().await.clone();
    match lib.game(&id).and_then(|g| g.files.get(index)) {
        Some(f) if f.virt.is_none() => serve_path(f.path.clone(), req, "application/octet-stream").await,
        Some(f) => {
            let range = req
                .headers()
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            serve_virtual(&state, f.clone(), range).await
        }
        None => not_found("file"),
    }
}

/// Serves a range of a track inside a CHD/PBP container. Only single
/// ranges are supported, which is what the emulator sends.
async fn serve_virtual(state: &AppState, f: crate::library::GameFile, range: Option<String>) -> Response {
    /// Larger reads are refused: the data is decompressed in memory.
    const MAX_RANGE: u64 = 16 << 20;
    let range = range
        .as_deref()
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| {
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { f.size - 1 } else { b.parse().ok()? };
            Some((start, end.min(f.size.saturating_sub(1))))
        });
    let Some((start, end)) = range.filter(|(s, e)| s <= e && e - s < MAX_RANGE) else {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{}", f.size))],
            "this file is decompressed on the fly: request ranges of at most 16 MiB",
        )
            .into_response();
    };
    let images = state.images.clone();
    let len = (end - start + 1) as usize;
    let read = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; len];
        let mut cache = images.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::library::read_file(&mut cache, &f, start, &mut buf).map(|()| (buf, f.size))
    })
    .await;
    match read {
        Ok(Ok((buf, size))) => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}")),
                (header::ACCEPT_RANGES, "bytes".to_owned()),
            ],
            buf,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reading a compressed image failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "read error").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn game_image(
    State(state): AppStateRef,
    viewer: Viewer,
    Path((id, kind)): Path<(String, String)>,
    req: Request,
) -> Response {
    if !viewer.may_read(&state, Some(&id), None) {
        return forbidden();
    }
    let Some(kind) = ImageKind::parse(&kind) else {
        return not_found("image kind");
    };
    let lib = state.library.read().await.clone();
    let Some(g) = lib.game(&id) else {
        return not_found("game");
    };
    let Some(name) = state.identify(g).await.name else {
        return not_found("image");
    };
    match metadata::image(&state, g.console, &name, kind).await {
        Some(path) => serve_path(path, req, "image/png").await,
        None => not_found("image"),
    }
}

/// The keys of a game's saves (see db.rs): the disc's serial, which stays
/// the same whatever the image format or dump ("SLES-00049"), or
/// "file-<id>" for discs without one. Multi-disc games keep their memory
/// cards and settings with the first disc's, so that every disc shares
/// them; save states stay with their disc.
struct SaveKeys {
    cards: String,
    states: String,
    /// The size of the game's console's memory cards.
    card_size: usize,
}

fn save_key(g: &Game) -> String {
    match g.serial.as_deref() {
        Some(s) if crate::db::valid_key(s) => s.to_owned(),
        _ => format!("file-{}", g.id),
    }
}

async fn save_keys(state: &AppState, id: &str) -> Option<SaveKeys> {
    let lib = state.library.read().await.clone();
    let game = lib.game(id)?;
    let groups = state.disc_groups().await;
    let first_disc = groups
        .get(id)
        .and_then(|g| g.discs.first())
        .and_then(|d| lib.game(&d.id));
    Some(SaveKeys {
        cards: save_key(first_disc.unwrap_or(game)),
        states: save_key(game),
        card_size: match game.console {
            Console::Ps1 => MEMCARD_SIZE,
            Console::Ps2 => PS2_MEMCARD_SIZE,
        },
    })
}

fn db_error(what: &str, e: &anyhow::Error) -> Response {
    tracing::error!(error = %e, "{what} failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{what} failed")).into_response()
}

/// Bytes from the database, never cached by the browser.
fn blob(data: Vec<u8>, mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        data,
    )
        .into_response()
}

/// The games with saved memory cards: their first disc, for a game on
/// several (its discs share them).
async fn list_memcards(State(state): AppStateRef, CurrentUser(user, _): CurrentUser) -> Response {
    let keys: std::collections::HashSet<String> = match state.db.memory_cards(user.id).await {
        Ok(cards) => cards.into_iter().map(|(k, _)| k).collect(),
        Err(e) => return db_error("listing memory cards", &e),
    };
    let lib = state.library.read().await.clone();
    let groups = state.disc_groups().await;
    let ids: Vec<&str> = lib
        .games
        .iter()
        .filter(|g| {
            groups
                .get(&g.id)
                .and_then(|d| d.discs.first())
                .is_none_or(|d| d.id == g.id)
        })
        .filter(|g| keys.contains(&save_key(g)))
        .map(|g| g.id.as_str())
        .collect();
    Json(ids).into_response()
}

async fn get_memcard(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
) -> Response {
    if !(1..=2).contains(&slot) {
        return not_found("memory card");
    }
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.memory_card(user.id, &keys.cards, slot).await {
        Ok(Some(data)) => blob(data, "application/octet-stream"),
        Ok(None) => not_found("memory card"),
        Err(e) => db_error("reading a memory card", &e),
    }
}

async fn put_memcard(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
    body: Bytes,
) -> Response {
    if !(1..=2).contains(&slot) {
        return not_found("memory card");
    }
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    if body.len() != keys.card_size {
        return (
            StatusCode::BAD_REQUEST,
            format!("this game's memory cards are {} bytes", keys.card_size),
        )
            .into_response();
    }
    match state.db.put_memory_card(user.id, &keys.cards, slot, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("saving a memory card", &e),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateJson {
    slot: u8,
    size: u64,
    /// Unix seconds.
    saved_at: u64,
    screenshot: bool,
    /// The BIOS it was saved with (it loads only with it), when known.
    bios: Option<String>,
}

async fn list_states(State(state): AppStateRef, CurrentUser(user, _): CurrentUser, Path(id): Path<String>) -> Response {
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.states(user.id, &keys.states).await {
        Ok(states) => {
            let out: Vec<StateJson> = states
                .into_iter()
                .filter(|s| (1..=STATE_SLOTS).contains(&s.slot))
                .map(|s| StateJson {
                    slot: s.slot,
                    size: s.size,
                    saved_at: s.saved_at,
                    screenshot: s.screenshot,
                    bios: s.bios,
                })
                .collect();
            let mut resp = Json(out).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => db_error("listing save states", &e),
    }
}

async fn get_state(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
) -> Response {
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.state(user.id, &keys.states, slot).await {
        Ok(Some(data)) => blob(data, "application/gzip"),
        Ok(None) => not_found("save state"),
        Err(e) => db_error("reading a save state", &e),
    }
}

async fn get_screenshot(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
) -> Response {
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.screenshot(user.id, &keys.states, slot).await {
        Ok(Some(data)) => blob(data, "image/png"),
        Ok(None) => not_found("screenshot"),
        Err(e) => db_error("reading a screenshot", &e),
    }
}

#[derive(serde::Deserialize)]
struct StateQuery {
    /// The BIOS the state was saved with.
    bios: Option<String>,
}

async fn put_state(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
    Query(q): Query<StateQuery>,
    body: Bytes,
) -> Response {
    if !(1..=STATE_SLOTS).contains(&slot) {
        return not_found("save state slot");
    }
    // Gzip, as compressed by the browser.
    if !body.starts_with(&[0x1f, 0x8b]) {
        return (StatusCode::BAD_REQUEST, "a save state is sent gzip-compressed").into_response();
    }
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    let lib = state.library.read().await.clone();
    let bios = q.bios.filter(|b| lib.bios(b).is_some());
    match state
        .db
        .put_state(user.id, &keys.states, slot, &body, bios.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("saving a save state", &e),
    }
}

async fn put_screenshot(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
    body: Bytes,
) -> Response {
    if !body.starts_with(b"\x89PNG") {
        return (StatusCode::BAD_REQUEST, "a screenshot is a PNG image").into_response();
    }
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.put_screenshot(user.id, &keys.states, slot, &body).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("save state"),
        Err(e) => db_error("saving a screenshot", &e),
    }
}

async fn delete_state(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
) -> Response {
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.delete_state(user.id, &keys.states, slot).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("deleting a save state", &e),
    }
}

/// Records the BIOS a game is started with: it is preselected next time.
async fn put_bios(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path(id): Path<String>,
    body: String,
) -> Response {
    let bios = body.trim();
    {
        let lib = state.library.read().await;
        let Some(b) = lib.bios(bios) else {
            return not_found("BIOS");
        };
        if lib.game(&id).is_some_and(|g| g.console != b.console) {
            return (StatusCode::BAD_REQUEST, "a BIOS of another console").into_response();
        }
    }
    let Some(keys) = save_keys(&state, &id).await else {
        return not_found("game");
    };
    match state.db.set_bios(user.id, &keys.cards, bios).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("saving the BIOS of a game", &e),
    }
}

/// Rescans the library and answers once it is done (on network storage, a
/// scan takes seconds), so that the page shows the new list; the DATs are
/// refreshed in the background after.
async fn rescan(State(state): AppStateRef, _: Admin) -> StatusCode {
    state.rescan().await;
    if state.config.fetch_metadata {
        let s = state.clone();
        tokio::spawn(async move { s.refresh_dats(false).await });
    }
    StatusCode::NO_CONTENT
}

// --- Memory cards, save by save ------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CardGame {
    id: String,
    title: String,
    console: Console,
    serial: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CardJson {
    /// The game the card belongs to (its first disc); none when it left the
    /// library.
    game: Option<CardGame>,
    key: String,
    slot: u8,
    console: Console,
    #[serde(flatten)]
    info: Option<crate::cards::CardInfo>,
    /// Why the card could not be read.
    error: Option<String>,
}

/// The games (first discs) by the key of their memory cards.
async fn games_by_card_key(state: &AppState) -> HashMap<String, CardGame> {
    let lib = state.library.read().await.clone();
    let groups = state.disc_groups().await;
    let mut out = HashMap::new();
    for g in &lib.games {
        let first = groups
            .get(&g.id)
            .and_then(|d| d.discs.first())
            .is_none_or(|d| d.id == g.id);
        if first {
            out.entry(save_key(g)).or_insert(CardGame {
                id: g.id.clone(),
                title: state.identify(g).await.title,
                console: g.console,
                serial: g.serial.clone(),
            });
        }
    }
    out
}

/// The user's memory cards, with their saves.
async fn list_cards(State(state): AppStateRef, CurrentUser(user, _): CurrentUser) -> Response {
    let cards = match state.db.memory_cards(user.id).await {
        Ok(c) => c,
        Err(e) => return db_error("listing memory cards", &e),
    };
    let games = games_by_card_key(&state).await;
    let mut out = Vec::with_capacity(cards.len());
    for (key, slot) in cards {
        let data = match state.db.memory_card(user.id, &key, slot).await {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => return db_error("reading a memory card", &e),
        };
        let console = if data.len() == MEMCARD_SIZE {
            Console::Ps1
        } else {
            Console::Ps2
        };
        let game = games.get(&key).map(|g| CardGame {
            id: g.id.clone(),
            title: g.title.clone(),
            console: g.console,
            serial: g.serial.clone(),
        });
        let (info, error) = match crate::cards::list(console, &data) {
            Ok(i) => (Some(i), None),
            Err(e) => (None, Some(e.to_string())),
        };
        out.push(CardJson {
            game,
            key,
            slot,
            console,
            info,
            error,
        });
    }
    out.sort_by(|a, b| {
        let title = |c: &CardJson| {
            c.game
                .as_ref()
                .map_or_else(|| c.key.to_lowercase(), |g| g.title.to_lowercase())
        };
        title(a).cmp(&title(b)).then(a.slot.cmp(&b.slot))
    });
    let mut resp = Json(out).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// A game's card: its key, its console, and the card (None: none yet).
async fn card_of(
    state: &AppState,
    user: u64,
    id: &str,
    slot: u8,
) -> Result<(String, Console, Option<Vec<u8>>), Box<Response>> {
    if !(1..=2).contains(&slot) {
        return Err(Box::new(not_found("memory card")));
    }
    let console = state.library.read().await.game(id).map(|g| g.console);
    let (Some(keys), Some(console)) = (save_keys(state, id).await, console) else {
        return Err(Box::new(not_found("game")));
    };
    match state.db.memory_card(user, &keys.cards, slot).await {
        Ok(card) => Ok((keys.cards, console, card)),
        Err(e) => Err(Box::new(db_error("reading a memory card", &e))),
    }
}

fn card_error(e: &crate::cards::CardError) -> Response {
    use crate::cards::CardError;
    let status = match e {
        CardError::NoSuchSave => StatusCode::NOT_FOUND,
        CardError::Exists | CardError::Full | CardError::Unformatted => StatusCode::CONFLICT,
        CardError::Corrupt(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, e.to_string()).into_response()
}

async fn delete_card(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot)): Path<(String, u8)>,
) -> Response {
    let (key, _, _) = match card_of(&state, user.id, &id, slot).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match state.db.delete_memory_card(user.id, &key, slot).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("deleting a memory card", &e),
    }
}

async fn delete_save(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot, save)): Path<(String, u8, String)>,
) -> Response {
    let (key, console, card) = match card_of(&state, user.id, &id, slot).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let Some(mut card) = card else {
        return not_found("memory card");
    };
    if let Err(e) = crate::cards::delete(console, &mut card, &save) {
        return card_error(&e);
    }
    match state.db.put_memory_card(user.id, &key, slot, &card).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("saving a memory card", &e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyRequest {
    to_game: String,
    to_slot: u8,
    /// Delete it from its card once copied.
    #[serde(default)]
    r#move: bool,
}

/// Copies (or moves) a save to another game's card, of the same console:
/// created (formatted) when the game has none yet.
async fn copy_save(
    State(state): AppStateRef,
    CurrentUser(user, _): CurrentUser,
    Path((id, slot, save)): Path<(String, u8, String)>,
    Json(req): Json<CopyRequest>,
) -> Response {
    let (from_key, console, from) = match card_of(&state, user.id, &id, slot).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let Some(mut from) = from else {
        return not_found("memory card");
    };
    let (to_key, to_console, to) = match card_of(&state, user.id, &req.to_game, req.to_slot).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if to_console != console {
        return (StatusCode::BAD_REQUEST, "a card of another console").into_response();
    }
    if to_key == from_key && req.to_slot == slot {
        return (StatusCode::BAD_REQUEST, "the save is already on this card").into_response();
    }
    // A new card, or a PS2 card never formatted (nothing on it), is
    // formatted first.
    let mut to = match to {
        Some(c) if crate::cards::list(console, &c).is_ok_and(|i| i.formatted) => c,
        Some(c) if crate::cards::list(console, &c).is_err() => {
            return (StatusCode::UNPROCESSABLE_ENTITY, "the destination card is damaged").into_response();
        }
        _ => crate::cards::blank(console),
    };
    if let Err(e) = crate::cards::copy(console, &from, &save, &mut to) {
        return card_error(&e);
    }
    let written = if req.r#move {
        if let Err(e) = crate::cards::delete(console, &mut from, &save) {
            return card_error(&e);
        }
        state
            .db
            .put_memory_cards(user.id, [(&to_key, req.to_slot, &to), (&from_key, slot, &from)])
            .await
    } else {
        state.db.put_memory_card(user.id, &to_key, req.to_slot, &to).await
    };
    match written {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error("saving a memory card", &e),
    }
}

#[cfg(test)]
mod card_size_tests {
    #[test]
    fn ps2_card_size_matches() {
        assert_eq!(super::PS2_MEMCARD_SIZE, texel_ps2::memcard::SIZE);
    }
}

#[cfg(test)]
mod cache_tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;
    use tower_http::services::{ServeDir, ServeFile};

    #[tokio::test]
    async fn assets_are_kept_the_page_is_checked() {
        let dir = std::env::temp_dir().join(format!("texel-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html>").unwrap();
        std::fs::write(dir.join("assets/index-AbC123.js"), "1").unwrap();
        let app = axum::Router::new()
            .fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html"))))
            .layer(axum::middleware::from_fn(super::cache_control));
        let cache = |path: &'static str| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(Request::get(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                resp.headers()[header::CACHE_CONTROL].to_str().unwrap().to_owned()
            }
        };
        assert_eq!(
            cache("/assets/index-AbC123.js").await,
            "public, max-age=31536000, immutable"
        );
        // An asset of an older build: the page answers, and is not kept.
        assert_eq!(cache("/assets/index-Old999.js").await, "no-cache");
        assert_eq!(cache("/").await, "no-cache");
        assert_eq!(cache("/register").await, "no-cache");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
