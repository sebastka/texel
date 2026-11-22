//! Users, sessions, sign-ins under way and invitations, in the database.

use serde::Serialize;
use sqlx::Row;

use super::{Role, User};
use crate::db::Db;

/// A user, as administrators see them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRow {
    pub id: u64,
    pub name: String,
    pub role: Role,
    /// "oidc", or "password" (an invited user).
    pub kind: &'static str,
    pub email: Option<String>,
    pub avatar: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub last_seen_at: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRow {
    pub id: u64,
    pub kind: String,
    pub note: String,
    /// A reset's user.
    pub user: Option<String>,
    pub created_by: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
    pub used_at: Option<u64>,
    pub used_by: Option<String>,
}

/// An invitation or reset, valid (not used, not expired).
pub struct Invite {
    pub id: u64,
    pub kind: String,
    pub note: String,
    /// A reset's user, and their e-mail address.
    pub user: Option<(u64, String)>,
}

fn user_from(r: &sqlx::mysql::MySqlRow) -> Result<User, sqlx::Error> {
    let role: String = r.try_get("role")?;
    Ok(User {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        role: if role == "admin" { Role::Admin } else { Role::Guest },
        email: r.try_get("email")?,
        oidc: r.try_get::<Option<String>, _>("oidc_subject")?.is_some(),
        avatar: r.try_get::<Option<String>, _>("avatar_version")?.map(|v| {
            format!(
                "/api/users/{}/avatar?v={v}",
                r.try_get::<u64, _>("id").unwrap_or_default()
            )
        }),
    })
}

/// The columns `user_from` reads (a macro: queries stay literal strings).
macro_rules! user_columns {
    () => {
        "u.id, u.name, u.role, u.email, u.oidc_subject, u.avatar_version"
    };
}

impl Db {
    // --- Sessions ---------------------------------------------------------------

    /// The user of a live session.
    pub async fn session_user(&self, token_hash: &[u8]) -> anyhow::Result<Option<User>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            user_columns!(),
            " FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ? AND s.expires_at > CURRENT_TIMESTAMP(3)"
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(user_from).transpose()?)
    }

    pub async fn create_session(
        &self,
        token_hash: &[u8],
        user: u64,
        id_token: Option<&str>,
        days: u64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, id_token, expires_at) \
             VALUES (?, ?, ?, CURRENT_TIMESTAMP(3) + INTERVAL ? DAY)",
        )
        .bind(token_hash)
        .bind(user)
        .bind(id_token)
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ends a session; returns its ID token (to sign out at the provider).
    pub async fn delete_session(&self, token_hash: &[u8]) -> anyhow::Result<Option<String>> {
        let id_token: Option<Option<String>> = sqlx::query_scalar("SELECT id_token FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(id_token.flatten())
    }

    pub async fn touch_user(&self, user: u64) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET last_seen_at = CURRENT_TIMESTAMP(3) WHERE id = ?")
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drops expired sessions, abandoned sign-ins, and invitations expired
    /// for a month.
    pub async fn clean_auth(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM sessions WHERE expires_at < CURRENT_TIMESTAMP(3)")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM oidc_logins WHERE created_at < CURRENT_TIMESTAMP(3) - INTERVAL 10 MINUTE")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM invites WHERE expires_at < CURRENT_TIMESTAMP(3) - INTERVAL 30 DAY")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- OpenID Connect -----------------------------------------------------------

    pub async fn put_oidc_login(
        &self,
        state: &str,
        verifier: &str,
        nonce: &str,
        browser_hash: &[u8],
        return_to: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO oidc_logins (state, verifier, nonce, browser_hash, return_to) VALUES (?, ?, ?, ?, ?)")
            .bind(state)
            .bind(verifier)
            .bind(nonce)
            .bind(browser_hash)
            .bind(return_to)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Takes a sign-in under way (once), if recent: its verifier, nonce,
    /// browser hash and return path.
    pub async fn take_oidc_login(&self, state: &str) -> anyhow::Result<Option<(String, String, Vec<u8>, String)>> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT verifier, nonce, browser_hash, return_to FROM oidc_logins \
             WHERE state = ? AND created_at > CURRENT_TIMESTAMP(3) - INTERVAL 10 MINUTE FOR UPDATE",
        )
        .bind(state)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM oidc_logins WHERE state = ?")
            .bind(state)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(match row {
            Some(r) => Some((
                r.try_get("verifier")?,
                r.try_get("nonce")?,
                r.try_get("browser_hash")?,
                r.try_get("return_to")?,
            )),
            None => None,
        })
    }

    /// The user of an OpenID Connect subject, created or brought up to
    /// date (name and role, from the provider and the configuration).
    pub async fn upsert_oidc_user(&self, issuer: &str, subject: &str, name: &str, role: Role) -> anyhow::Result<User> {
        sqlx::query(
            "INSERT INTO users (name, role, oidc_issuer, oidc_subject, last_seen_at) \
             VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP(3)) \
             ON DUPLICATE KEY UPDATE name = VALUES(name), role = VALUES(role), last_seen_at = VALUES(last_seen_at)",
        )
        .bind(name)
        .bind(role.as_str())
        .bind(issuer)
        .bind(subject)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users u WHERE u.oidc_issuer = ? AND u.oidc_subject = ?"
        ))
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await?;
        Ok(user_from(&row)?)
    }

    // --- Pictures ---------------------------------------------------------------------

    pub async fn avatar_url(&self, user: u64) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT avatar_url FROM users WHERE id = ?")
            .bind(user)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    /// Sets (with the URL it came from) or drops a user's picture.
    pub async fn set_avatar(&self, user: u64, picture: Option<(&str, &super::avatar::Picture)>) -> anyhow::Result<()> {
        let (url, bytes, media_type, version) = match picture {
            Some((url, p)) => (
                Some(url),
                Some(&p.bytes[..]),
                Some(p.media_type),
                Some(p.version.as_str()),
            ),
            None => (None, None, None, None),
        };
        sqlx::query("UPDATE users SET avatar_url = ?, avatar = ?, avatar_type = ?, avatar_version = ? WHERE id = ?")
            .bind(url)
            .bind(bytes)
            .bind(media_type)
            .bind(version)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A user's picture and its media type.
    pub async fn avatar(&self, user: u64) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        Ok(
            sqlx::query_as("SELECT avatar, avatar_type FROM users WHERE id = ? AND avatar IS NOT NULL")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    // --- Invited users --------------------------------------------------------------

    /// A user by e-mail address, with their password hash.
    pub async fn password_user(&self, email: &str) -> anyhow::Result<Option<(User, String)>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            user_columns!(),
            ", u.password_hash FROM users u WHERE u.email = ? AND u.password_hash IS NOT NULL"
        ))
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some((user_from(&r)?, r.try_get("password_hash")?))),
            None => Ok(None),
        }
    }

    pub async fn password_hash(&self, user: u64) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
            .bind(user)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    pub async fn email_taken(&self, email: &str) -> anyhow::Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE email = ?")
            .bind(email)
            .fetch_one(&self.pool)
            .await?;
        Ok(n > 0)
    }

    /// Registers an invited user, using the invitation (both or neither):
    /// None when the invitation was used meanwhile or the address taken.
    pub async fn register(&self, invite: u64, email: &str, name: &str, hash: &str) -> anyhow::Result<Option<User>> {
        let mut tx = self.pool.begin().await?;
        let used = sqlx::query(
            "UPDATE invites SET used_at = CURRENT_TIMESTAMP(3) \
             WHERE id = ? AND kind = 'invite' AND used_at IS NULL AND expires_at > CURRENT_TIMESTAMP(3)",
        )
        .bind(invite)
        .execute(&mut *tx)
        .await?;
        if used.rows_affected() != 1 {
            return Ok(None);
        }
        let inserted = sqlx::query(
            "INSERT IGNORE INTO users (name, role, email, password_hash, last_seen_at) \
             VALUES (?, 'guest', ?, ?, CURRENT_TIMESTAMP(3))",
        )
        .bind(name)
        .bind(email)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() != 1 {
            return Ok(None);
        }
        let id = inserted.last_insert_id();
        sqlx::query("UPDATE invites SET used_by = ? WHERE id = ?")
            .bind(id)
            .bind(invite)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(User {
            id,
            name: name.to_owned(),
            role: Role::Guest,
            email: Some(email.to_owned()),
            oidc: false,
            avatar: None,
        }))
    }

    /// Sets a password with a reset token (used once); signs the user out
    /// everywhere. The user, or None when the token was used meanwhile.
    pub async fn reset_password(&self, invite: u64, user: u64, hash: &str) -> anyhow::Result<Option<User>> {
        let mut tx = self.pool.begin().await?;
        let used = sqlx::query(
            "UPDATE invites SET used_at = CURRENT_TIMESTAMP(3), used_by = ? \
             WHERE id = ? AND kind = 'reset' AND user_id = ? AND used_at IS NULL \
             AND expires_at > CURRENT_TIMESTAMP(3)",
        )
        .bind(user)
        .bind(invite)
        .bind(user)
        .execute(&mut *tx)
        .await?;
        if used.rows_affected() != 1 {
            return Ok(None);
        }
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(hash)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(concat!("SELECT ", user_columns!(), " FROM users u WHERE u.id = ?"))
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(user_from(&row)?))
    }

    /// Sets a user's password; their other sessions end.
    pub async fn set_password(&self, user: u64, hash: &str, keep_session: &[u8]) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(hash)
            .bind(user)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash <> ?")
            .bind(user)
            .bind(keep_session)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_name(&self, user: u64, name: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET name = ? WHERE id = ?")
            .bind(name)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- Invitations ------------------------------------------------------------------

    pub async fn create_invite(
        &self,
        token_hash: &[u8],
        kind: &str,
        user: Option<u64>,
        note: &str,
        created_by: Option<u64>,
        days: u64,
    ) -> anyhow::Result<u64> {
        let r = sqlx::query(
            "INSERT INTO invites (token_hash, kind, user_id, note, created_by, expires_at) \
             VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP(3) + INTERVAL ? DAY)",
        )
        .bind(token_hash)
        .bind(kind)
        .bind(user)
        .bind(note)
        .bind(created_by)
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(r.last_insert_id())
    }

    /// A valid invitation or reset, by its token's hash.
    pub async fn invite(&self, token_hash: &[u8]) -> anyhow::Result<Option<Invite>> {
        let row = sqlx::query(
            "SELECT i.id, i.kind, i.note, i.user_id, u.email FROM invites i LEFT JOIN users u ON u.id = i.user_id \
             WHERE i.token_hash = ? AND i.used_at IS NULL AND i.expires_at > CURRENT_TIMESTAMP(3)",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let user: Option<u64> = r.try_get("user_id")?;
        let email: Option<String> = r.try_get("email")?;
        Ok(Some(Invite {
            id: r.try_get("id")?,
            kind: r.try_get("kind")?,
            note: r.try_get("note")?,
            user: user.map(|u| (u, email.unwrap_or_default())),
        }))
    }

    pub async fn invites(&self) -> anyhow::Result<Vec<InviteRow>> {
        let rows = sqlx::query(
            "SELECT i.id, i.kind, i.note, t.name AS target, c.name AS creator, \
             CAST(UNIX_TIMESTAMP(i.created_at) AS UNSIGNED) AS created_at, \
             CAST(UNIX_TIMESTAMP(i.expires_at) AS UNSIGNED) AS expires_at, \
             CAST(UNIX_TIMESTAMP(i.used_at) AS UNSIGNED) AS used_at, b.name AS user_name \
             FROM invites i LEFT JOIN users t ON t.id = i.user_id LEFT JOIN users c ON c.id = i.created_by \
             LEFT JOIN users b ON b.id = i.used_by ORDER BY i.created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(InviteRow {
                    id: r.try_get("id")?,
                    kind: r.try_get("kind")?,
                    note: r.try_get("note")?,
                    user: r.try_get("target")?,
                    created_by: r.try_get("creator")?,
                    created_at: r.try_get("created_at")?,
                    expires_at: r.try_get("expires_at")?,
                    used_at: r.try_get("used_at")?,
                    used_by: r.try_get("user_name")?,
                })
            })
            .collect()
    }

    pub async fn delete_invite(&self, id: u64) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM invites WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    // --- Users, for administrators ---------------------------------------------------------

    pub async fn users(&self) -> anyhow::Result<Vec<UserRow>> {
        let rows = sqlx::query(
            "SELECT id, name, role, email, oidc_subject, avatar_version, \
             CAST(UNIX_TIMESTAMP(created_at) AS UNSIGNED) AS created_at, \
             CAST(UNIX_TIMESTAMP(last_seen_at) AS UNSIGNED) AS last_seen_at \
             FROM users ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let u = user_from(r)?;
                Ok(UserRow {
                    id: u.id,
                    name: u.name,
                    role: u.role,
                    kind: if u.oidc { "oidc" } else { "password" },
                    email: u.email,
                    avatar: u.avatar,
                    created_at: r.try_get("created_at")?,
                    last_seen_at: r.try_get("last_seen_at")?,
                })
            })
            .collect()
    }

    pub async fn user(&self, id: u64) -> anyhow::Result<Option<User>> {
        let row = sqlx::query(concat!("SELECT ", user_columns!(), " FROM users u WHERE u.id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(user_from).transpose()?)
    }

    /// Deletes a user with their sessions and saves.
    pub async fn delete_user(&self, id: u64) -> anyhow::Result<bool> {
        let r = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }
}
