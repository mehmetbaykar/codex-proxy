#![cfg(test)]

use std::sync::Arc;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::fs;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::codex_adapter::CodexAdapter;
use crate::files::init_db;
use crate::state::AppState;

pub(crate) async fn test_state(upstream_url: impl Into<String>) -> Result<AppState> {
    let root = std::env::temp_dir().join(format!("codex-proxy-test-{}", Uuid::new_v4()));
    let files_dir = root.join("files");
    let logs_dir = root.join("logs");
    let db_dir = root.join("db");
    fs::create_dir_all(&files_dir).await?;
    fs::create_dir_all(&logs_dir).await?;
    fs::create_dir_all(&db_dir).await?;
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(db_dir.join("proxy.sqlite3"))
                .create_if_missing(true),
        )
        .await?;
    init_db(&db).await?;
    Ok(AppState {
        client: reqwest::Client::builder().build()?,
        db,
        files_dir,
        logs_dir,
        upstream_url: upstream_url.into(),
        upstream_identity_encoding: false,
        static_api_key: None,
        log_full_body: false,
        log_write_lock: Arc::new(Mutex::new(())),
        model_aliases: Arc::new(std::collections::HashMap::new()),
        auth: Arc::new(crate::codex_auth::CodexAuth::for_tests()),
        codex_adapter: Arc::new(CodexAdapter::new()),
    })
}
