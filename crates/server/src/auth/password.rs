//! Passwords of invited users: Argon2id (its default parameters: 19 MiB,
//! 2 passes), each with its own random salt, kept as a PHC string
//! ("$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>"). Hashing takes tens of
//! milliseconds: it runs on a blocking thread.

use argon2::{Argon2, PasswordHasher, PasswordVerifier};

pub const MIN_LEN: usize = 10;
pub const MAX_LEN: usize = 200;

pub async fn hash(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        Ok(Argon2::default()
            .hash_password_with_salt(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hashing a password: {e}"))?
            .to_string())
    })
    .await?
}

/// Whether `password` matches `hash`. With no hash (no such user), a dummy
/// one is checked, so that the answer takes as long.
pub async fn verify(password: String, hash: Option<String>) -> bool {
    const DUMMY: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$1oWE1UhS1N/hPdUKOyVGPCs/u2gyZ8OP0d8HIu7gFNE";
    tokio::task::spawn_blocking(move || {
        let known = hash.is_some();
        let hash = hash.unwrap_or_else(|| DUMMY.to_owned());
        // An unparsable hash fails to verify.
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok();
        known && ok
    })
    .await
    .unwrap_or(false)
}

/// Why a new password is refused, if it is.
pub fn weak(password: &str) -> Option<String> {
    let n = password.chars().count();
    if n < MIN_LEN {
        Some(format!("a password has at least {MIN_LEN} characters"))
    } else if n > MAX_LEN {
        Some(format!("a password has at most {MAX_LEN} characters"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_and_verifies() {
        let h = hash("correct horse battery".into()).await.unwrap();
        assert!(h.starts_with("$argon2id$v=19$"));
        // Salted: the same password hashes differently.
        assert_ne!(h, hash("correct horse battery".into()).await.unwrap());
        assert!(verify("correct horse battery".into(), Some(h.clone())).await);
        assert!(!verify("wrong horse battery".into(), Some(h)).await);
        assert!(!verify("anything".into(), None).await);
    }

    #[test]
    fn length_rules() {
        assert!(weak("short").is_some());
        assert!(weak("long enough!").is_none());
    }
}
