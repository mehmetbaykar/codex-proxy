use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;

pub(crate) const SUPPORTED_MODELS: &[&str] = &[
    "gpt-5.3-codex",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2-codex",
    "gpt-5.1-codex-max",
    "gpt-5.2",
    "gpt-5.1-codex-mini",
    "gpt-5.3-codex-spark",
];
pub(crate) const DEFAULT_UPSTREAM_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";
pub(crate) const CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";
pub(crate) const DEFAULT_INSTRUCTIONS: &str = "You are Codex, a coding assistant.";
pub(crate) const REQUESTS_LOG_FILE: &str = "requests.jsonl";
pub(crate) const UPSTREAM_LOG_FILE: &str = "upstream.jsonl";
pub(crate) const PAYLOADS_DIR_NAME: &str = "payloads";
pub(crate) const CURSOR_MAX_TOOL_CALL_ID_LEN: usize = 40;

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) listen_addr: SocketAddr,
    pub(crate) state_root: PathBuf,
    pub(crate) upstream_url: String,
    pub(crate) upstream_identity_encoding: bool,
    pub(crate) tune_upstream_transport: bool,
    pub(crate) static_api_key: Option<String>,
    pub(crate) log_full_body: bool,
    pub(crate) model_aliases: Arc<HashMap<String, String>>,
}

impl Config {
    pub(crate) fn from_env() -> Self {
        let bind_host = std::env::var("BIND_ADDR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "0.0.0.0".to_string());
        let port = std::env::var("PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(3000);
        let listen_addr = std::env::var("LISTEN_ADDR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| {
                format!("{bind_host}:{port}")
                    .parse::<SocketAddr>()
                    .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], port)))
            });

        let state_root = PathBuf::from(
            std::env::var("STATE_ROOT")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| ".state".to_string()),
        );
        let static_api_key = std::env::var("CURSOR_PROXY_API_KEY")
            .ok()
            .or_else(|| std::env::var("CODEX_CURSOR_PROXY_API_KEY").ok())
            .filter(|value| !value.trim().is_empty());

        Self {
            listen_addr,
            state_root,
            upstream_url: std::env::var("CODEX_UPSTREAM_URL")
                .unwrap_or_else(|_| DEFAULT_UPSTREAM_URL.to_string()),
            upstream_identity_encoding: env_flag("CODEX_UPSTREAM_IDENTITY_ENCODING"),
            tune_upstream_transport: env_flag("CODEX_UPSTREAM_TRANSPORT_TUNING"),
            static_api_key,
            log_full_body: env_flag("PROXY_LOG_FULL_BODY") || env_flag("IS_DEBUG"),
            model_aliases: Arc::new(load_model_aliases()),
        }
    }

    pub(crate) fn files_dir(&self) -> PathBuf {
        self.state_root.join("files")
    }

    pub(crate) fn db_dir(&self) -> PathBuf {
        self.state_root.join("db")
    }

    pub(crate) fn logs_dir(&self) -> PathBuf {
        self.state_root.join("logs")
    }

    pub(crate) fn payloads_dir(&self) -> PathBuf {
        self.logs_dir().join(PAYLOADS_DIR_NAME)
    }
}

pub(crate) const DEFAULT_MODEL_ALIASES: &[(&str, &str)] =
    &[("gpt-5.3-codex-spark-preview", "gpt-5.3-codex-spark")];

fn load_model_aliases() -> HashMap<String, String> {
    if let Some(raw) = std::env::var("CODEX_MODEL_ALIASES")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return parse_alias_list(&raw);
    }
    DEFAULT_MODEL_ALIASES
        .iter()
        .map(|(alias, canonical)| ((*alias).to_string(), (*canonical).to_string()))
        .collect()
}

fn parse_alias_list(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let (alias, canonical) = pair.split_once('=')?;
            let alias = alias.trim();
            let canonical = canonical.trim();
            if alias.is_empty() || canonical.is_empty() {
                return None;
            }
            Some((alias.to_string(), canonical.to_string()))
        })
        .collect()
}

pub(crate) fn env_flag(key: &str) -> bool {
    std::env::var(key).ok().as_deref().map(str::trim) == Some("1")
}

pub(crate) fn init_tracing() {
    let default_filter = if env_flag("IS_DEBUG") {
        "codex_proxy=debug,info"
    } else {
        "codex_proxy=info,info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string()))
        .init();
}

pub(crate) fn build_upstream_client(tune_upstream_transport: bool) -> Result<Client> {
    let builder = if tune_upstream_transport {
        Client::builder()
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_keepalive_interval(Duration::from_secs(30))
            .tcp_keepalive_retries(3)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .http2_adaptive_window(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(300))
    } else {
        Client::builder()
    };
    builder.build().map_err(Into::into)
}
