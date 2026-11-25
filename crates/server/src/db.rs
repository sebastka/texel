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

/// A memory card, without its image.
pub struct CardMeta {
    pub id: u64,
    /// "ps1" or "ps2".
    pub console: String,
    pub name: String,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
}

fn card_meta(r: &sqlx::mysql::MySqlRow) -> anyhow::Result<CardMeta> {
    Ok(CardMeta {
        id: r.try_get("id")?,
        console: r.try_get("console")?,
        name: r.try_get("name")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
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

    // --- Settings ---------------------------------------------------------------
    // The administrators' (see `settings`): a row per setting, its value as JSON.

    /// Every setting: name and value.
    pub async fn settings(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT name, value FROM settings")
            .fetch_all(&self.pool)
            .await?)
    }

    /// Sets these settings (the others stay), at once.
    pub async fn set_settings(&self, rows: &[(String, String)]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for (name, value) in rows {
            sqlx::query(
                "INSERT INTO settings (name, value) VALUES (?, ?) ON DUPLICATE KEY UPDATE value = VALUES(value)",
            )
            .bind(name)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // --- Memory cards ---------------------------------------------------------
    // A user's own cards, named, of one console ("ps1" or "ps2"): inserted in
    // whichever game the user plays.

    /// A user's cards of a console, with their images, by name.
    pub async fn cards(&self, user: u64, console: &str) -> anyhow::Result<Vec<(CardMeta, Vec<u8>)>> {
        let rows = sqlx::query(
            "SELECT id, console, name, data, CAST(UNIX_TIMESTAMP(created_at) AS UNSIGNED) AS created_at, \
             CAST(UNIX_TIMESTAMP(updated_at) AS UNSIGNED) AS updated_at \
             FROM cards WHERE user_id = ? AND console = ? ORDER BY name, id",
        )
        .bind(user)
        .bind(console)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok((card_meta(r)?, r.try_get("data")?))).collect()
    }

    /// One of a user's cards: what it is, and its image.
    pub async fn card(&self, user: u64, id: u64) -> anyhow::Result<Option<(CardMeta, Vec<u8>)>> {
        let row = sqlx::query(
            "SELECT id, console, name, data, CAST(UNIX_TIMESTAMP(created_at) AS UNSIGNED) AS created_at, \
             CAST(UNIX_TIMESTAMP(updated_at) AS UNSIGNED) AS updated_at \
             FROM cards WHERE user_id = ? AND id = ?",
        )
        .bind(user)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok((card_meta(&r)?, r.try_get("data")?))).transpose()
    }

    /// The console of one of a user's cards (None: not theirs).
    pub async fn card_console(&self, user: u64, id: u64) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT console FROM cards WHERE user_id = ? AND id = ?")
                .bind(user)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn count_cards(&self, user: u64, console: &str) -> anyhow::Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cards WHERE user_id = ? AND console = ?")
            .bind(user)
            .bind(console)
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// A new card; returns its id.
    pub async fn create_card(&self, user: u64, console: &str, name: &str, data: &[u8]) -> anyhow::Result<u64> {
        let done = sqlx::query("INSERT INTO cards (user_id, console, name, data) VALUES (?, ?, ?, ?)")
            .bind(user)
            .bind(console)
            .bind(name)
            .bind(data)
            .execute(&self.pool)
            .await?;
        Ok(done.last_insert_id())
    }

    /// Renames one of a user's cards; false when it is not theirs.
    pub async fn rename_card(&self, user: u64, id: u64, name: &str) -> anyhow::Result<bool> {
        if self.card_console(user, id).await?.is_none() {
            return Ok(false);
        }
        sqlx::query("UPDATE cards SET name = ? WHERE user_id = ? AND id = ?")
            .bind(name)
            .bind(user)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    /// Replaces a card's image (the caller checked it is the user's).
    pub async fn put_card(&self, user: u64, id: u64, data: &[u8]) -> anyhow::Result<()> {
        sqlx::query("UPDATE cards SET data = ? WHERE user_id = ? AND id = ?")
            .bind(data)
            .bind(user)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Writes two cards at once (a save moved from one to the other): both
    /// or neither.
    pub async fn put_cards(&self, user: u64, cards: [(u64, &[u8]); 2]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for (id, data) in cards {
            sqlx::query("UPDATE cards SET data = ? WHERE user_id = ? AND id = ?")
                .bind(data)
                .bind(user)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Deletes one of a user's cards; false when it is not theirs.
    pub async fn delete_card(&self, user: u64, id: u64) -> anyhow::Result<bool> {
        let done = sqlx::query("DELETE FROM cards WHERE user_id = ? AND id = ?")
            .bind(user)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The cards a game was last played with, in its two slots.
    pub async fn last_cards(&self, user: u64, key: &str) -> anyhow::Result<[Option<u64>; 2]> {
        let row: Option<(Option<u64>, Option<u64>)> =
            sqlx::query_as("SELECT card1, card2 FROM game_settings WHERE user_id = ? AND save_key = ?")
                .bind(user)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map_or([None, None], |(a, b)| [a, b]))
    }

    /// Records the cards a game is played with (the caller checked they
    /// are the user's).
    pub async fn set_last_cards(&self, user: u64, key: &str, cards: [Option<u64>; 2]) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO game_settings (user_id, save_key, card1, card2) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE card1 = VALUES(card1), card2 = VALUES(card2)",
        )
        .bind(user)
        .bind(key)
        .bind(cards[0])
        .bind(cards[1])
        .execute(&self.pool)
        .await?;
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
