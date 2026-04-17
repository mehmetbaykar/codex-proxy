use anyhow::Result;
use axum::http::{
    HeaderMap,
    header::{ACCEPT_ENCODING, CONTENT_TYPE, USER_AGENT},
};
use reqwest::StatusCode;
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
    request.send().await.map_err(Into::into)
}
