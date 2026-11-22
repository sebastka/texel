//! The database (MariaDB): users and their sessions (see auth), their
//! memory cards, save states and the BIOS of each game, and the
//! verification hashes. Caches (DATs, artwork, game details) stay
//! on disk: they can be fetched again.

use std::time::{Duration, Instant};

use anyhow::Context;
use sqlx::Row;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};

use crate::verify::{HashCache, HashEntry};

/// How long to wait for the database at startup: in a pod, or a container
/// started at the same time, it may come up after the server.
const CONNECT_FOR: Duration = Duration::from_secs(60);
/// Longest path kept in `file_hashes` (its key).
const MAX_PATH: usize = 700;

pub struct Db {
    pub(crate) pool: MySqlPool,
}

/// A save state, without its data.
pub struct StateInfo {
    pub slot: u8,
    pub size: u64,
    /// Unix seconds.
    pub saved_at: u64,
    pub screenshot: bool,
    pub bios: Option<String>,
}

impl Db {
    /// Connects (waiting for the database to be up) and applies the
    /// migrations.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let deadline = Instant::now() + CONNECT_FOR;
        let pool = loop {
            let attempt = MySqlPoolOptions::new()
                .max_connections(8)
                .acquire_timeout(Duration::from_secs(10))
                .connect(url)
                .await;
            match attempt {
                Ok(pool) => break pool,
                // The error never contains the password.
                Err(e) if Instant::now() < deadline => {
                    tracing::warn!(error = %e, "database not reachable yet");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e).context("connecting to the database"),
            }
        };
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("migrating the database")?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    // --- Memory cards ---------------------------------------------------------

    pub async fn memory_card(&self, user: u64, key: &str, slot: u8) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT data FROM memory_cards WHERE user_id = ? AND save_key = ? AND slot = ?")
                .bind(user)
                .bind(key)
                .bind(slot)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// A user's memory cards: their save keys and slots.
    pub async fn memory_cards(&self, user: u64) -> anyhow::Result<Vec<(String, u8)>> {
        Ok(
            sqlx::query_as("SELECT save_key, slot FROM memory_cards WHERE user_id = ? ORDER BY save_key, slot")
                .bind(user)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn put_memory_card(&self, user: u64, key: &str, slot: u8, data: &[u8]) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO memory_cards (user_id, save_key, slot, data) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE data = VALUES(data)",
        )
        .bind(user)
        .bind(key)
        .bind(slot)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_memory_card(&self, user: u64, key: &str, slot: u8) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM memory_cards WHERE user_id = ? AND save_key = ? AND slot = ?")
            .bind(user)
            .bind(key)
            .bind(slot)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Writes two cards at once (a save moved from one to the other): both
    /// or neither.
    pub async fn put_memory_cards(&self, user: u64, cards: [(&str, u8, &[u8]); 2]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for (key, slot, data) in cards {
            sqlx::query(
                "INSERT INTO memory_cards (user_id, save_key, slot, data) VALUES (?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE data = VALUES(data)",
            )
            .bind(user)
            .bind(key)
            .bind(slot)
            .bind(data)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // --- Save states ------------------------------------------------------------

    pub async fn states(&self, user: u64, key: &str) -> anyhow::Result<Vec<StateInfo>> {
        let rows = sqlx::query(
            "SELECT slot, CAST(LENGTH(data) AS UNSIGNED) AS size, \
             CAST(UNIX_TIMESTAMP(saved_at) AS UNSIGNED) AS saved_at, \
             CAST(screenshot IS NOT NULL AS UNSIGNED) AS screenshot, bios \
             FROM save_states WHERE user_id = ? AND save_key = ? ORDER BY slot",
        )
        .bind(user)
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(StateInfo {
                    slot: r.try_get("slot")?,
                    size: r.try_get("size")?,
                    saved_at: r.try_get("saved_at")?,
                    screenshot: r.try_get::<u64, _>("screenshot")? != 0,
                    bios: r.try_get("bios")?,
                })
            })
            .collect()
    }

    pub async fn state(&self, user: u64, key: &str, slot: u8) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT data FROM save_states WHERE user_id = ? AND save_key = ? AND slot = ?")
                .bind(user)
                .bind(key)
                .bind(slot)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn screenshot(&self, user: u64, key: &str, slot: u8) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar(
            "SELECT screenshot FROM save_states WHERE user_id = ? AND save_key = ? AND slot = ? \
             AND screenshot IS NOT NULL",
        )
        .bind(user)
        .bind(key)
        .bind(slot)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Saves a state; its old screenshot goes (the new one follows).
    pub async fn put_state(
        &self,
        user: u64,
        key: &str,
        slot: u8,
        data: &[u8],
        bios: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO save_states (user_id, save_key, slot, data, screenshot, bios, saved_at) \
             VALUES (?, ?, ?, ?, NULL, ?, CURRENT_TIMESTAMP(3)) \
             ON DUPLICATE KEY UPDATE data = VALUES(data), screenshot = NULL, bios = VALUES(bios), \
             saved_at = VALUES(saved_at)",
        )
        .bind(user)
        .bind(key)
        .bind(slot)
        .bind(data)
        .bind(bios)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets the screenshot of an existing state. False without one.
    pub async fn put_screenshot(&self, user: u64, key: &str, slot: u8, png: &[u8]) -> anyhow::Result<bool> {
        let exists: Option<u8> =
            sqlx::query_scalar("SELECT slot FROM save_states WHERE user_id = ? AND save_key = ? AND slot = ?")
                .bind(user)
                .bind(key)
                .bind(slot)
                .fetch_optional(&self.pool)
                .await?;
        if exists.is_none() {
            return Ok(false);
        }
        sqlx::query("UPDATE save_states SET screenshot = ? WHERE user_id = ? AND save_key = ? AND slot = ?")
            .bind(png)
            .bind(user)
            .bind(key)
            .bind(slot)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    pub async fn delete_state(&self, user: u64, key: &str, slot: u8) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM save_states WHERE user_id = ? AND save_key = ? AND slot = ?")
            .bind(user)
            .bind(key)
            .bind(slot)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- The BIOS of each game ----------------------------------------------------

    /// The BIOS a game was last started with, else the one of its latest
    /// save state (`states_key`: save states are per disc).
    pub async fn last_bios(&self, user: u64, key: &str, states_key: &str) -> anyhow::Result<Option<String>> {
        let chosen: Option<Option<String>> =
            sqlx::query_scalar("SELECT bios FROM game_settings WHERE user_id = ? AND save_key = ?")
                .bind(user)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        if let Some(Some(bios)) = chosen {
            return Ok(Some(bios));
        }
        Ok(sqlx::query_scalar(
            "SELECT bios FROM save_states WHERE user_id = ? AND save_key = ? AND bios IS NOT NULL \
             ORDER BY saved_at DESC LIMIT 1",
        )
        .bind(user)
        .bind(states_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn set_bios(&self, user: u64, key: &str, bios: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO game_settings (user_id, save_key, bios) VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE bios = VALUES(bios)",
        )
        .bind(user)
        .bind(key)
        .bind(bios)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- Verification hashes ------------------------------------------------------

    pub async fn hashes(&self) -> anyhow::Result<HashCache> {
        let rows = sqlx::query("SELECT path, size, mtime, sha1 FROM file_hashes")
            .fetch_all(&self.pool)
            .await?;
        let mut cache = HashCache::default();
        for r in rows {
            cache.files.insert(
                r.try_get("path")?,
                HashEntry {
                    size: r.try_get("size")?,
                    mtime: r.try_get("mtime")?,
                    sha1: r.try_get("sha1")?,
                },
            );
        }
        Ok(cache)
    }

    pub async fn put_hash(&self, path: &str, e: &HashEntry) -> anyhow::Result<()> {
        if path.chars().count() > MAX_PATH {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO file_hashes (path, size, mtime, sha1) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE size = VALUES(size), mtime = VALUES(mtime), sha1 = VALUES(sha1)",
        )
        .bind(path)
        .bind(e.size)
        .bind(e.mtime)
        .bind(&e.sha1)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Save keys are serials ("SLES-00049") or "file-<id>".
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
