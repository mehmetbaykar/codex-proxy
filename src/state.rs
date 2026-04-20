use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::http::Method;
use reqwest::Client;
use serde_json::Value;
use sqlx::{Pool, Sqlite};
use tokio::sync::Mutex;

use crate::codex_adapter::CodexAdapter;
use crate::codex_auth::CodexAuth;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) client: Client,
    pub(crate) db: Pool<Sqlite>,
    pub(crate) files_dir: PathBuf,
    pub(crate) logs_dir: PathBuf,
    pub(crate) upstream_url: String,
    pub(crate) upstream_identity_encoding: bool,
    pub(crate) static_api_key: Option<String>,
    pub(crate) log_full_body: bool,
    pub(crate) log_write_lock: Arc<Mutex<()>>,
    pub(crate) model_aliases: Arc<HashMap<String, String>>,
    pub(crate) auth: Arc<CodexAuth>,
    pub(crate) codex_adapter: Arc<CodexAdapter>,
    pub(crate) originator: Arc<str>,
    pub(crate) user_agent: Arc<str>,
}

#[derive(Clone, Default)]
pub(crate) struct WsSessionState {
    pub(crate) last_request: Option<Value>,
    pub(crate) last_response_output: Option<Value>,
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub(crate) request_id: String,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) route_kind: &'static str,
    pub(crate) content_type: Option<String>,
    pub(crate) client_ip: Option<String>,
    pub(crate) client_request_id: Option<String>,
    pub(crate) request_started_at: Instant,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ClientStreamOptions {
    pub(crate) include_usage: bool,
}
