use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{Value, json};

use crate::types::{ErrorBody, ErrorEnvelope};

pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    json_error_with_type(status, message, None)
}

pub(crate) fn json_error_with_type(
    status: StatusCode,
    message: &str,
    error_type: Option<&str>,
) -> Response {
    json_error_full(status, message, error_type, None, None)
}

pub(crate) fn json_error_full(
    status: StatusCode,
    message: &str,
    error_type: Option<&str>,
    code: Option<&str>,
    param: Option<&str>,
) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody {
                message: message.to_string(),
                r#type: error_type.map(ToOwned::to_owned),
                code: code.map(ToOwned::to_owned),
                param: param.map(ToOwned::to_owned),
            },
        }),
    )
        .into_response()
}

pub(crate) fn unsupported_proxy_route_error(path: &str) -> Response {
    json_error_full(
        StatusCode::NOT_IMPLEMENTED,
        &format!(
            "Route {path} is recognized but not supported by this proxy's stable Codex facade."
        ),
        Some("unsupported_route_error"),
        Some("unsupported_route"),
        None,
    )
}

pub(crate) fn impossible_upstream_route_error(path: &str) -> Response {
    json_error_full(
        StatusCode::NOT_IMPLEMENTED,
        &format!(
            "Route {path} requires durable response lifecycle semantics that cannot be truthfully provided over the ChatGPT Codex upstream."
        ),
        Some("upstream_capability_error"),
        Some("upstream_capability"),
        None,
    )
}

/// Normalize an upstream error body to the official OpenAI envelope shape.
///
/// The ChatGPT/Codex backend ships two incompatible error shapes:
///   * FastAPI default: `{"detail":"Unsupported parameter: X"}`
///   * OpenAI structured: `{"error":{"message","type","code","param"}}`
///
/// Clients using the OpenAI SDK only parse the second form; the first becomes
/// an opaque string. Translate `detail` into the canonical shape so typed
/// exceptions work end-to-end.
pub(crate) fn normalize_upstream_error_body(raw: &str, status: StatusCode) -> String {
    let parsed: Option<Value> = serde_json::from_str(raw).ok();
    if let Some(value) = &parsed
        && value.get("error").is_some()
    {
        return raw.to_string();
    }
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("detail"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| raw.trim().to_string());
    let error_type = match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "invalid_request_error",
    };
    json!({
        "error": {
            "message": message,
            "type": error_type,
            "code": serde_json::Value::Null,
            "param": serde_json::Value::Null,
        }
    })
    .to_string()
}

pub(crate) async fn upstream_error_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let raw = response.text().await.unwrap_or_else(|_| {
        json!({
            "error": {
                "message": "Upstream request failed and response body could not be read",
                "type": "server_error"
            }
        })
        .to_string()
    });
    let body = normalize_upstream_error_body(&raw, status);

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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    #[test]
    fn detail_shape_error_translated_to_openai_envelope() -> Result<()> {
        let out = normalize_upstream_error_body(
            r#"{"detail":"Unsupported parameter: temperature"}"#,
            StatusCode::BAD_REQUEST,
        );
        let parsed: Value = serde_json::from_str(&out)?;
        assert_eq!(
            parsed["error"]["message"],
            json!("Unsupported parameter: temperature")
        );
        assert_eq!(parsed["error"]["type"], json!("invalid_request_error"));
        assert!(parsed["error"].get("code").is_some(), "code key must be present (null ok)");
        assert!(parsed["error"].get("param").is_some(), "param key must be present (null ok)");
        Ok(())
    }

    #[test]
    fn structured_error_passes_through_unchanged() {
        let raw = r#"{"error":{"message":"Invalid value","type":"invalid_request_error","param":"include[1]","code":"invalid_value"}}"#;
        let out = normalize_upstream_error_body(raw, StatusCode::BAD_REQUEST);
        assert_eq!(out, raw);
    }

    #[test]
    fn non_json_body_wrapped_as_message() -> Result<()> {
        let out = normalize_upstream_error_body(
            "Cloudflare blocked the request",
            StatusCode::SERVICE_UNAVAILABLE,
        );
        let parsed: Value = serde_json::from_str(&out)?;
        assert_eq!(
            parsed["error"]["message"],
            json!("Cloudflare blocked the request")
        );
        assert_eq!(parsed["error"]["type"], json!("server_error"));
        Ok(())
    }

    #[tokio::test]
    async fn proxy_error_envelope_includes_code_and_param_when_set() -> Result<()> {
        let response = unsupported_proxy_route_error("/v1/embeddings");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let parsed: Value = serde_json::from_slice(&body)?;
        assert_eq!(parsed["error"]["type"], json!("unsupported_route_error"));
        assert_eq!(parsed["error"]["code"], json!("unsupported_route"));
        // `param` should be absent when None (serde_skip_serializing_if = Option::is_none)
        assert!(
            parsed["error"].get("param").is_none()
                || parsed["error"]["param"] == Value::Null,
            "param should be absent when None"
        );
        Ok(())
    }
}
