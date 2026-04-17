use anyhow::Result;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::config::{DEFAULT_INSTRUCTIONS, SUPPORTED_MODELS};
use crate::errors::json_error;
use crate::state::{AppState, ClientStreamOptions};

#[allow(clippy::result_large_err)]
pub(crate) async fn normalize_responses_payload(
    state: &AppState,
    payload: &mut Value,
) -> std::result::Result<(), Response> {
    let Some(object) = payload.as_object_mut() else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Request body must be a JSON object",
        ));
    };

    apply_model_alias(&state.model_aliases, object);
    validate_supported_model(object)?;
    map_openai_compat_fields(object);

    let has_messages = object.get("messages").map(Value::is_array).unwrap_or(false);
    let has_input = object.get("input").is_some();
    if has_input {
        normalize_responses_input_shape(object);
        if let Some(input) = object.get_mut("input") {
            normalize_input_items(state, input)
                .await
                .map_err(normalize_input_error_response)?;
        }
        object.remove("messages");
    } else if has_messages {
        let messages = object
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (input, instructions) = messages_to_input(state, &messages)
            .await
            .map_err(normalize_input_error_response)?;
        object.insert("input".to_string(), Value::Array(input));
        if !instructions.is_empty() {
            merge_instructions(object, instructions);
        }
        object.remove("messages");
    } else {
        object.insert("input".to_string(), Value::Array(vec![]));
    }

    promote_instruction_items(object);
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    ensure_instructions(object);
    sanitize_upstream_payload(object);
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(crate) async fn normalize_chat_payload(
    state: &AppState,
    payload: &mut Value,
) -> std::result::Result<ClientStreamOptions, Response> {
    let Some(object) = payload.as_object_mut() else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Request body must be a JSON object",
        ));
    };

    apply_model_alias(&state.model_aliases, object);
    validate_supported_model(object)?;
    let client_options = extract_client_stream_options(object);
    map_openai_compat_fields(object);

    let has_messages = object.get("messages").map(Value::is_array).unwrap_or(false);
    let has_input = object.get("input").is_some();
    if has_messages {
        let messages = object
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (input, instructions) = messages_to_input(state, &messages)
            .await
            .map_err(normalize_input_error_response)?;
        object.insert("input".to_string(), Value::Array(input));
        if !instructions.is_empty() {
            merge_instructions(object, instructions);
        }
        object.remove("messages");
    } else if has_input {
        normalize_responses_input_shape(object);
        if let Some(input) = object.get_mut("input") {
            normalize_input_items(state, input)
                .await
                .map_err(normalize_input_error_response)?;
        }
    } else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "chat/completions requires either messages or input",
        ));
    }

    promote_instruction_items(object);
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    ensure_instructions(object);
    sanitize_upstream_payload(object);
    Ok(client_options)
}

pub(crate) async fn messages_to_input(
    state: &AppState,
    messages: &[Value],
) -> Result<(Vec<Value>, String)> {
    let mut input = Vec::<Value>::new();
    let mut system_chunks = Vec::<String>::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content");
        match role {
            "system" | "developer" => {
                if let Some(Value::String(text)) = content {
                    system_chunks.push(text.clone());
                } else if let Some(Value::Array(parts)) = content {
                    for part in parts {
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("text") | Some("input_text")
                        ) {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                system_chunks.push(text.to_string());
                            }
                        }
                    }
                }
            }
            "user" => {
                let normalized = normalize_content_parts(state, content, "user").await?;
                input.push(json!({"role":"user","content":normalized}));
            }
            "assistant" => {
                let tool_calls = message.get("tool_calls").and_then(Value::as_array);
                let has_content = match content {
                    Some(Value::String(value)) => !value.is_empty(),
                    Some(Value::Array(parts)) => !parts.is_empty(),
                    _ => false,
                };
                if has_content {
                    let normalized =
                        normalize_content_parts(state, content, "assistant").await?;
                    input.push(json!({"role":"assistant","content":normalized}));
                } else if tool_calls.is_none() {
                    input.push(json!({
                        "role":"assistant",
                        "content":[{"type":"output_text","text":""}]
                    }));
                }
                if let Some(tool_calls) = tool_calls {
                    for tool_call in tool_calls {
                        input.push(json!({
                            "type":"function_call",
                            "name": tool_call.get("function").and_then(|function| function.get("name")).and_then(Value::as_str).unwrap_or(""),
                            "arguments": tool_call.get("function").and_then(|function| function.get("arguments")).cloned().unwrap_or(Value::String("{}".into())),
                            "call_id": tool_call.get("id").and_then(Value::as_str).unwrap_or("")
                        }));
                    }
                }
            }
            "tool" => {
                input.push(json!({
                    "type":"function_call_output",
                    "call_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "output": message.get("content").cloned().unwrap_or(Value::String("".to_string()))
                }));
            }
            _ => {}
        }
    }
    Ok((input, system_chunks.join("\n\n")))
}

pub(crate) async fn normalize_content_parts(
    state: &AppState,
    content: Option<&Value>,
    role: &str,
) -> Result<Vec<Value>> {
    let is_assistant = role == "assistant";
    let text_type = if is_assistant { "output_text" } else { "input_text" };
    let mut out = Vec::<Value>::new();
    match content {
        Some(Value::String(text)) => {
            out.push(json!({"type":text_type,"text":text}));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
                match part_type {
                    "text" | "input_text" | "output_text" => {
                        let keep_type = if is_assistant && part_type == "output_text" {
                            "output_text"
                        } else {
                            text_type
                        };
                        out.push(json!({
                            "type":keep_type,
                            "text":part.get("text").and_then(Value::as_str).unwrap_or("")
                        }));
                    }
                    "refusal" if is_assistant => {
                        out.push(json!({
                            "type":"refusal",
                            "refusal":part.get("refusal").and_then(Value::as_str).unwrap_or("")
                        }));
                    }
                    "image_url" => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| value.get("url").or(Some(value)))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !url.is_empty() {
                            out.push(json!({"type":"input_image","image_url":url}));
                        }
                    }
                    "input_image" => {
                        if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
                            let (data_url, _) =
                                crate::files::resolve_file_id_to_data_url(state, file_id).await?;
                            let mut part = part.clone();
                            if let Some(object) = part.as_object_mut() {
                                object.remove("file_id");
                                object.insert("image_url".to_string(), Value::String(data_url));
                            }
                            out.push(part);
                        } else {
                            out.push(part.clone());
                        }
                    }
                    "file" => {
                        let file = part.get("file").cloned().unwrap_or_else(|| json!({}));
                        if let Some(data) = file.get("file_data").and_then(Value::as_str) {
                            out.push(json!({
                                "type":"input_file",
                                "file_data": data,
                                "filename": file.get("filename").and_then(Value::as_str).unwrap_or("upload.bin")
                            }));
                        } else if let Some(file_url) = file.get("file_url").and_then(Value::as_str)
                        {
                            out.push(json!({
                                "type":"input_file",
                                "file_url": file_url,
                                "filename": file.get("filename").and_then(Value::as_str).unwrap_or("upload.bin")
                            }));
                        } else if let Some(file_id) = file.get("file_id").and_then(Value::as_str) {
                            let resolved =
                                crate::files::resolve_file_id_to_data_url(state, file_id).await?;
                            out.push(json!({
                                "type":"input_file",
                                "file_data": resolved.0,
                                "filename": resolved.1
                            }));
                        }
                    }
                    "input_file" => {
                        if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
                            let resolved =
                                crate::files::resolve_file_id_to_data_url(state, file_id).await?;
                            out.push(json!({
                                "type":"input_file",
                                "file_data": resolved.0,
                                "filename": resolved.1
                            }));
                        } else {
                            out.push(part.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {
            out.push(json!({"type":text_type,"text":""}));
        }
    }

    if out.is_empty() {
        out.push(json!({"type":text_type,"text":""}));
    }
    Ok(out)
}

pub(crate) async fn normalize_input_items(state: &AppState, input: &mut Value) -> Result<()> {
    let Some(items) = input.as_array_mut() else {
        return Ok(());
    };
    for item in items {
        if !item.is_object() {
            *item = match item.clone() {
                Value::String(text) => json!({
                    "role":"user",
                    "content":[{"type":"input_text","text":text}]
                }),
                other => json!({
                    "role":"user",
                    "content":[{"type":"input_text","text":other.to_string()}]
                }),
            };
        }

        if let Some(role) = item.get("role").and_then(Value::as_str).map(ToOwned::to_owned) {
            let text_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            if let Some(content) = item.get("content").cloned() {
                match content {
                    Value::Array(_) => {
                        let normalized =
                            normalize_content_parts(state, Some(&content), &role).await?;
                        item["content"] = Value::Array(normalized);
                    }
                    Value::String(_) => {}
                    Value::Null => {
                        item["content"] = Value::Array(vec![json!({
                            "type":text_type,
                            "text":""
                        })]);
                    }
                    other => {
                        item["content"] = Value::Array(vec![json!({
                            "type":text_type,
                            "text":other.to_string()
                        })]);
                    }
                }
            } else if matches!(role.as_str(), "user" | "assistant") {
                item["content"] = Value::Array(vec![json!({
                    "type":text_type,
                    "text":""
                })]);
            }
        }

        if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_file") => {
                        if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
                            let (data, filename) =
                                crate::files::resolve_file_id_to_data_url(state, file_id).await?;
                            part["file_data"] = Value::String(data);
                            part.as_object_mut().map(|object| object.remove("file_id"));
                            if part.get("filename").is_none() {
                                part["filename"] = Value::String(filename);
                            }
                        }
                    }
                    Some("input_image") => {
                        if let Some(file_id) = part.get("file_id").and_then(Value::as_str) {
                            let (data, _) =
                                crate::files::resolve_file_id_to_data_url(state, file_id).await?;
                            part["image_url"] = Value::String(data);
                            part.as_object_mut().map(|object| object.remove("file_id"));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn apply_model_alias(
    aliases: &std::collections::HashMap<String, String>,
    object: &mut serde_json::Map<String, Value>,
) {
    let Some(model) = object.get("model").and_then(Value::as_str) else {
        return;
    };
    if let Some(canonical) = aliases.get(model) {
        object.insert("model".to_string(), Value::String(canonical.clone()));
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn validate_supported_model(
    object: &serde_json::Map<String, Value>,
) -> std::result::Result<(), Response> {
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if SUPPORTED_MODELS.contains(&model.as_str()) {
        return Ok(());
    }
    Err(json_error(
        StatusCode::BAD_REQUEST,
        &format!(
            "Unsupported model. Supported: {}",
            SUPPORTED_MODELS.join(", ")
        ),
    ))
}

pub(crate) fn normalize_input_error_response(_: anyhow::Error) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "A referenced file_id is unknown, deleted, or expired",
    )
}

pub(crate) fn extract_client_stream_options(
    object: &serde_json::Map<String, Value>,
) -> ClientStreamOptions {
    let include_usage = object
        .get("stream_options")
        .and_then(|value| value.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ClientStreamOptions { include_usage }
}

pub(crate) fn merge_instructions(
    object: &mut serde_json::Map<String, Value>,
    instructions: String,
) {
    if instructions.trim().is_empty() {
        return;
    }
    if let Some(existing) = object.get("instructions").and_then(Value::as_str) {
        if !existing.trim().is_empty() {
            object.insert(
                "instructions".to_string(),
                Value::String(format!("{existing}\n\n{instructions}")),
            );
            return;
        }
    }
    object.insert("instructions".to_string(), Value::String(instructions));
}

pub(crate) fn content_to_instruction_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") | Some("input_text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => other.to_string(),
    }
}

pub(crate) fn promote_instruction_items(object: &mut serde_json::Map<String, Value>) {
    let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut instruction_chunks = Vec::<String>::new();
    items.retain(|item| {
        let role = item.get("role").and_then(Value::as_str);
        if matches!(role, Some("system") | Some("developer")) {
            if let Some(content) = item.get("content") {
                let text = content_to_instruction_text(content);
                if !text.trim().is_empty() {
                    instruction_chunks.push(text);
                }
            }
            return false;
        }
        true
    });

    if !instruction_chunks.is_empty() {
        merge_instructions(object, instruction_chunks.join("\n\n"));
    }
}

fn is_allowed_upstream_field(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "input"
            | "instructions"
            | "tools"
            | "tool_choice"
            | "store"
            | "include"
            | "stream"
            | "reasoning"
            | "temperature"
            | "top_p"
            | "max_output_tokens"
            | "truncation"
            | "text"
            | "parallel_tool_calls"
            | "previous_response_id"
            | "service_tier"
    )
}

pub(crate) fn sanitize_upstream_payload(object: &mut serde_json::Map<String, Value>) {
    object.retain(|key, _| is_allowed_upstream_field(key));
}

pub(crate) fn ensure_instructions(object: &mut serde_json::Map<String, Value>) {
    let current = object
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if current.is_empty() {
        let default_instructions = std::env::var("CODEX_DEFAULT_INSTRUCTIONS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.to_string());
        object.insert(
            "instructions".to_string(),
            Value::String(default_instructions),
        );
    }
}

pub(crate) fn map_openai_compat_fields(object: &mut serde_json::Map<String, Value>) {
    let dropped: Vec<&str> = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .into_iter()
        .filter(|field| object.remove(*field).is_some())
        .collect();
    if !dropped.is_empty() {
        tracing::debug!("dropped unsupported upstream token-cap fields: {:?}", dropped);
    }
    // Codex upstream rejects service_tier values other than "priority"
    // ("400 Unsupported service_tier"). Match CLIProxyAPI: drop anything else
    // so omit-or-priority is the only shape forwarded.
    let current_tier = object
        .get("service_tier")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(tier) = current_tier
        && tier != "priority"
    {
        object.remove("service_tier");
        tracing::debug!("dropped unsupported service_tier: {tier}");
    }
    if object.get("reasoning").is_none() {
        if let Some(value) = object.remove("reasoning_effort") {
            if let Some(effort) = value.as_str() {
                object.insert("reasoning".to_string(), json!({ "effort": effort }));
            }
        }
    }
    if object.get("text").is_none() {
        if let Some(value) = object.remove("response_format") {
            object.insert("text".to_string(), json!({"format": value}));
        }
    }
}

pub(crate) fn normalize_responses_input_shape(object: &mut serde_json::Map<String, Value>) {
    if object.get("input").is_none() {
        object.insert("input".to_string(), Value::Array(vec![]));
        return;
    }

    let current = object.get("input").cloned().unwrap_or(Value::Null);
    match current {
        Value::String(text) => {
            object.insert(
                "input".to_string(),
                json!([{
                    "role":"user",
                    "content":[{"type":"input_text","text":text}]
                }]),
            );
        }
        Value::Array(_) => {}
        other => {
            object.insert(
                "input".to_string(),
                json!([{
                    "role":"user",
                    "content":[{"type":"input_text","text":other.to_string()}]
                }]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use serde_json::{Value, json};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tokio::fs;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::{map_openai_compat_fields, normalize_responses_payload, promote_instruction_items};
    use crate::files::init_db;
    use crate::state::AppState;

    async fn test_state() -> Result<AppState> {
        let root = std::env::temp_dir().join(format!("codex-proxy-norm-{}", Uuid::new_v4()));
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
            upstream_url: String::new(),
            upstream_identity_encoding: false,
            static_api_key: None,
            log_full_body: false,
            log_write_lock: Arc::new(Mutex::new(())),
            model_aliases: Arc::new(std::collections::HashMap::new()),
            auth: Arc::new(crate::codex_auth::CodexAuth::for_tests()),
        })
    }

    #[test]
    fn map_openai_fields_rewrites_reasoning_and_drops_token_caps() {
        let mut payload = serde_json::Map::new();
        payload.insert("max_completion_tokens".to_string(), json!(321));
        payload.insert("reasoning_effort".to_string(), json!("high"));
        payload.insert("response_format".to_string(), json!({"type":"json_schema"}));

        map_openai_compat_fields(&mut payload);

        assert_eq!(payload.get("max_output_tokens"), None);
        assert_eq!(payload.get("max_completion_tokens"), None);
        assert_eq!(payload.get("reasoning"), Some(&json!({"effort":"high"})));
        assert_eq!(
            payload.get("text"),
            Some(&json!({"format":{"type":"json_schema"}}))
        );
    }

    #[test]
    fn apply_model_alias_rewrites_known_alias() {
        let mut aliases = std::collections::HashMap::new();
        aliases.insert(
            "gpt-5.3-codex-spark-preview".to_string(),
            "gpt-5.3-codex-spark".to_string(),
        );
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.3-codex-spark-preview"));

        super::apply_model_alias(&aliases, &mut payload);

        assert_eq!(payload.get("model"), Some(&json!("gpt-5.3-codex-spark")));
    }

    #[test]
    fn apply_model_alias_passes_through_unknown() {
        let aliases = std::collections::HashMap::new();
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.4"));

        super::apply_model_alias(&aliases, &mut payload);

        assert_eq!(payload.get("model"), Some(&json!("gpt-5.4")));
    }

    #[test]
    fn service_tier_non_priority_is_dropped() {
        let mut payload = serde_json::Map::new();
        payload.insert("service_tier".to_string(), json!("default"));
        map_openai_compat_fields(&mut payload);
        assert!(!payload.contains_key("service_tier"));

        let mut payload = serde_json::Map::new();
        payload.insert("service_tier".to_string(), json!("priority"));
        map_openai_compat_fields(&mut payload);
        assert_eq!(payload.get("service_tier"), Some(&json!("priority")));
    }

    #[test]
    fn sanitize_drops_fields_rejected_by_chatgpt_backend() {
        // Upstream chatgpt.com/backend-api/codex/responses returns
        // `400 Unsupported parameter: <name>` for these fields even though
        // Cursor emits them per OpenAI Chat Completions canonical spec.
        // Confirmed by live replay 2026-04-17.
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.4"));
        payload.insert("metadata".to_string(), json!({"workspace":"test"}));
        payload.insert("prompt_cache_retention".to_string(), json!("24h"));
        payload.insert("prompt_cache_key".to_string(), json!("cursor-session-42"));

        super::sanitize_upstream_payload(&mut payload);

        assert!(!payload.contains_key("metadata"));
        assert!(!payload.contains_key("prompt_cache_retention"));
        assert!(!payload.contains_key("prompt_cache_key"));
    }

    #[tokio::test]
    async fn assistant_output_text_preserved_through_responses_normalization() -> Result<()> {
        let state = test_state().await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":[
                {"role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"output_text","text":"hello"}]},
                {"role":"user","content":[{"type":"input_text","text":"again"}]}
            ]
        });

        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;

        let Some(input) = payload.get("input").and_then(Value::as_array) else {
            panic!("payload should have input array");
        };
        let Some(assistant) = input
            .iter()
            .find(|item| item.get("role").and_then(Value::as_str) == Some("assistant"))
        else {
            panic!("assistant item missing");
        };
        assert_eq!(
            assistant["content"][0]["type"],
            json!("output_text"),
            "assistant content must keep output_text"
        );
        Ok(())
    }

    #[test]
    fn promote_instruction_items_merges_system_and_developer_messages() {
        let mut payload = json!({
            "input": [
                {"role":"system","content":"System rule"},
                {"role":"developer","content":[{"type":"input_text","text":"Developer rule"}]},
                {"role":"user","content":"Hello"}
            ]
        });

        let Some(object) = payload.as_object_mut() else {
            panic!("payload should be an object");
        };
        promote_instruction_items(object);

        assert_eq!(
            payload.get("instructions"),
            Some(&json!("System rule\n\nDeveloper rule"))
        );
        assert_eq!(payload["input"].as_array().map(Vec::len), Some(1));
    }
}
