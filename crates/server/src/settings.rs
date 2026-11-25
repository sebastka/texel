//! Server settings an administrator changes in the administration page,
//! kept in the database (a row per setting) and in memory. Features still
//! being tried out start off, behind them.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::auth::Admin;
use crate::state::AppState;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Netplay rooms opened from now on hide the input delay: members run
    /// ahead with the others' last inputs and go back when they differ
    /// (rollback). Off: lockstep, each frame waits for every input.
    pub netplay_rollback: bool,
}

impl Settings {
    /// From the database's rows (`name`, JSON value): unknown names and
    /// values that do not fit are left out (their default is used).
    fn from_rows(rows: Vec<(String, String)>) -> Self {
        let mut out = serde_json::to_value(Self::default()).expect("settings serialise");
        let map = out.as_object_mut().expect("an object");
        for (name, value) in rows {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&value) else {
                continue;
            };
            // Of the right type only.
            if let Some(old) = map.get(&name)
                && std::mem::discriminant(old) == std::mem::discriminant(&value)
            {
                map.insert(name, value);
            }
        }
        serde_json::from_value(out).unwrap_or_default()
    }

    fn to_rows(&self) -> Vec<(String, String)> {
        let value = serde_json::to_value(self).expect("settings serialise");
        value
            .as_object()
            .expect("an object")
            .iter()
            .map(|(k, v)| (k.clone(), v.to_string()))
            .collect()
    }
}

/// The settings in the database (defaults when it has none).
pub async fn load(db: &crate::db::Db) -> anyhow::Result<Settings> {
    Ok(Settings::from_rows(db.settings().await?))
}

pub async fn get(State(state): State<Arc<AppState>>, _: Admin) -> Response {
    Json(state.settings.read().await.clone()).into_response()
}

pub async fn put(State(state): State<Arc<AppState>>, Admin(admin): Admin, Json(new): Json<Settings>) -> Response {
    // Held while writing: two administrators saving at once leave one's.
    let mut current = state.settings.write().await;
    if let Err(e) = state.db.set_settings(&new.to_rows()).await {
        tracing::error!(error = %e, "saving the settings failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, "saving the settings failed").into_response();
    }
    if *current != new {
        tracing::info!(by = admin.id, settings = ?new, "settings changed");
    }
    *current = new.clone();
    Json(new).into_response()
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn rows_round_trip_and_bad_ones_are_left_out() {
        let on = Settings { netplay_rollback: true };
        assert_eq!(Settings::from_rows(on.to_rows()), on);
        let rows = vec![
            ("netplayRollback".to_owned(), "\"yes\"".to_owned()),
            ("gone".to_owned(), "true".to_owned()),
            ("broken".to_owned(), "{".to_owned()),
        ];
        assert_eq!(Settings::from_rows(rows), Settings::default());
    }
}
