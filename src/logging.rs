use axum::http::{HeaderMap, header::CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::warn;
use uuid::Uuid;

use crate::config::{PAYLOADS_DIR_NAME, REQUESTS_LOG_FILE};
use crate::state::{AppState, RequestContext};

pub(crate) fn classify_pathname(path: &str) -> &'static str {
    let normalized = path.trim_end_matches('/');
    match normalized {
        "/v1/chat/completions" | "/chat/completions" => "chatCompletions",
        "/v1/responses" | "/responses" => "responsesApi",
        "/v1/responses/ws" | "/responses/ws" => "responsesWebSocket",
        "/v1/models" | "/models" => "models",
        "/v1/files" | "/files" => "files",
        _ if path.contains("upload") || path.contains("/files") || path.contains("attachment") => {
            "unknownUpload"
        }
        _ => "other",
    }
}

const REDACTED_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];

pub(crate) fn redacted_headers(headers: &HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        let recorded = if REDACTED_HEADER_NAMES.contains(&key.as_str()) {
            Value::String("<redacted>".to_string())
        } else {
            value
                .to_str()
                .map(|text| Value::String(text.to_string()))
                .unwrap_or_else(|_| Value::String("<binary>".to_string()))
        };
        out.insert(key, recorded);
    }
    Value::Object(out)
}

pub(crate) fn forwarded_client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            headers
                .get("cf-connecting-ip")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

async fn write_jsonl_log(state: &AppState, file_name: &str, value: &Value) {
    let _guard = state.log_write_lock.lock().await;
    let Ok(line) = serde_json::to_string(value) else {
        return;
    };

    let path = state.logs_dir.join(file_name);
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(mut file) => {
            if let Err(err) = file.write_all(line.as_bytes()).await {
                warn!("failed to append log {}: {err}", path.display());
                return;
            }
            if let Err(err) = file.write_all(b"\n").await {
                warn!("failed to append log newline {}: {err}", path.display());
            }
        }
        Err(err) => warn!("failed to open log {}: {err}", path.display()),
    }
}

async fn write_dump_bytes(state: &AppState, file_name: String, bytes: &[u8]) {
    let payload_path = state.logs_dir.join(PAYLOADS_DIR_NAME).join(file_name);
    if let Err(err) = fs::write(&payload_path, bytes).await {
        warn!("failed to write payload dump {}: {err}", payload_path.display());
    }
}

pub(crate) async fn write_raw_payload_dump(
    state: &AppState,
    request_id: &str,
    label: &str,
    bytes: &[u8],
) {
    if !state.log_full_body {
        return;
    }
    write_dump_bytes(state, format!("{request_id}-{label}"), bytes).await;
}

pub(crate) async fn write_payload_dump(
    state: &AppState,
    request_id: &str,
    label: &str,
    value: &Value,
) {
    if !state.log_full_body {
        return;
    }
    let serialized = match serde_json::to_vec_pretty(value) {
        Ok(data) => data,
        Err(err) => {
            warn!("failed to serialize payload dump {request_id}/{label}: {err}");
            return;
        }
    };
    write_dump_bytes(state, format!("{request_id}-{label}.json"), &serialized).await;
}

pub(crate) fn top_level_keys(body: &Value) -> Vec<String> {
    body.as_object()
        .map(|object| {
            let mut keys = object.keys().cloned().collect::<Vec<_>>();
            keys.sort_unstable();
            keys
        })
        .unwrap_or_default()
}

pub(crate) fn summarize_client_payload(body: &Value) -> Value {
    if let Some(object) = body.as_object() {
        let mut summary = serde_json::Map::new();
        summary.insert(
            "top_level_keys".to_string(),
            Value::Array(
                top_level_keys(body)
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );

        if let Some(messages) = object.get("messages").and_then(Value::as_array) {
            summary.insert("messages_len".to_string(), json!(messages.len()));
            summary.insert(
                "messages_roles".to_string(),
                Value::Array(
                    messages
                        .iter()
                        .filter_map(|message| message.get("role").and_then(Value::as_str))
                        .map(|role| Value::String(role.to_string()))
                        .collect(),
                ),
            );
        }

        if let Some(input) = object.get("input").and_then(Value::as_array) {
            summary.insert("input_len".to_string(), json!(input.len()));
            summary.insert(
                "input_kinds".to_string(),
                Value::Array(
                    input
                        .iter()
                        .map(|item| {
                            if let Some(item_type) = item.get("type").and_then(Value::as_str) {
                                Value::String(format!("type:{item_type}"))
                            } else if let Some(role) = item.get("role").and_then(Value::as_str) {
                                Value::String(format!("role:{role}"))
                            } else {
                                Value::String("item".to_string())
                            }
                        })
                        .take(32)
                        .collect(),
                ),
            );
        }

        return Value::Object(summary);
    }

    match body {
        Value::Null => json!({ "kind": "null" }),
        Value::Array(items) => json!({ "kind": "array", "length": items.len() }),
        Value::String(_) => json!({ "kind": "string" }),
        Value::Bool(_) => json!({ "kind": "boolean" }),
        Value::Number(_) => json!({ "kind": "number" }),
        Value::Object(_) => Value::Null,
    }
}

pub(crate) async fn log_request_event(state: &AppState, file_name: &str, record: Value) {
    write_jsonl_log(state, file_name, &record).await;
}

pub(crate) async fn log_json_request(
    state: &AppState,
    context: Option<&RequestContext>,
    headers: Option<&HeaderMap>,
    phase: &str,
    body: &Value,
    sanitized: Option<&Value>,
) {
    let request_id = context
        .map(|context| context.request_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let body_bytes = serde_json::to_vec(body)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let record = json!({
        "ts": crate::files::now_unix(),
        "phase": phase,
        "request_id": request_id,
        "method": context.map(|context| context.method.as_str()).unwrap_or("POST"),
        "path": context.map(|context| context.path.as_str()).unwrap_or(""),
        "route_kind": context.map(|context| context.route_kind).unwrap_or("other"),
        "content_type": context.and_then(|context| context.content_type.clone()).unwrap_or_else(|| "application/json".to_string()),
        "body_bytes": body_bytes,
        "top_level_keys": top_level_keys(body),
        "payload_summary": summarize_client_payload(body),
        "client_ip": context.and_then(|context| context.client_ip.clone()),
        "client_request_id": context.and_then(|context| context.client_request_id.clone()),
        "headers": headers.map(redacted_headers),
        "sanitized_top_level_keys": sanitized.map(top_level_keys),
    });
    log_request_event(state, REQUESTS_LOG_FILE, record).await;
    write_payload_dump(state, &request_id, "raw-client-body", body).await;
    if let Some(sanitized_body) = sanitized {
        write_payload_dump(
            state,
            &request_id,
            "sanitized-upstream-body",
            sanitized_body,
        )
        .await;
    }
}

pub(crate) async fn log_unmatched_request(
    state: &AppState,
    request_context: Option<&RequestContext>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let request_id = request_context
        .map(|context| context.request_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let method = request_context
        .map(|context| context.method.to_string())
        .unwrap_or_else(|| req.method().to_string());
    let path = request_context
        .map(|context| context.path.clone())
        .unwrap_or_else(|| req.uri().path().to_string());
    let content_type = request_context
        .and_then(|context| context.content_type.clone())
        .or_else(|| {
            req.headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
        });
    let route_kind = classify_pathname(&path);

    let (parts, body) = req.into_parts();
    let headers_snapshot = redacted_headers(&parts.headers);
    let body_bytes = axum::body::to_bytes(body, 8 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let raw_text = String::from_utf8_lossy(&body_bytes);
    let parsed_json = serde_json::from_slice::<Value>(&body_bytes).ok();
    let record = json!({
        "ts": crate::files::now_unix(),
        "phase": "unmatched_request",
        "request_id": request_id,
        "method": method,
        "path": path,
        "route_kind": route_kind,
        "content_type": content_type,
        "body_bytes": body_bytes.len(),
        "top_level_keys": parsed_json.as_ref().map(top_level_keys),
        "payload_summary": parsed_json.as_ref().map(summarize_client_payload),
        "text_preview": if parsed_json.is_none() && !raw_text.is_empty() {
            Some(raw_text.chars().take(512).collect::<String>())
        } else {
            None::<String>
        },
        "client_ip": request_context.and_then(|context| context.client_ip.clone()),
        "client_request_id": request_context.and_then(|context| context.client_request_id.clone()),
        "headers": headers_snapshot,
    });
    log_request_event(state, REQUESTS_LOG_FILE, record).await;
    if let Some(json_body) = parsed_json.as_ref() {
        write_payload_dump(state, &request_id, "raw-client-body", json_body).await;
    } else if state.log_full_body && !body_bytes.is_empty() {
        write_raw_payload_dump(state, &request_id, "raw-client-body.bin", &body_bytes).await;
    }

    let mut response = crate::errors::json_error(
        axum::http::StatusCode::NOT_FOUND,
        &format!("Unsupported route: {path}. Check proxy request logs for diagnostics."),
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(crate::config::REQUEST_ID_HEADER, value);
    }
    response
}
