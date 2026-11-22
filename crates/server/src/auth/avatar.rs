//! OpenID Connect users' pictures: fetched at sign-in from the `picture`
//! claim, kept in the database, served from Texel's origin (the page's
//! Content Security Policy allows no other, and the provider is not asked
//! on every page view).
//!
//! Users may be able to set that URL at the provider, so the fetch is
//! careful: https only (http on localhost), 5 s, three redirects (https
//! too), 2 MiB; and only a PNG, JPEG, WebP or GIF image is kept (by its
//! signature), served with its own type and `nosniff`, never as anything a
//! browser would run. A failure never blocks a sign-in: the user keeps
//! their initials.

use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

pub const MAX_SIZE: usize = 2 << 20;
const TIMEOUT: Duration = Duration::from_secs(5);
const REDIRECTS: usize = 3;

/// A picture: its bytes, its media type, and its version (a hash, for its
/// URL).
pub struct Picture {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub version: String,
}

fn allowed(url: &reqwest::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

pub fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("texel/", env!("CARGO_PKG_VERSION")))
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= REDIRECTS {
                attempt.error("too many redirects")
            } else if !allowed(attempt.url()) {
                attempt.error("a redirect to a URL that is not https")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context("building the avatar client")
}

/// The image's type, from its first bytes.
pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

pub async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<Picture> {
    let url = reqwest::Url::parse(url).context("not a URL")?;
    if !allowed(&url) {
        bail!("not an https URL");
    }
    let mut response = http.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|n| n > MAX_SIZE as u64) {
        bail!("larger than {MAX_SIZE} bytes");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_SIZE {
            bail!("larger than {MAX_SIZE} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }
    let media_type = media_type(&bytes).context("not a PNG, JPEG, WebP or GIF image")?;
    let version = hex::encode(&Sha256::digest(&bytes)[..6]);
    Ok(Picture {
        bytes,
        media_type,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_types_by_signature() {
        assert_eq!(media_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(media_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(media_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(media_type(b"GIF89a..."), Some("image/gif"));
        // SVG (scripts), HTML, anything else: refused.
        assert_eq!(media_type(b"<svg xmlns="), None);
        assert_eq!(media_type(b"<!doctype html>"), None);
    }

    /// A local server answering each path with a canned response.
    async fn serve() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let (head, body): (String, Vec<u8>) = match path.as_str() {
                        "/a.png" => ("200 OK".into(), b"\x89PNG\r\n\x1a\nimage".to_vec()),
                        "/a.svg" => ("200 OK".into(), b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()),
                        "/big.png" => ("200 OK".into(), vec![0x89; MAX_SIZE + 1]),
                        "/away" => ("302 Found\r\nLocation: http://example.com/a.png".into(), Vec::new()),
                        _ => ("404 Not Found".into(), Vec::new()),
                    };
                    let reply = format!(
                        "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        format!("http://localhost:{}", addr.port())
    }

    #[tokio::test]
    async fn fetches_images_and_nothing_else() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let base = serve().await;
        let http = client().unwrap();
        let p = fetch(&http, &format!("{base}/a.png")).await.unwrap();
        assert_eq!(p.media_type, "image/png");
        assert_eq!(p.version.len(), 12);
        assert!(
            fetch(&http, &format!("{base}/a.svg")).await.is_err(),
            "not an image we keep"
        );
        assert!(fetch(&http, &format!("{base}/big.png")).await.is_err(), "too large");
        assert!(
            fetch(&http, &format!("{base}/away")).await.is_err(),
            "a redirect to http"
        );
        assert!(fetch(&http, &format!("{base}/missing")).await.is_err());
    }

    #[test]
    fn only_https() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(allowed(&u("https://sso.example.com/a.png")));
        assert!(allowed(&u("http://localhost:9000/a.png")));
        assert!(!allowed(&u("http://sso.example.com/a.png")));
        assert!(!allowed(&u("file:///etc/passwd")));
        assert!(!allowed(&u("data:image/png;base64,AAAA")));
    }
}
