use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::fs;
use tokio::sync::Mutex;

mod codex_auth;
mod codex_adapter;
mod config;
mod errors;
mod files;
mod logging;
mod middleware;
mod normalization;
mod routes;
mod state;
mod streaming;
#[cfg(test)]
mod test_support;
mod types;
mod upstream;

#[tokio::main]
async fn main() -> Result<()> {
    config::init_tracing();
    let config = config::Config::from_env();

    let auth_path = config.state_root.join("auth").join("auth.json");

    if matches!(std::env::args().nth(1).as_deref(), Some("login")) {
        if let Some(parent) = auth_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let http = config::build_upstream_client(false)?;
        codex_auth::run_login(auth_path, http).await?;
        return Ok(());
    }

    let files_dir = config.files_dir();
    let db_dir = config.db_dir();
    let logs_dir = config.logs_dir();
    fs::create_dir_all(&files_dir).await?;
    fs::create_dir_all(&db_dir).await?;
    fs::create_dir_all(&logs_dir).await?;
    if config.log_full_body {
        fs::create_dir_all(config.payloads_dir()).await?;
    }

    let db_path = db_dir.join("proxy.sqlite3");
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true),
        )
        .await
        .context("failed to open sqlite db")?;
    files::init_db(&db).await?;

    let upstream_client = config::build_upstream_client(config.tune_upstream_transport)?;
    let auth = codex_auth::CodexAuth::load(auth_path, upstream_client.clone()).await?;

    let state = state::AppState {
        client: upstream_client,
        db,
        files_dir,
        logs_dir: logs_dir.clone(),
        upstream_url: config.upstream_url.clone(),
        upstream_identity_encoding: config.upstream_identity_encoding,
        static_api_key: config.static_api_key.clone(),
        log_full_body: config.log_full_body,
        log_write_lock: Arc::new(Mutex::new(())),
        model_aliases: config.model_aliases.clone(),
        auth: Arc::new(auth),
        codex_adapter: Arc::new(codex_adapter::CodexAdapter::new()),
    };

    let app = routes::build_router(state);

    tracing::info!("starting codex-proxy on {}", config.listen_addr);
    tracing::info!("request diagnostics logging to {}", logs_dir.display());
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to install ctrl_c handler: {err}");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => tracing::warn!("failed to install SIGTERM handler: {err}"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received ctrl_c, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
