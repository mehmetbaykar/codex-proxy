use anyhow::Result;
use axum::http::{
    HeaderMap, HeaderName,
    header::{ACCEPT_ENCODING, CONTENT_TYPE, USER_AGENT},
};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::config::{
    CLIENT_REQUEST_ID_HEADER, FORWARDED_CODEX_BETA_HEADERS, REQUEST_ID_HEADER, UPSTREAM_LOG_FILE,
};
use crate::logging::log_request_event;
use crate::state::{AppState, RequestContext};

pub(crate) const FORWARDED_SUCCESS_HEADERS: &[&str] = &[
    "x-ratelimit-remaining-requests",
    "x-ratelimit-remaining-tokens",
    "x-ratelimit-limit-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "x-request-id",
];

pub(crate) async fn build_chatgpt_upstream_headers(
    state: &AppState,
    request_context: Option<&RequestContext>,
    incoming_headers: &HeaderMap,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(account_id) = state.auth.current_account_id().await
        && let Ok(value) = account_id.parse()
    {
        headers.insert("ChatGPT-Account-Id", value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(state.originator.as_ref()) {
        headers.insert("originator", value);
    }
    if let Some(context) = request_context {
        if let Ok(value) = context.request_id.parse() {
            headers.insert("session_id", value);
        }
        if let Ok(value) = axum::http::HeaderValue::from_str(&context.request_id) {
            headers.insert(REQUEST_ID_HEADER, value);
        }
        if let Some(client_request_id) = &context.client_request_id
            && let Ok(value) = axum::http::HeaderValue::from_str(client_request_id)
        {
            headers.insert(CLIENT_REQUEST_ID_HEADER, value);
        }
    } else if let Some(value) = incoming_headers.get(REQUEST_ID_HEADER) {
        headers.insert(REQUEST_ID_HEADER, value.clone());
        headers.insert("session_id", value.clone());
    }
    for name in FORWARDED_CODEX_BETA_HEADERS {
        if let Some(value) = incoming_headers.get(*name)
            && let Ok(name) = HeaderName::from_bytes(name.as_bytes())
        {
            headers.insert(name, value.clone());
        }
    }
    headers
}

pub(crate) fn processed_success_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if FORWARDED_SUCCESS_HEADERS.contains(&key.as_str()) {
            forwarded.insert(name.clone(), value.clone());
        } else if let Ok(provider_name) = format!("llm_provider-{key}").parse::<HeaderName>() {
            forwarded.insert(provider_name, value.clone());
        }
    }
    forwarded
}

pub(crate) async fn open_upstream_response(
    state: &AppState,
    request_context: Option<&RequestContext>,
    incoming_headers: &HeaderMap,
    body: &Value,
) -> Result<reqwest::Response> {
    let token = state.auth.current_token().await?;
    let response = send_upstream(state, request_context, incoming_headers, body, &token).await?;
    let response = if response.status() == StatusCode::UNAUTHORIZED {
        tracing::info!("Codex upstream returned 401; refreshing token and retrying once");
        let fresh = state.auth.force_refresh().await?;
        send_upstream(state, request_context, incoming_headers, body, &fresh).await?
    } else {
        response
    };
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

async fn send_upstream(
    state: &AppState,
    request_context: Option<&RequestContext>,
    incoming_headers: &HeaderMap,
    body: &Value,
    token: &str,
) -> Result<reqwest::Response> {
    let chatgpt_headers =
        build_chatgpt_upstream_headers(state, request_context, incoming_headers).await;
    let mut request = state
        .client
        .post(&state.upstream_url)
        .bearer_auth(token)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "text/event-stream")
        .header("connection", "Keep-Alive")
        .header(USER_AGENT, state.user_agent.as_ref())
        .json(body);
    if state.upstream_identity_encoding {
        request = request.header(ACCEPT_ENCODING, "identity");
    }
    for (name, value) in &chatgpt_headers {
        request = request.header(name, value);
    }
    request.send().await.map_err(Into::into)
}
