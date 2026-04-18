use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use crate::types::{ErrorBody, ErrorEnvelope};

pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    json_error_with_type(status, message, None)
}

pub(crate) fn json_error_with_type(
    status: StatusCode,
    message: &str,
    error_type: Option<&str>,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                message: message.to_string(),
                r#type: error_type.map(ToOwned::to_owned),
            },
        }),
    )
        .into_response()
}

pub(crate) fn unsupported_proxy_route_error(path: &str) -> Response {
    json_error_with_type(
        StatusCode::NOT_IMPLEMENTED,
        &format!(
            "Route {path} is recognized but not supported by this proxy's stable Codex facade."
        ),
        Some("unsupported_route_error"),
    )
}

pub(crate) fn impossible_upstream_route_error(path: &str) -> Response {
    json_error_with_type(
        StatusCode::NOT_IMPLEMENTED,
        &format!(
            "Route {path} requires durable response lifecycle semantics that cannot be truthfully provided over the ChatGPT Codex upstream."
        ),
        Some("upstream_capability_error"),
    )
}

pub(crate) async fn upstream_error_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let body = response.text().await.unwrap_or_else(|_| {
        json!({
            "error": {
                "message": "Upstream request failed and response body could not be read"
            }
        })
        .to_string()
    });

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    if let Ok(header_value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(CONTENT_TYPE, header_value);
    }
    response
}

pub(crate) fn upstream_open_error_response(err: &anyhow::Error) -> Response {
    let message = err.to_string();
    if message.contains("Cannot read") && message.contains("codex-proxy login") {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex upstream authentication is missing. Run: docker exec -it codex-proxy codex-proxy login",
        );
    }
    if message.contains("refresh_token rejected") {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex refresh token is no longer valid. Run: docker exec -it codex-proxy codex-proxy login",
        );
    }
    json_error(StatusCode::BAD_GATEWAY, "Failed to open upstream stream")
}
