//! The OpenID Connect provider: its discovery document and keys (cached,
//! refreshed hourly and on an unknown key id), the authorization request,
//! the code exchange, and the claims of the signed-in user.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use super::jwt::{self, Jwks};

const REFRESH: Duration = Duration::from_secs(3600);
/// At most this often on an unknown key id.
const KEY_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// None for a public client (PKCE only).
    pub client_secret: Option<String>,
    pub scopes: String,
    pub groups_claim: String,
    /// Who may sign in: members of any of these groups (empty: anyone the
    /// provider accepts).
    pub user_groups: Vec<String>,
    /// Administrators: members of these groups, or these users (by subject,
    /// e-mail address or user name).
    pub admin_groups: Vec<String>,
    pub admin_users: Vec<String>,
    /// The provider's name, on the sign-in button.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    jwks_uri: String,
    #[serde(default)]
    end_session_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

struct Discovered {
    meta: Metadata,
    jwks: Jwks,
    at: Instant,
    keys_at: Instant,
}

pub struct Provider {
    pub config: OidcConfig,
    http: reqwest::Client,
    cache: Mutex<Option<Discovered>>,
}

/// Who signed in.
pub struct Identity {
    pub subject: String,
    pub name: String,
    pub admin: bool,
    /// The `picture` claim: a URL of the user's picture.
    pub picture: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    #[serde(default)]
    access_token: Option<String>,
}

impl Provider {
    pub fn new(config: OidcConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            cache: Mutex::new(None),
        }
    }

    async fn metadata(&self) -> anyhow::Result<Metadata> {
        let mut cache = self.cache.lock().await;
        if let Some(d) = cache.as_ref().filter(|d| d.at.elapsed() < REFRESH) {
            return Ok(d.meta.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        );
        let meta: Metadata = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("fetching the provider's discovery document")?
            .json()
            .await
            .context("reading the provider's discovery document")?;
        if meta.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            bail!(
                "the provider says its issuer is {}, not {}",
                meta.issuer,
                self.config.issuer
            );
        }
        let jwks = self.fetch_keys(&meta).await?;
        let now = Instant::now();
        *cache = Some(Discovered {
            meta: meta.clone(),
            jwks,
            at: now,
            keys_at: now,
        });
        Ok(meta)
    }

    async fn fetch_keys(&self, meta: &Metadata) -> anyhow::Result<Jwks> {
        self.http
            .get(&meta.jwks_uri)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("fetching the provider's keys")?
            .json()
            .await
            .context("reading the provider's keys")
    }

    /// The provider's keys, fetched again when `kid` is not among them.
    async fn keys(&self, kid: Option<&str>) -> anyhow::Result<Jwks> {
        let meta = self.metadata().await?;
        let mut cache = self.cache.lock().await;
        let d = cache.as_mut().context("no discovery document")?;
        let known = kid.is_none_or(|k| d.jwks.keys.iter().any(|j| j.kid.as_deref() == Some(k)));
        if !known && d.keys_at.elapsed() >= KEY_RETRY {
            d.jwks = self.fetch_keys(&meta).await?;
            d.keys_at = Instant::now();
        }
        Ok(d.jwks.clone())
    }

    /// Where to send the browser to sign in.
    pub async fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> anyhow::Result<String> {
        let meta = self.metadata().await?;
        let mut url = reqwest::Url::parse(&meta.authorization_endpoint).context("the authorization endpoint")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.config.scopes)
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Exchanges the code, checks the ID token, and says who signed in
    /// (and with which ID token, for signing out).
    pub async fn complete(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        nonce: &str,
    ) -> anyhow::Result<(Identity, String)> {
        let meta = self.metadata().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let mut request = self.http.post(&meta.token_endpoint);
        match &self.config.client_secret {
            // client_secret_basic, unless the provider takes the secret in
            // the form only.
            Some(secret)
                if meta
                    .token_endpoint_auth_methods_supported
                    .iter()
                    .any(|m| m == "client_secret_post")
                    && !meta
                        .token_endpoint_auth_methods_supported
                        .iter()
                        .any(|m| m == "client_secret_basic") =>
            {
                form.push(("client_id", &self.config.client_id));
                form.push(("client_secret", secret));
            }
            Some(secret) => {
                request = request.basic_auth(form_encode(&self.config.client_id), Some(form_encode(secret)));
            }
            None => form.push(("client_id", &self.config.client_id)),
        }
        let response = request.form(&form).send().await.context("exchanging the code")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "the provider refused the code: {status} {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let tokens: TokenResponse = response.json().await.context("reading the provider's tokens")?;
        let kid = jwt::key_id(&tokens.id_token);
        let jwks = self.keys(kid.as_deref()).await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut claims = jwt::verify(
            &tokens.id_token,
            &jwks,
            &jwt::Expected {
                issuer: &meta.issuer,
                client_id: &self.config.client_id,
                nonce,
                now,
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        // Userinfo completes the claims (many providers give groups there
        // only); the ID token's subject must match.
        if let (Some(url), Some(access)) = (&meta.userinfo_endpoint, &tokens.access_token) {
            match self.userinfo(url, access).await {
                Ok(info) if info.get("sub") == claims.get("sub") => claims.extend(info),
                Ok(_) => bail!("the provider's userinfo is about someone else"),
                Err(e) => tracing::warn!(error = %e, "reading the provider's userinfo failed"),
            }
        }
        let identity = self.identity(&claims)?;
        Ok((identity, tokens.id_token))
    }

    async fn userinfo(&self, url: &str, access: &str) -> anyhow::Result<Map<String, Value>> {
        Ok(self
            .http
            .get(url)
            .bearer_auth(access)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    fn identity(&self, claims: &Map<String, Value>) -> anyhow::Result<Identity> {
        let text = |k: &str| claims.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let subject = text("sub").context("no subject")?.to_owned();
        let groups: Vec<&str> = match claims.get(&self.config.groups_claim) {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => s.split(',').map(str::trim).collect(),
            _ => Vec::new(),
        };
        let in_any = |wanted: &[String]| wanted.iter().any(|w| groups.contains(&w.as_str()));
        let names = [Some(subject.as_str()), text("email"), text("preferred_username")];
        let admin = in_any(&self.config.admin_groups)
            || self
                .config
                .admin_users
                .iter()
                .any(|u| names.iter().flatten().any(|n| n.eq_ignore_ascii_case(u)));
        if !admin && !self.config.user_groups.is_empty() && !in_any(&self.config.user_groups) {
            bail!(Refused);
        }
        let name = text("name")
            .or(text("preferred_username"))
            .or(text("email"))
            .unwrap_or(&subject)
            .chars()
            .take(64)
            .collect();
        let picture = text("picture").map(str::to_owned);
        Ok(Identity {
            subject,
            name,
            admin,
            picture,
        })
    }

    /// Where to send the browser to sign out at the provider too, if it
    /// says.
    pub async fn logout_url(&self, id_token: &str, back: &str) -> Option<String> {
        let meta = self.metadata().await.ok()?;
        let mut url = reqwest::Url::parse(meta.end_session_endpoint.as_deref()?).ok()?;
        url.query_pairs_mut()
            .append_pair("id_token_hint", id_token)
            .append_pair("client_id", &self.config.client_id)
            .append_pair("post_logout_redirect_uri", back);
        Some(url.into())
    }
}

/// The user is not in the groups allowed to sign in.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("your account is not allowed to use Texel")
    }
}

impl std::error::Error for Refused {}

/// application/x-www-form-urlencoded, as client credentials go in the Basic
/// scheme (RFC 6749, 2.3.1).
fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => char::from(b).to_string(),
            b' ' => "+".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn provider(user_groups: &[&str]) -> Provider {
        Provider::new(
            OidcConfig {
                issuer: "https://sso.example.com".into(),
                client_id: "texel".into(),
                client_secret: None,
                scopes: "openid".into(),
                groups_claim: "groups".into(),
                user_groups: user_groups.iter().map(|s| (*s).to_owned()).collect(),
                admin_groups: vec!["texel-admins".into()],
                admin_users: vec!["boss@example.com".into()],
                name: "SSO".into(),
            },
            crate::auth::test_client(),
        )
    }

    fn claims(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn roles_from_groups_and_users() {
        let p = provider(&[]);
        let i = p
            .identity(&claims(
                json!({ "sub": "1", "name": "Ann", "groups": ["texel-admins"] }),
            ))
            .unwrap();
        assert!(i.admin);
        assert_eq!(i.name, "Ann");
        let i = p
            .identity(&claims(json!({ "sub": "2", "email": "Boss@example.com" })))
            .unwrap();
        assert!(i.admin);
        assert_eq!(i.name, "Boss@example.com");
        let i = p
            .identity(&claims(json!({ "sub": "3", "preferred_username": "bob" })))
            .unwrap();
        assert!(!i.admin);
        assert_eq!(i.name, "bob");
    }

    #[test]
    fn who_may_sign_in() {
        let p = provider(&["texel"]);
        assert!(
            p.identity(&claims(json!({ "sub": "1", "groups": "texel, other" })))
                .is_ok()
        );
        assert!(p.identity(&claims(json!({ "sub": "2", "groups": ["other"] }))).is_err());
        // Administrators always may.
        assert!(
            p.identity(&claims(json!({ "sub": "3", "groups": ["texel-admins"] })))
                .is_ok()
        );
    }

    #[test]
    fn basic_credentials_are_form_encoded() {
        assert_eq!(form_encode("a b+c/é"), "a+b%2Bc%2F%C3%A9");
    }
}
