//! Accounts: who may see the library and keep saves.
//!
//! Users sign in with OpenID Connect (the server is the client: the browser
//! never holds a token; see `oidc`), or, invited by an administrator, with
//! an e-mail address and a password (Argon2id; see `password`). An
//! invitation is a token the administrator hands over, typed in the
//! registration form (or carried by its link); a reset token sets a new
//! password the same way. Only tokens' SHA-256 are stored.
//!
//! A session is a random token in an `HttpOnly`, `SameSite=Lax` cookie
//! (`Secure`, `__Host-` prefixed, behind https), its SHA-256 in the
//! database. Requests that change something, and WebSocket upgrades, must
//! come from the page itself (`same_origin`).
//!
//! Roles: administrators (OpenID Connect users the configuration names)
//! invite people and manage users; everyone else is a guest. Netplay rooms
//! are open to anyone with their code: it grants its room's BIOS and disc,
//! nothing else (`Viewer`).

mod avatar;
pub mod jwt;
pub mod oidc;
mod password;
mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::state::AppState;

/// Session lookups are cached this long: disc reads come by the hundred.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// Failed password sign-ins for an address before it is held back, and
/// for how long.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Invitation tokens: unambiguous characters (no 0/O, 1/I/L), in groups.
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const TOKEN_LEN: usize = 20;
const MAX_NAME: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Guest,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Guest => "guest",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: u64,
    pub name: String,
    pub role: Role,
    /// Invited users': what they sign in with.
    pub email: Option<String>,
    /// Signed in with OpenID Connect (their name comes from the provider).
    pub oidc: bool,
    /// Their picture's URL (versioned), when the provider gave one.
    pub avatar: Option<String>,
}

pub struct AuthConfig {
    /// The address users reach Texel at, e.g. "https://texel.example.com".
    pub public_url: Option<String>,
    pub oidc: Option<oidc::OidcConfig>,
    pub session_days: u64,
    pub invite_days: u64,
}

pub struct Auth {
    pub oidc: Option<oidc::Provider>,
    /// Fetches users' pictures (see `avatar`).
    avatars: reqwest::Client,
    public_url: Option<String>,
    secure: bool,
    pub session_days: u64,
    pub invite_days: u64,
    cache: Mutex<HashMap<[u8; 32], (User, Instant)>>,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Auth {
    pub fn new(config: AuthConfig, http: reqwest::Client) -> Self {
        let public_url = config.public_url.map(|u| u.trim_end_matches('/').to_owned());
        Self {
            secure: public_url.as_deref().is_some_and(|u| u.starts_with("https://")),
            public_url,
            oidc: config.oidc.map(|c| oidc::Provider::new(c, http)),
            avatars: avatar::client().expect("an HTTP client"),
            session_days: config.session_days.max(1),
            invite_days: config.invite_days.max(1),
            cache: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn session_cookie(&self) -> &'static str {
        if self.secure {
            "__Host-texel_session"
        } else {
            "texel_session"
        }
    }

    fn login_cookie(&self) -> &'static str {
        if self.secure {
            "__Host-texel_login"
        } else {
            "texel_login"
        }
    }

    /// A Set-Cookie value (`max_age` 0: deleting it).
    fn cookie(&self, name: &str, value: &str, path: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
        ))
        .expect("a valid cookie")
    }

    /// The signed-in user of a request, and their session's hash.
    pub async fn user(&self, db: &crate::db::Db, headers: &HeaderMap) -> Option<(User, [u8; 32])> {
        let token = cookie(headers, self.session_cookie())?;
        let hash = sha(token);
        if let Some((user, at)) = self.lock_cache().get(&hash)
            && at.elapsed() < CACHE_FOR
        {
            return Some((user.clone(), hash));
        }
        let user = match db.session_user(&hash).await {
            Ok(u) => u?,
            Err(e) => {
                tracing::warn!(error = %e, "reading a session failed");
                return None;
            }
        };
        if let Err(e) = db.touch_user(user.id).await {
            tracing::warn!(error = %e, "recording a visit failed");
        }
        let mut cache = self.lock_cache();
        cache.retain(|_, (_, at)| at.elapsed() < CACHE_FOR);
        cache.insert(hash, (user.clone(), Instant::now()));
        Some((user, hash))
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], (User, Instant)>> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forgets cached sessions (a user deleted, signed out, renamed).
    pub fn forget_sessions(&self) {
        self.lock_cache().clear();
    }

    /// Starts a session: the Set-Cookie header.
    async fn start_session(
        &self,
        db: &crate::db::Db,
        user: u64,
        id_token: Option<&str>,
    ) -> anyhow::Result<HeaderValue> {
        let token = random_token();
        db.create_session(&sha(&token), user, id_token, self.session_days)
            .await?;
        Ok(self.cookie(self.session_cookie(), &token, "/", self.session_days * 86_400))
    }

    /// Whether an address is held back for too many failed sign-ins.
    fn held_back(&self, email: &str) -> bool {
        let mut f = self.failures.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.retain(|_, (_, at)| at.elapsed() < FAILURE_WINDOW);
        f.get(email).is_some_and(|(n, _)| *n >= MAX_FAILURES)
    }

    fn failed(&self, email: &str) {
        let mut f = self.failures.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = f.entry(email.to_owned()).or_insert((0, Instant::now()));
        e.0 += 1;
    }

    /// The origin pages are served from, when configured.
    fn public_origin(&self) -> Option<&str> {
        let url = self.public_url.as_deref()?;
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let host_end = rest.find('/').map_or(url.len(), |i| url.len() - rest.len() + i);
        Some(&url[..host_end])
    }

    /// The redirect URI registered at the provider.
    fn redirect_uri(&self, headers: &HeaderMap) -> String {
        format!("{}/auth/oidc/callback", self.base_url(headers))
    }

    /// Where Texel is: the public URL, else what the request says.
    fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(u) = &self.public_url {
            return u.clone();
        }
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!("http://{host}")
    }
}

/// A cookie's value.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

fn sha(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system's randomness");
    b
}

/// 32 random bytes, base64url: sessions, PKCE verifiers, states, nonces.
fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// An invitation token, as handed over: "ABCDE-FGHJK-MNPQR-STUVW" (about
/// 99 bits).
fn invite_token() -> String {
    let bytes = random_bytes::<TOKEN_LEN>();
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| char::from(TOKEN_ALPHABET[usize::from(*b) % TOKEN_ALPHABET.len()]))
        .collect();
    chars
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// A token as typed: dashes, spaces and case do not matter.
fn normalise_token(t: &str) -> String {
    t.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn token_hash(t: &str) -> [u8; 32] {
    sha(&normalise_token(t))
}

fn clean_name(name: &str) -> Option<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn clean_email(email: &str) -> Option<String> {
    let e = email.trim().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && e.len() <= 254
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then_some(e)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn internal(what: &str, e: &anyhow::Error) -> Response {
    tracing::error!(error = %e, "{what} failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, format!("{what} failed"))
}

// --- Extractors ---------------------------------------------------------------------

/// The signed-in user (401 otherwise), and their session's hash.
pub struct CurrentUser(pub User, pub [u8; 32]);

impl FromRequestParts<Arc<AppState>> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        match state.auth.user(&state.db, &parts.headers).await {
            Some((u, h)) => Ok(Self(u, h)),
            None => Err(error(StatusCode::UNAUTHORIZED, "sign in first")),
        }
    }
}

/// An administrator (403 for others).
pub struct Admin(pub User);

impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let CurrentUser(user, _) = CurrentUser::from_request_parts(parts, state).await?;
        if user.role == Role::Admin {
            Ok(Self(user))
        } else {
            Err(error(StatusCode::FORBIDDEN, "for administrators"))
        }
    }
}

/// Who asks for a BIOS or a disc: a user, or a player in a netplay room
/// (its code, from the cookie `rooms/{code}` sets).
pub struct Viewer {
    pub user: Option<User>,
    room: Option<String>,
}

pub const ROOM_COOKIE: &str = "texel_room";

impl Viewer {
    /// Whether they may read this game, or this BIOS.
    pub fn may_read(&self, state: &AppState, game: Option<&str>, bios: Option<&str>) -> bool {
        self.user.is_some()
            || self
                .room
                .as_deref()
                .is_some_and(|code| state.rooms.grants(code, game, bios))
    }
}

impl FromRequestParts<Arc<AppState>> for Viewer {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let user = state.auth.user(&state.db, &parts.headers).await.map(|(u, _)| u);
        let room = cookie(&parts.headers, ROOM_COOKIE).map(str::to_owned);
        Ok(Self { user, room })
    }
}

impl Auth {
    /// The cookie granting a room's files to whoever looked it up.
    pub fn room_cookie(&self, code: &str) -> HeaderValue {
        self.cookie(ROOM_COOKIE, code, "/api", 86_400)
    }
}

// --- Cross-site requests --------------------------------------------------------------------

/// Requests that change something, and WebSocket upgrades, must come from
/// Texel's own pages: browsers say where a request comes from
/// (`Sec-Fetch-Site`, `Origin`). Clients that are not browsers send
/// neither, and are not at risk.
pub async fn same_origin(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let upgrade = headers.contains_key(header::UPGRADE);
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) && !upgrade;
    if safe || allowed_origin(&state.auth, headers) {
        next.run(req).await
    } else {
        error(StatusCode::FORBIDDEN, "a request from another site")
    }
}

fn allowed_origin(auth: &Auth, headers: &HeaderMap) -> bool {
    let text = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
    if let Some(site) = text("sec-fetch-site") {
        // A WebSocket opened by the page is same-origin too.
        return site == "same-origin" || site == "none";
    }
    let Some(origin) = text("origin") else {
        return true;
    };
    match auth.public_origin() {
        Some(public) => origin.eq_ignore_ascii_case(public),
        None => {
            let host = text("host").unwrap_or_default();
            origin
                .split_once("://")
                .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
        }
    }
}

// --- Routes -----------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigJson {
    /// The provider's name, when OpenID Connect is configured.
    oidc: Option<String>,
}

/// What the sign-in page offers.
pub async fn config(State(state): State<Arc<AppState>>) -> Json<ConfigJson> {
    Json(ConfigJson {
        oidc: state.auth.oidc.as_ref().map(|p| p.config.name.clone()),
    })
}

pub async fn me(CurrentUser(user, _): CurrentUser) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeUpdate {
    name: Option<String>,
    password: Option<String>,
    current_password: Option<String>,
}

/// Invited users change their name or password (the provider's users have
/// theirs from the provider).
pub async fn update_me(
    State(state): State<Arc<AppState>>,
    CurrentUser(user, session): CurrentUser,
    Json(update): Json<MeUpdate>,
) -> Response {
    if user.oidc {
        return error(
            StatusCode::BAD_REQUEST,
            "your account is managed by your sign-in provider",
        );
    }
    if let Some(name) = &update.name {
        let Some(name) = clean_name(name) else {
            return error(StatusCode::BAD_REQUEST, "a name is needed");
        };
        if let Err(e) = state.db.set_name(user.id, &name).await {
            return internal("renaming", &e);
        }
    }
    if let Some(new) = update.password {
        if let Some(why) = password::weak(&new) {
            return error(StatusCode::BAD_REQUEST, why);
        }
        let current = state.db.password_hash(user.id).await.ok().flatten();
        if !password::verify(update.current_password.unwrap_or_default(), current).await {
            return error(StatusCode::FORBIDDEN, "the current password is wrong");
        }
        let hash = match password::hash(new).await {
            Ok(h) => h,
            Err(e) => return internal("hashing a password", &e),
        };
        if let Err(e) = state.db.set_password(user.id, &hash, &session).await {
            return internal("changing a password", &e);
        }
    }
    state.auth.forget_sessions();
    match state.db.user(user.id).await {
        Ok(Some(u)) => Json(u).into_response(),
        Ok(None) => error(StatusCode::UNAUTHORIZED, "sign in first"),
        Err(e) => internal("reading a user", &e),
    }
}

fn signed_in(cookie: HeaderValue, user: &User) -> Response {
    let mut resp = Json(user).into_response();
    resp.headers_mut().append(header::SET_COOKIE, cookie);
    resp
}

#[derive(Deserialize)]
pub struct Credentials {
    email: String,
    password: String,
}

pub async fn password_login(State(state): State<Arc<AppState>>, Json(c): Json<Credentials>) -> Response {
    let email = c.email.trim().to_lowercase();
    if state.auth.held_back(&email) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts: try again in a quarter of an hour",
        );
    }
    let found = match state.db.password_user(&email).await {
        Ok(f) => f,
        Err(e) => return internal("signing in", &e),
    };
    let (user, hash) = match found {
        Some((u, h)) => (Some(u), Some(h)),
        None => (None, None),
    };
    if !password::verify(c.password, hash).await {
        state.auth.failed(&email);
        return error(StatusCode::UNAUTHORIZED, "wrong e-mail address or password");
    }
    let user = user.expect("verified: a user");
    match state.auth.start_session(&state.db, user.id, None).await {
        Ok(cookie) => signed_in(cookie, &user),
        Err(e) => internal("signing in", &e),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteJson {
    kind: String,
    note: String,
    /// A reset's account.
    email: Option<String>,
}

/// What a token is for, before the form is sent.
pub async fn invite(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    match state.db.invite(&token_hash(&token)).await {
        Ok(Some(i)) => Json(InviteJson {
            kind: i.kind,
            note: i.note,
            email: i.user.map(|(_, e)| e),
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "this token is unknown, used or expired"),
        Err(e) => internal("reading an invitation", &e),
    }
}

#[derive(Deserialize)]
pub struct Registration {
    token: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
    password: String,
}

/// Registers with an invitation, or sets a new password with a reset.
pub async fn register(State(state): State<Arc<AppState>>, Json(r): Json<Registration>) -> Response {
    let invite = match state.db.invite(&token_hash(&r.token)).await {
        Ok(Some(i)) => i,
        Ok(None) => return error(StatusCode::NOT_FOUND, "this token is unknown, used or expired"),
        Err(e) => return internal("reading an invitation", &e),
    };
    if let Some(why) = password::weak(&r.password) {
        return error(StatusCode::BAD_REQUEST, why);
    }
    let user = if let Some((user_id, _)) = invite.user {
        let hash = match password::hash(r.password).await {
            Ok(h) => h,
            Err(e) => return internal("hashing a password", &e),
        };
        state.auth.forget_sessions();
        state.db.reset_password(invite.id, user_id, &hash).await
    } else {
        let Some(email) = clean_email(&r.email) else {
            return error(StatusCode::BAD_REQUEST, "an e-mail address is needed");
        };
        let Some(name) = clean_name(&r.name) else {
            return error(StatusCode::BAD_REQUEST, "a name is needed");
        };
        match state.db.email_taken(&email).await {
            Ok(true) => return error(StatusCode::CONFLICT, "this e-mail address already has an account"),
            Ok(false) => {}
            Err(e) => return internal("registering", &e),
        }
        let hash = match password::hash(r.password).await {
            Ok(h) => h,
            Err(e) => return internal("hashing a password", &e),
        };
        state.db.register(invite.id, &email, &name, &hash).await
    };
    let user = match user {
        Ok(Some(u)) => u,
        Ok(None) => return error(StatusCode::CONFLICT, "this token was just used, or the address taken"),
        Err(e) => return internal("registering", &e),
    };
    match state.auth.start_session(&state.db, user.id, None).await {
        Ok(cookie) => signed_in(cookie, &user),
        Err(e) => internal("signing in", &e),
    }
}

/// Signs out; with OpenID Connect, says where to sign out at the provider.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let mut redirect = None;
    if let Some(token) = cookie(&headers, state.auth.session_cookie()) {
        match state.db.delete_session(&sha(token)).await {
            Ok(Some(id_token)) => {
                if let Some(p) = &state.auth.oidc {
                    redirect = p
                        .logout_url(&id_token, &format!("{}/", state.auth.base_url(&headers)))
                        .await;
                }
            }
            Ok(None) => {}
            Err(e) => return internal("signing out", &e),
        }
        state.auth.forget_sessions();
    }
    let mut resp = Json(json!({ "redirect": redirect })).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        state.auth.cookie(state.auth.session_cookie(), "", "/", 0),
    );
    resp
}

#[derive(Deserialize)]
pub struct LoginQuery {
    /// The page to come back to: a hash route ("#/games/ps2").
    #[serde(default, rename = "return")]
    return_to: String,
}

/// Starts an OpenID Connect sign-in.
pub async fn oidc_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    let Some(provider) = &state.auth.oidc else {
        return error(StatusCode::NOT_FOUND, "OpenID Connect is not configured");
    };
    let return_to = if q.return_to.starts_with("#/") && q.return_to.len() <= 500 {
        q.return_to
    } else {
        String::new()
    };
    let (login_state, verifier, nonce, browser) = (random_token(), random_token(), random_token(), random_token());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = match provider
        .authorize_url(&state.auth.redirect_uri(&headers), &login_state, &nonce, &challenge)
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "starting an OpenID Connect sign-in failed");
            return sign_in_error("the sign-in provider cannot be reached");
        }
    };
    if let Err(e) = state
        .db
        .put_oidc_login(&login_state, &verifier, &nonce, &sha(&browser), &return_to)
        .await
    {
        return internal("starting a sign-in", &e);
    }
    let mut resp = Redirect::to(&url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        state.auth.cookie(state.auth.login_cookie(), &browser, "/", 600),
    );
    resp
}

/// Fetches the user's picture when its URL changed (or drops it, when the
/// provider no longer gives one). Failures are logged: the user keeps the
/// picture they had, or their initials.
async fn update_picture(state: &AppState, user: u64, url: Option<&str>) {
    let known = match state.db.avatar_url(user).await {
        Ok(u) => u,
        Err(e) => return tracing::warn!(error = %e, "reading a user's picture URL failed"),
    };
    let result = match url {
        None if known.is_some() => state.db.set_avatar(user, None).await,
        None => Ok(()),
        Some(url) if known.as_deref() == Some(url) => Ok(()),
        Some(url) => match avatar::fetch(&state.auth.avatars, url).await {
            Ok(p) => state.db.set_avatar(user, Some((url, &p))).await,
            Err(e) => {
                tracing::warn!(user, error = %e, "fetching a user's picture failed");
                Ok(())
            }
        },
    };
    match result {
        Ok(()) => state.auth.forget_sessions(),
        Err(e) => tracing::warn!(error = %e, "saving a user's picture failed"),
    }
}

/// A user's picture, for signed-in users. Its URL carries its version: kept
/// by browsers until it changes.
pub async fn user_avatar(State(state): State<Arc<AppState>>, _: CurrentUser, Path(id): Path<u64>) -> Response {
    match state.db.avatar(id).await {
        Ok(Some((bytes, media_type))) => {
            let Ok(media_type) = HeaderValue::from_str(&media_type) else {
                return error(StatusCode::NOT_FOUND, "no picture");
            };
            (
                [
                    (header::CONTENT_TYPE, media_type),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, max-age=31536000, immutable"),
                    ),
                    // Never a document, whatever is in it.
                    (
                        header::CONTENT_SECURITY_POLICY,
                        HeaderValue::from_static("sandbox; default-src 'none'"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no picture"),
        Err(e) => internal("reading a picture", &e),
    }
}

/// Back to the sign-in page, saying what went wrong.
fn sign_in_error(message: &str) -> Response {
    let encoded: String = message
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Redirect::to(&format!("/#/signin/{encoded}")).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The provider sends the browser back here.
pub async fn oidc_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(provider) = &state.auth.oidc else {
        return error(StatusCode::NOT_FOUND, "OpenID Connect is not configured");
    };
    if let Some(e) = q.error {
        let why = q.error_description.unwrap_or(e);
        return sign_in_error(&format!(
            "the provider refused: {}",
            why.chars().take(200).collect::<String>()
        ));
    }
    let (Some(code), Some(login_state)) = (q.code, q.state) else {
        return sign_in_error("the provider sent nothing back");
    };
    let login = match state.db.take_oidc_login(&login_state).await {
        Ok(Some(l)) => l,
        Ok(None) => return sign_in_error("this sign-in expired: try again"),
        Err(e) => return internal("signing in", &e),
    };
    let (verifier, nonce, browser_hash, return_to) = login;
    // The sign-in must finish in the browser that started it.
    let browser = cookie(&headers, state.auth.login_cookie()).map(sha);
    if browser.is_none_or(|b| b[..] != browser_hash[..]) {
        return sign_in_error("this sign-in was started elsewhere: try again");
    }
    let (identity, id_token) = match provider
        .complete(&code, &verifier, &state.auth.redirect_uri(&headers), &nonce)
        .await
    {
        Ok(r) => r,
        Err(e) if e.is::<oidc::Refused>() => return sign_in_error(&e.to_string()),
        Err(e) => {
            tracing::warn!(error = %e, "an OpenID Connect sign-in failed");
            return sign_in_error("the sign-in failed");
        }
    };
    let role = if identity.admin { Role::Admin } else { Role::Guest };
    let user = match state
        .db
        .upsert_oidc_user(&provider.config.issuer, &identity.subject, &identity.name, role)
        .await
    {
        Ok(u) => u,
        Err(e) => return internal("signing in", &e),
    };
    tracing::info!(user = user.id, admin = identity.admin, "signed in with OpenID Connect");
    update_picture(&state, user.id, identity.picture.as_deref()).await;
    let session = match state.auth.start_session(&state.db, user.id, Some(&id_token)).await {
        Ok(c) => c,
        Err(e) => return internal("signing in", &e),
    };
    let mut resp = Redirect::to(&format!("/{return_to}")).into_response();
    let h = resp.headers_mut();
    h.append(header::SET_COOKIE, session);
    h.append(
        header::SET_COOKIE,
        state.auth.cookie(state.auth.login_cookie(), "", "/", 0),
    );
    resp
}

// --- Administration ------------------------------------------------------------------------

pub async fn list_users(State(state): State<Arc<AppState>>, _: Admin) -> Response {
    match state.db.users().await {
        Ok(u) => Json(u).into_response(),
        Err(e) => internal("listing users", &e),
    }
}

pub async fn delete_user(State(state): State<Arc<AppState>>, Admin(admin): Admin, Path(id): Path<u64>) -> Response {
    if id == admin.id {
        return error(StatusCode::BAD_REQUEST, "you cannot delete yourself");
    }
    match state.db.delete_user(id).await {
        Ok(true) => {
            state.auth.forget_sessions();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => internal("deleting a user", &e),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewToken {
    id: u64,
    /// Shown once: only its hash is kept.
    token: String,
}

/// A reset token for an invited user who forgot their password.
pub async fn reset_user(State(state): State<Arc<AppState>>, Admin(admin): Admin, Path(id): Path<u64>) -> Response {
    let user = match state.db.user(id).await {
        Ok(Some(u)) => u,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => return internal("reading a user", &e),
    };
    if user.oidc {
        return error(
            StatusCode::BAD_REQUEST,
            "this user signs in with the provider: no password here",
        );
    }
    new_token(&state, "reset", Some(id), &user.name, admin.id).await
}

#[derive(Deserialize)]
pub struct NewInvite {
    #[serde(default)]
    note: String,
}

pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    Admin(admin): Admin,
    Json(n): Json<NewInvite>,
) -> Response {
    let note: String = n.note.chars().filter(|c| !c.is_control()).take(100).collect();
    new_token(&state, "invite", None, note.trim(), admin.id).await
}

async fn new_token(state: &AppState, kind: &str, user: Option<u64>, note: &str, by: u64) -> Response {
    let token = invite_token();
    match state
        .db
        .create_invite(&token_hash(&token), kind, user, note, Some(by), state.auth.invite_days)
        .await
    {
        Ok(id) => Json(NewToken { id, token }).into_response(),
        Err(e) => internal("creating a token", &e),
    }
}

/// From the command line (`--create-invite`): an invitation without an
/// administrator, for a server without OpenID Connect.
pub async fn create_invite_cli(db: &crate::db::Db, note: &str, days: u64) -> anyhow::Result<String> {
    let token = invite_token();
    db.create_invite(&token_hash(&token), "invite", None, note, None, days.max(1))
        .await?;
    Ok(token)
}

pub async fn list_invites(State(state): State<Arc<AppState>>, _: Admin) -> Response {
    match state.db.invites().await {
        Ok(i) => Json(i).into_response(),
        Err(e) => internal("listing invitations", &e),
    }
}

pub async fn delete_invite(State(state): State<Arc<AppState>>, _: Admin, Path(id): Path<u64>) -> Response {
    match state.db.delete_invite(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such invitation"),
        Err(e) => internal("deleting an invitation", &e),
    }
}

/// An HTTP client for tests (with the crypto provider `main` installs).
#[cfg(test)]
pub fn test_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn auth(public_url: Option<&str>) -> Auth {
        Auth::new(
            AuthConfig {
                public_url: public_url.map(str::to_owned),
                oidc: None,
                session_days: 30,
                invite_days: 7,
            },
            test_client(),
        )
    }

    #[test]
    fn cookies() {
        let h = headers(&[
            ("cookie", "a=1; texel_session=tok; b=2"),
            ("cookie", "texel_room=ABCDEF"),
        ]);
        assert_eq!(cookie(&h, "texel_session"), Some("tok"));
        assert_eq!(cookie(&h, "texel_room"), Some("ABCDEF"));
        assert_eq!(cookie(&h, "missing"), None);
        let secure = auth(Some("https://texel.example.com/"));
        assert_eq!(secure.session_cookie(), "__Host-texel_session");
        let set = secure.cookie("x", "v", "/", 60);
        assert!(
            set.to_str()
                .unwrap()
                .contains("HttpOnly; SameSite=Lax; Max-Age=60; Secure")
        );
        assert_eq!(auth(Some("http://192.0.2.10:8080")).session_cookie(), "texel_session");
    }

    #[test]
    fn origins() {
        let a = auth(Some("https://texel.example.com"));
        assert_eq!(a.public_origin(), Some("https://texel.example.com"));
        assert!(allowed_origin(&a, &headers(&[("sec-fetch-site", "same-origin")])));
        assert!(!allowed_origin(&a, &headers(&[("sec-fetch-site", "cross-site")])));
        assert!(allowed_origin(&a, &headers(&[("origin", "https://texel.example.com")])));
        assert!(!allowed_origin(&a, &headers(&[("origin", "https://evil.example")])));
        // Not a browser.
        assert!(allowed_origin(&a, &HeaderMap::new()));
        let b = auth(None);
        assert!(allowed_origin(
            &b,
            &headers(&[("origin", "http://texel.lan:8080"), ("host", "texel.lan:8080")])
        ));
        assert!(!allowed_origin(
            &b,
            &headers(&[("origin", "http://evil:8080"), ("host", "texel.lan:8080")])
        ));
    }

    #[test]
    fn tokens() {
        let t = invite_token();
        assert_eq!(t.len(), 23);
        assert_eq!(t.matches('-').count(), 3);
        // Typed loosely, it is the same token.
        assert_eq!(token_hash(&t.to_lowercase().replace('-', " ")), token_hash(&t));
        assert_ne!(invite_token(), t);
        assert_eq!(random_token().len(), 43);
    }

    #[test]
    fn names_and_addresses() {
        assert_eq!(clean_name("  Ann\u{7} "), Some("Ann".into()));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_email(" Ann@Example.COM "), Some("ann@example.com".into()));
        assert_eq!(clean_email("ann"), None);
        assert_eq!(clean_email("ann@localhost"), None);
        assert_eq!(clean_email("a n@example.com"), None);
    }
}
