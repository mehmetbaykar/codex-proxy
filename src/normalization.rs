use anyhow::Result;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::config::SUPPORTED_MODELS;
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
    map_legacy_chat_tool_fields(object);
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
    ensure_reasoning_encrypted_content(object);
    ensure_parallel_tool_calls(object);
    map_web_search_options_to_tool(object);
    sanitize_include_values(object);
    sanitize_tool_types(object);
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
    map_legacy_chat_tool_fields(object);
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
    ensure_reasoning_encrypted_content(object);
    ensure_parallel_tool_calls(object);
    map_web_search_options_to_tool(object);
    sanitize_include_values(object);
    sanitize_tool_types(object);
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
                    let normalized = normalize_content_parts(state, content, "assistant").await?;
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
    let text_type = if is_assistant {
        "output_text"
    } else {
        "input_text"
    };
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
                        let image_url_value = part.get("image_url");
                        let url = image_url_value
                            .and_then(|value| value.get("url").or(Some(value)))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let detail = image_url_value
                            .and_then(|value| value.get("detail"))
                            .and_then(Value::as_str)
                            .or_else(|| part.get("detail").and_then(Value::as_str));
                        if !url.is_empty() {
                            let mut normalized = json!({"type":"input_image","image_url":url});
                            if let Some(detail) = detail {
                                normalized["detail"] = Value::String(detail.to_string());
                            }
                            out.push(normalized);
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

        if let Some(role) = item
            .get("role")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        {
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
                    Value::String(text) => {
                        item["content"] = Value::Array(vec![json!({
                            "type":text_type,
                            "text":text
                        })]);
                    }
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
    // Derived empirically via scripts/probe/probe.sh (see scripts/probe/matrix.tsv,
    // 2026-04-18 vs gpt-5.4). Every key here was observed to pass backend validation;
    // canonical OpenAI fields not listed (temperature, top_p, truncation, max_*_tokens,
    // n, seed, stop, logprobs, user, safety_identifier, metadata, stream_options,
    // frequency_penalty, presence_penalty, modalities, audio, background, ...) return
    // `400 {"detail":"Unsupported parameter: X"}` from chatgpt.com/backend-api/codex.
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
            | "text"
            | "parallel_tool_calls"
            | "previous_response_id"
            | "prompt_cache_key"
            | "service_tier"
    )
}

pub(crate) fn sanitize_upstream_payload(object: &mut serde_json::Map<String, Value>) {
    object.retain(|key, _| is_allowed_upstream_field(key));
}

pub(crate) fn ensure_instructions(object: &mut serde_json::Map<String, Value>) {
    // Preserve any non-empty caller-supplied instructions. When missing/empty,
    // match the Codex TUI upstream contract of sending `instructions: ""` unless
    // the operator explicitly sets `CODEX_DEFAULT_INSTRUCTIONS`.
    let current_is_nonempty = object
        .get("instructions")
        .and_then(Value::as_str)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if current_is_nonempty {
        return;
    }
    let fallback = std::env::var("CODEX_DEFAULT_INSTRUCTIONS")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_default();
    object.insert("instructions".to_string(), Value::String(fallback));
}

pub(crate) fn ensure_parallel_tool_calls(object: &mut serde_json::Map<String, Value>) {
    if !object.contains_key("parallel_tool_calls") {
        object.insert("parallel_tool_calls".to_string(), Value::Bool(true));
    }
}

pub(crate) fn ensure_reasoning_encrypted_content(object: &mut serde_json::Map<String, Value>) {
    match object.get_mut("include") {
        Some(Value::Array(include)) => {
            let has_encrypted_reasoning = include
                .iter()
                .any(|value| value.as_str() == Some("reasoning.encrypted_content"));
            if !has_encrypted_reasoning {
                include.push(Value::String("reasoning.encrypted_content".to_string()));
            }
        }
        Some(_) => {
            object.insert(
                "include".to_string(),
                Value::Array(vec![Value::String(
                    "reasoning.encrypted_content".to_string(),
                )]),
            );
        }
        None => {
            object.insert(
                "include".to_string(),
                Value::Array(vec![Value::String(
                    "reasoning.encrypted_content".to_string(),
                )]),
            );
        }
    }
}

pub(crate) fn map_openai_compat_fields(object: &mut serde_json::Map<String, Value>) {
    let dropped: Vec<&str> = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .into_iter()
        .filter(|field| object.remove(*field).is_some())
        .collect();
    if !dropped.is_empty() {
        tracing::debug!(
            "dropped unsupported upstream token-cap fields: {:?}",
            dropped
        );
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
    // `reasoning.effort: "minimal"` is canonical OpenAI for gpt-5/o-series but
    // the ChatGPT/Codex backend rejects it with
    // `unsupported_value: supported are 'none','low','medium','high','xhigh'`.
    // Clamp to "low" so clients on the modern spec don't hit a 400.
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut)
        && reasoning.get("effort").and_then(Value::as_str) == Some("minimal")
    {
        reasoning.insert("effort".to_string(), Value::String("low".to_string()));
        tracing::debug!(
            "clamped reasoning.effort from 'minimal' to 'low' (not supported upstream)"
        );
    }
    // Only `text.format.type == "text"` is accepted by the backend. Both
    // `json_object` and `json_schema` (and the legacy `response_format` field
    // carrying those) cause 400. Drop silently so canonical OpenAI structured-
    // output requests degrade to plain text rather than erroring.
    if let Some(value) = object.remove("response_format") {
        let kind = value
            .as_object()
            .and_then(|inner| inner.get("type"))
            .and_then(Value::as_str);
        match kind {
            Some("text") => {
                if object.get("text").is_none() {
                    object.insert("text".to_string(), json!({"format": {"type": "text"}}));
                }
            }
            Some(other) => {
                tracing::debug!(
                    "dropped response_format.type='{other}' (upstream only accepts 'text')"
                );
            }
            None => {
                tracing::debug!("dropped response_format (no type field)");
            }
        }
    }
    if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
        let format_kind = text
            .get("format")
            .and_then(|format| format.get("type"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(kind) = format_kind
            && kind != "text"
        {
            tracing::debug!("dropped text.format.type='{kind}' (upstream only accepts 'text')");
            text.remove("format");
        }
        if text.is_empty() {
            object.remove("text");
        }
    }
}

/// Filter `include` array to the value set the ChatGPT/Codex backend accepts.
/// Anything else yields `400 invalid_value`.
pub(crate) fn sanitize_include_values(object: &mut serde_json::Map<String, Value>) {
    const SUPPORTED: &[&str] = &[
        "reasoning.encrypted_content",
        "file_search_call.results",
        "web_search_call.results",
        "web_search_call.action.sources",
        "message.input_image.image_url",
        "computer_call_output.output.image_url",
        "code_interpreter_call.outputs",
        "message.output_text.logprobs",
    ];
    let Some(include) = object.get_mut("include").and_then(Value::as_array_mut) else {
        return;
    };
    let mut dropped = Vec::<String>::new();
    include.retain(|value| match value.as_str() {
        Some(s) if SUPPORTED.contains(&s) => true,
        Some(other) => {
            dropped.push(other.to_string());
            false
        }
        None => {
            dropped.push(value.to_string());
            false
        }
    });
    if !dropped.is_empty() {
        tracing::debug!("dropped unsupported include values: {:?}", dropped);
    }
}

/// Filter `tools[].type` to the set gpt-5.4 accepts. Other types 400 with
/// `Tool 'X' is not supported`. Keeps custom function tools (`type:"function"`).
pub(crate) fn sanitize_tool_types(object: &mut serde_json::Map<String, Value>) {
    const SUPPORTED: &[&str] = &["function", "web_search", "image_generation"];
    let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    let mut dropped = Vec::<String>::new();
    tools.retain(|tool| {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or("");
        if SUPPORTED.contains(&kind) {
            true
        } else {
            dropped.push(kind.to_string());
            false
        }
    });
    if !dropped.is_empty() {
        tracing::debug!("dropped unsupported tool types: {:?}", dropped);
    }
    if tools.is_empty() {
        object.remove("tools");
    }
}

/// Canonical OpenAI spec has a top-level `web_search_options` field; upstream
/// rejects it as `Unsupported parameter` but DOES accept `{type:"web_search"}`
/// in the `tools` array. Migrate the field into a tool and drop the original.
pub(crate) fn map_web_search_options_to_tool(object: &mut serde_json::Map<String, Value>) {
    let Some(options) = object.remove("web_search_options") else {
        return;
    };
    let already_present = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
        });
    if already_present {
        tracing::debug!("dropped redundant web_search_options (tools already has web_search)");
        return;
    }
    let mut tool = serde_json::Map::new();
    tool.insert("type".to_string(), Value::String("web_search".to_string()));
    if let Some(obj) = options.as_object() {
        for (key, value) in obj {
            if matches!(key.as_str(), "search_context_size" | "user_location") {
                tool.insert(key.clone(), value.clone());
            }
        }
    }
    let tools = object
        .entry("tools".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = tools.as_array_mut() {
        arr.push(Value::Object(tool));
    }
}

pub(crate) fn map_legacy_chat_tool_fields(object: &mut serde_json::Map<String, Value>) {
    if object.get("tools").is_none()
        && let Some(Value::Array(functions)) = object.remove("functions")
    {
        let tools = functions
            .into_iter()
            .map(|function| {
                let mut tool = function;
                if let Some(tool_object) = tool.as_object_mut() {
                    tool_object
                        .entry("type".to_string())
                        .or_insert_with(|| Value::String("function".to_string()));
                }
                tool
            })
            .collect::<Vec<_>>();
        object.insert("tools".to_string(), Value::Array(tools));
    }

    if object.get("tool_choice").is_none()
        && let Some(function_call) = object.remove("function_call")
    {
        let tool_choice = match function_call {
            Value::String(value) => Value::String(value),
            Value::Object(mut value) => {
                if let Some(name) = value
                    .remove("name")
                    .and_then(|field| field.as_str().map(ToOwned::to_owned))
                {
                    json!({
                        "type": "function",
                        "name": name,
                    })
                } else {
                    Value::Object(value)
                }
            }
            other => other,
        };
        object.insert("tool_choice".to_string(), tool_choice);
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
    use anyhow::Result;
    use serde_json::{Value, json};

    use super::{
        map_legacy_chat_tool_fields, map_openai_compat_fields, normalize_responses_payload,
        promote_instruction_items,
    };
    use crate::test_support::test_state;

    #[test]
    fn map_openai_fields_rewrites_reasoning_and_drops_token_caps() {
        let mut payload = serde_json::Map::new();
        payload.insert("max_completion_tokens".to_string(), json!(321));
        payload.insert("reasoning_effort".to_string(), json!("high"));
        // json_schema is canonical OpenAI but backend 400s -> we drop silently
        payload.insert("response_format".to_string(), json!({"type":"json_schema"}));

        map_openai_compat_fields(&mut payload);

        assert_eq!(payload.get("max_output_tokens"), None);
        assert_eq!(payload.get("max_completion_tokens"), None);
        assert_eq!(payload.get("reasoning"), Some(&json!({"effort":"high"})));
        assert_eq!(
            payload.get("text"),
            None,
            "json_schema response_format must be dropped, not forwarded"
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
        // `400 {"detail":"Unsupported parameter: <name>"}` for these fields even
        // though Cursor emits them per OpenAI Chat Completions canonical spec.
        // Confirmed by scripts/probe/probe.sh (see scripts/probe/matrix.tsv) 2026-04-18.
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.4"));
        payload.insert("metadata".to_string(), json!({"workspace":"test"}));
        payload.insert("prompt_cache_retention".to_string(), json!("24h"));
        payload.insert("temperature".to_string(), json!(0.7));
        payload.insert("top_p".to_string(), json!(0.9));
        payload.insert("truncation".to_string(), json!("auto"));
        payload.insert("max_output_tokens".to_string(), json!(128));

        super::sanitize_upstream_payload(&mut payload);

        assert!(!payload.contains_key("metadata"));
        assert!(!payload.contains_key("prompt_cache_retention"));
        assert!(!payload.contains_key("temperature"));
        assert!(!payload.contains_key("top_p"));
        assert!(!payload.contains_key("truncation"));
        assert!(!payload.contains_key("max_output_tokens"));
    }

    #[test]
    fn sanitize_keeps_prompt_cache_key() {
        // Backend accepts `prompt_cache_key` (any reasonable length; internal
        // cap is 64 chars shared with the session_id header). Confirmed via
        // scripts/probe/probe.sh — earlier assumption it 400s was wrong.
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.4"));
        payload.insert("prompt_cache_key".to_string(), json!("cursor-session-42"));

        super::sanitize_upstream_payload(&mut payload);

        assert_eq!(
            payload.get("prompt_cache_key"),
            Some(&json!("cursor-session-42"))
        );
    }

    #[tokio::test]
    async fn assistant_output_text_preserved_through_responses_normalization() -> Result<()> {
        let state = test_state(String::new()).await?;
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

    #[tokio::test]
    async fn normalization_always_requests_reasoning_encrypted_content() -> Result<()> {
        let state = test_state(String::new()).await?;
        // Seed with a backend-accepted include value; `reasoning.encrypted_content`
        // must be present after normalization regardless of what the client sent.
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":"hello",
            "include":["web_search_call.action.sources"]
        });

        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;

        let include = payload["include"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("include should be an array"))?;
        assert!(
            include
                .iter()
                .any(|value| value.as_str() == Some("reasoning.encrypted_content")),
            "reasoning.encrypted_content must always be present"
        );
        assert!(
            include
                .iter()
                .any(|value| value.as_str() == Some("web_search_call.action.sources")),
            "client-supplied (accepted) include values must survive sanitization"
        );
        Ok(())
    }

    #[tokio::test]
    async fn normalization_drops_backend_rejected_include_values() -> Result<()> {
        // Backend returns `400 invalid_value` for `reasoning.summary` and `usage`
        // (not in the 8-value supported set). Probe: matrix.tsv.
        let state = test_state(String::new()).await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":"hello",
            "include":["reasoning.summary","usage"]
        });

        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;

        let include = payload["include"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("include should be an array"))?;
        assert!(
            include
                .iter()
                .all(|v| v.as_str() != Some("reasoning.summary"))
        );
        assert!(include.iter().all(|v| v.as_str() != Some("usage")));
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

    #[test]
    fn legacy_functions_and_function_call_map_to_tools_and_tool_choice() {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "functions".to_string(),
            json!([{ "name": "lookup", "description": "Lookup docs", "parameters": { "type": "object" } }]),
        );
        payload.insert("function_call".to_string(), json!({ "name": "lookup" }));

        map_legacy_chat_tool_fields(&mut payload);

        assert_eq!(payload["tools"][0]["type"], json!("function"));
        assert_eq!(payload["tools"][0]["name"], json!("lookup"));
        assert_eq!(
            payload["tool_choice"],
            json!({ "type": "function", "name": "lookup" })
        );
        assert!(payload.get("functions").is_none());
        assert!(payload.get("function_call").is_none());
    }

    #[tokio::test]
    async fn normalize_responses_input_items_converts_string_content_to_parts() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":[
                {"role":"user","content":"hello"},
                {"role":"assistant","content":"hi"}
            ]
        });

        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;

        assert_eq!(
            payload["input"][0]["content"][0],
            json!({"type":"input_text","text":"hello"})
        );
        assert_eq!(
            payload["input"][1]["content"][0],
            json!({"type":"output_text","text":"hi"})
        );
        Ok(())
    }

    #[tokio::test]
    async fn normalize_responses_preserves_image_detail() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "messages":[{
                "role":"user",
                "content":[{
                    "type":"image_url",
                    "image_url":{"url":"https://example.com/a.png","detail":"high"}
                }]
            }]
        });

        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;

        assert_eq!(
            payload["input"][0]["content"][0]["type"],
            json!("input_image")
        );
        assert_eq!(
            payload["input"][0]["content"][0]["image_url"],
            json!("https://example.com/a.png")
        );
        assert_eq!(payload["input"][0]["content"][0]["detail"], json!("high"));
        Ok(())
    }

    #[test]
    fn response_format_json_object_is_dropped() {
        let mut payload = serde_json::Map::new();
        payload.insert("response_format".to_string(), json!({"type":"json_object"}));
        map_openai_compat_fields(&mut payload);
        assert!(
            !payload.contains_key("response_format"),
            "response_format must not survive the mapping pass"
        );
        assert!(
            !payload.contains_key("text"),
            "json_object must not be forwarded as text.format — backend 400s"
        );
    }

    #[test]
    fn response_format_text_becomes_text_format() {
        let mut payload = serde_json::Map::new();
        payload.insert("response_format".to_string(), json!({"type":"text"}));
        map_openai_compat_fields(&mut payload);
        assert_eq!(
            payload.get("text"),
            Some(&json!({"format":{"type":"text"}}))
        );
    }

    #[test]
    fn reasoning_minimal_clamped_to_low() {
        let mut payload = serde_json::Map::new();
        payload.insert("reasoning".to_string(), json!({"effort":"minimal"}));
        map_openai_compat_fields(&mut payload);
        assert_eq!(payload.get("reasoning"), Some(&json!({"effort":"low"})));
    }

    #[tokio::test]
    async fn missing_instructions_becomes_empty_string() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({"model":"gpt-5.4","input":"hi"});
        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;
        assert_eq!(payload.get("instructions"), Some(&json!("")));
        Ok(())
    }

    #[tokio::test]
    async fn caller_supplied_instructions_preserved() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":"hi",
            "instructions":"Be terse."
        });
        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;
        assert_eq!(payload.get("instructions"), Some(&json!("Be terse.")));
        Ok(())
    }

    #[tokio::test]
    async fn parallel_tool_calls_forced_true_when_missing() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({"model":"gpt-5.4","input":"hi"});
        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;
        assert_eq!(payload.get("parallel_tool_calls"), Some(&json!(true)));
        Ok(())
    }

    #[tokio::test]
    async fn parallel_tool_calls_caller_false_preserved() -> Result<()> {
        let state = test_state(String::new()).await?;
        let mut payload = json!({
            "model":"gpt-5.4",
            "input":"hi",
            "parallel_tool_calls": false
        });
        normalize_responses_payload(&state, &mut payload)
            .await
            .map_err(|_| anyhow::anyhow!("normalization failed"))?;
        assert_eq!(payload.get("parallel_tool_calls"), Some(&json!(false)));
        Ok(())
    }

    #[test]
    fn include_filters_to_supported_values() -> Result<()> {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "include".to_string(),
            json!([
                "reasoning.encrypted_content",
                "reasoning.summary",
                "usage",
                "web_search_call.action.sources"
            ]),
        );
        super::sanitize_include_values(&mut payload);
        let include = payload["include"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("include should be an array"))?;
        let strings: Vec<&str> = include.iter().filter_map(Value::as_str).collect();
        assert!(strings.contains(&"reasoning.encrypted_content"));
        assert!(strings.contains(&"web_search_call.action.sources"));
        assert!(!strings.contains(&"reasoning.summary"));
        assert!(!strings.contains(&"usage"));
        Ok(())
    }

    #[test]
    fn tools_filters_to_supported_types() -> Result<()> {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "tools".to_string(),
            json!([
                {"type":"function","name":"lookup","parameters":{"type":"object"}},
                {"type":"file_search"},
                {"type":"code_interpreter"},
                {"type":"web_search"},
                {"type":"image_generation"},
                {"type":"apply_patch"}
            ]),
        );
        super::sanitize_tool_types(&mut payload);
        let tools = payload["tools"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tools should be an array"))?;
        let types: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("type").and_then(Value::as_str))
            .collect();
        assert_eq!(types, vec!["function", "web_search", "image_generation"]);
        Ok(())
    }

    #[test]
    fn web_search_options_injects_web_search_tool_and_drops_original() {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "web_search_options".to_string(),
            json!({"search_context_size":"high"}),
        );
        super::map_web_search_options_to_tool(&mut payload);
        assert!(
            !payload.contains_key("web_search_options"),
            "upstream 400s on web_search_options, must be dropped"
        );
        let tool = &payload["tools"][0];
        assert_eq!(tool["type"], json!("web_search"));
        assert_eq!(tool["search_context_size"], json!("high"));
    }

    #[test]
    fn web_search_options_no_op_when_tool_already_present() -> Result<()> {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "tools".to_string(),
            json!([{"type":"web_search","search_context_size":"low"}]),
        );
        payload.insert(
            "web_search_options".to_string(),
            json!({"search_context_size":"high"}),
        );
        super::map_web_search_options_to_tool(&mut payload);
        assert!(!payload.contains_key("web_search_options"));
        let tools = payload["tools"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tools should be an array"))?;
        assert_eq!(tools.len(), 1);
        Ok(())
    }

    #[test]
    fn allowlist_matches_probe_matrix() {
        // Invariant: every top-level field name appearing as `accept` in the
        // committed probe matrix must survive `sanitize_upstream_payload`, and
        // every `reject` row must be stripped. Regenerate via
        // `CODEX_PROBE_TOKEN=... scripts/probe/probe.sh`.
        let matrix = include_str!("../scripts/probe/matrix.tsv");
        // Probe rows describe compound field states (e.g. "tool_function" with
        // `tools:[...]`). This test focuses on the top-level field names we
        // forward verbatim to upstream, so map each probe name → the JSON key
        // we'd actually sanitize. Compound / value-specific probes are covered
        // by dedicated tests elsewhere.
        let top_level = |probe_name: &str| -> Option<&'static str> {
            match probe_name {
                "baseline" => None,
                "temperature" => Some("temperature"),
                "top_p" => Some("top_p"),
                "max_output_tokens" => Some("max_output_tokens"),
                "max_tokens" => Some("max_tokens"),
                "max_completion_tokens" => Some("max_completion_tokens"),
                "n" => Some("n"),
                "seed" => Some("seed"),
                "stop" => Some("stop"),
                "logprobs" => Some("logprobs"),
                "top_logprobs" => Some("top_logprobs"),
                "user" => Some("user"),
                "safety_identifier" => Some("safety_identifier"),
                "metadata" => Some("metadata"),
                "prompt_cache_key_short" | "prompt_cache_key_long" => Some("prompt_cache_key"),
                "parallel_tool_calls" => Some("parallel_tool_calls"),
                "truncation_auto" | "truncation_disabled" => Some("truncation"),
                "background" => Some("background"),
                "modalities" => Some("modalities"),
                "audio" => Some("audio"),
                "stream_options" => Some("stream_options"),
                "prompt_cache_retention" => Some("prompt_cache_retention"),
                "conversation" => Some("conversation"),
                "frequency_penalty" => Some("frequency_penalty"),
                "presence_penalty" => Some("presence_penalty"),
                "max_tool_calls" => Some("max_tool_calls"),
                "web_search_options" => Some("web_search_options"),
                "context_management" => Some("context_management"),
                _ => None, // value-specific / compound probes handled by other tests
            }
        };
        for line in matrix.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut fields = line.splitn(6, '\t');
            let probe_name = fields.next().unwrap_or("");
            let _status = fields.next().unwrap_or("");
            let verdict = fields.next().unwrap_or("");
            let Some(key) = top_level(probe_name) else {
                continue;
            };
            match verdict {
                "accept" => assert!(
                    super::is_allowed_upstream_field(key),
                    "probe matrix says `{key}` accepts but allowlist drops it"
                ),
                "reject" => assert!(
                    !super::is_allowed_upstream_field(key),
                    "probe matrix says `{key}` rejects but allowlist forwards it"
                ),
                _ => {}
            }
        }
    }
}
