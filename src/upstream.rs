use anyhow::Result;
use axum::http::{
    HeaderMap,
    header::{ACCEPT_ENCODING, CONTENT_TYPE, USER_AGENT},
};
use serde_json::{Value, json};

use crate::config::{CLIENT_REQUEST_ID_HEADER, REQUEST_ID_HEADER, UPSTREAM_LOG_FILE};
use crate::logging::log_request_event;
use crate::state::{AppState, RequestContext};

pub(crate) async fn open_upstream_response(
    state: &AppState,
    request_context: Option<&RequestContext>,
    incoming_headers: &HeaderMap,
    body: &Value,
) -> Result<reqwest::Response> {
    let token = upstream_access_token().await?;
    let mut request = state
        .client
        .post(&state.upstream_url)
        .bearer_auth(token)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "text/event-stream")
        .header(USER_AGENT, "codex-proxy/0.1")
        .json(body);
    if state.upstream_identity_encoding {
        request = request.header(ACCEPT_ENCODING, "identity");
    }
    if let Some(context) = request_context {
        request = request.header(REQUEST_ID_HEADER, context.request_id.clone());
        if let Some(client_request_id) = &context.client_request_id {
            request = request.header(CLIENT_REQUEST_ID_HEADER, client_request_id.clone());
        }
    } else if let Some(value) = incoming_headers.get(REQUEST_ID_HEADER) {
        request = request.header(REQUEST_ID_HEADER, value.clone());
    }

    let response = request.send().await?;
    log_request_event(
        state,
        UPSTREAM_LOG_FILE,
        json!({
            "ts": crate::files::now_unix(),
            "phase": "upstream_open",
            "request_id": request_context.map(|context| context.request_id.clone()),
            "path": request_context.map(|context| context.path.clone()),
            "route_kind": request_context.map(|context| context.route_kind),
            "status": response.status().as_u16(),
            "endpoint": state.upstream_url.clone(),
        }),
    )
    .await;
    Ok(response)
}

pub(crate) async fn upstream_access_token() -> Result<String> {
    if let Ok(token) = std::env::var("CODEX_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    let auth_paths = vec![
        std::env::var("AUTH_PATH")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        Some("/app/state/auth/auth.json".to_string()),
        std::env::var("HOME")
            .ok()
            .map(|home| format!("{home}/.codex/auth.json")),
    ];
    for path in auth_paths.into_iter().flatten() {
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                if let Some(token) = value
                    .get("tokens")
                    .and_then(|tokens| tokens.get("access_token"))
                    .and_then(Value::as_str)
                {
                    if !token.is_empty() {
                        return Ok(token.to_string());
                    }
                }
            }
        }
    }
    anyhow::bail!("missing upstream access token")
}
