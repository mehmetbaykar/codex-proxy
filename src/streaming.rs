use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::CURSOR_MAX_TOOL_CALL_ID_LEN;

#[derive(Default)]
pub(crate) struct ToolStreamState {
    item_id_to_output_index: HashMap<String, i64>,
    item_ids_with_arg_deltas: HashSet<String>,
    tool_call_aliases: HashMap<String, String>,
    fallback_tool_index: i64,
}

impl ToolStreamState {
    pub(crate) fn ensure_cursor_tool_call_id(
        &mut self,
        raw_ids: &[Option<&str>],
    ) -> Option<String> {
        let ids = raw_ids
            .iter()
            .flatten()
            .copied()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return None;
        }

        for id in &ids {
            if let Some(existing) = self.tool_call_aliases.get(*id) {
                let alias = existing.clone();
                for other in &ids {
                    self.tool_call_aliases
                        .insert((*other).to_string(), alias.clone());
                }
                return Some(alias);
            }
        }

        let preferred = ids
            .iter()
            .find(|candidate| candidate.len() <= CURSOR_MAX_TOOL_CALL_ID_LEN)
            .map(|candidate| (*candidate).to_string())
            .unwrap_or_else(|| {
                let digest = Sha256::digest(ids[0].as_bytes());
                let hex = format!("{digest:x}");
                format!("c{}", &hex[..CURSOR_MAX_TOOL_CALL_ID_LEN - 1])
            });
        for id in ids {
            self.tool_call_aliases
                .insert(id.to_string(), preferred.clone());
        }
        Some(preferred)
    }
}

pub(crate) fn chat_chunk_line(
    chunk_id: &str,
    created: i64,
    model: &str,
    delta: Value,
    finish_reason: Value,
) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    )
}

pub(crate) fn map_response_event_to_chat_chunk(
    event: &Value,
    tool_state: &mut ToolStreamState,
    chunk_id: &str,
    created: i64,
    model: &str,
) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    match event_type {
        "response.output_text.delta" => event.get("delta").and_then(Value::as_str).map(|delta| {
            chat_chunk_line(
                chunk_id,
                created,
                model,
                json!({ "content": delta }),
                Value::Null,
            )
        }),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            event.get("delta").and_then(Value::as_str).map(|delta| {
                chat_chunk_line(
                    chunk_id,
                    created,
                    model,
                    json!({ "reasoning_content": delta }),
                    Value::Null,
                )
            })
        }
        // The ChatGPT/Codex backend does NOT emit `response.refusal.delta`; refusal
        // content arrives as a `response.content_part.added` whose part.type is
        // "refusal". Surface that into `choices[].delta.refusal` so chat-completions
        // clients see the refusal mid-stream, matching OpenAI spec behavior.
        "response.content_part.added" => {
            let part = event.get("part")?;
            if part.get("type").and_then(Value::as_str) != Some("refusal") {
                return None;
            }
            let refusal = part.get("refusal").and_then(Value::as_str).unwrap_or("");
            Some(chat_chunk_line(
                chunk_id,
                created,
                model,
                json!({ "refusal": refusal }),
                Value::Null,
            ))
        }
        "response.output_item.added" => {
            let item = event.get("item")?;
            let item_type = item.get("type").and_then(Value::as_str)?;
            if !matches!(item_type, "function_call" | "custom_tool_call") {
                return None;
            }

            let item_id = item.get("id").and_then(Value::as_str);
            let call_id = item.get("call_id").and_then(Value::as_str);
            let cursor_id = tool_state.ensure_cursor_tool_call_id(&[call_id, item_id])?;
            let output_index = event
                .get("output_index")
                .and_then(Value::as_i64)
                .unwrap_or(tool_state.fallback_tool_index);
            if event.get("output_index").and_then(Value::as_i64).is_none() {
                tool_state.fallback_tool_index = output_index + 1;
            }
            if let Some(item_id) = item_id {
                tool_state
                    .item_id_to_output_index
                    .insert(item_id.to_string(), output_index);
            }
            if let Some(call_id) = call_id {
                tool_state
                    .item_id_to_output_index
                    .insert(call_id.to_string(), output_index);
            }
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("custom_tool");
            Some(chat_chunk_line(
                chunk_id,
                created,
                model,
                json!({
                    "tool_calls": [{
                        "index": output_index,
                        "id": cursor_id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" }
                    }]
                }),
                Value::Null,
            ))
        }
        "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
            let delta = event.get("delta").and_then(Value::as_str)?;
            let item_id = event.get("item_id").and_then(Value::as_str);
            if let Some(item_id) = item_id {
                tool_state
                    .item_ids_with_arg_deltas
                    .insert(item_id.to_string());
            }
            let output_index = event
                .get("output_index")
                .and_then(Value::as_i64)
                .or_else(|| {
                    item_id.and_then(|id| tool_state.item_id_to_output_index.get(id).copied())
                })
                .unwrap_or(0);
            Some(chat_chunk_line(
                chunk_id,
                created,
                model,
                json!({
                    "tool_calls": [{
                        "index": output_index,
                        "function": { "arguments": delta }
                    }]
                }),
                Value::Null,
            ))
        }
        "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
            let nested_item = event.get("item");
            let item_id = event.get("item_id").and_then(Value::as_str).or_else(|| {
                nested_item
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
            });
            let call_id = nested_item
                .and_then(|value| value.get("call_id"))
                .and_then(Value::as_str);
            let args = event
                .get("arguments")
                .and_then(Value::as_str)
                .or_else(|| event.get("input").and_then(Value::as_str))
                .or_else(|| {
                    nested_item
                        .and_then(|value| value.get("arguments"))
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    nested_item
                        .and_then(|value| value.get("input"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("");
            if args.is_empty() {
                return None;
            }
            let item_key = item_id?;
            if tool_state.item_ids_with_arg_deltas.contains(item_key) {
                return None;
            }
            let output_index = event
                .get("output_index")
                .and_then(Value::as_i64)
                .or_else(|| tool_state.item_id_to_output_index.get(item_key).copied())
                .unwrap_or(0);
            tool_state
                .item_id_to_output_index
                .insert(item_key.to_string(), output_index);
            let cursor_id = tool_state.ensure_cursor_tool_call_id(&[call_id, Some(item_key)])?;
            let name = event
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
                    nested_item
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("custom_tool");
            Some(chat_chunk_line(
                chunk_id,
                created,
                model,
                json!({
                    "tool_calls": [{
                        "index": output_index,
                        "id": cursor_id,
                        "type": "function",
                        "function": { "name": name, "arguments": args }
                    }]
                }),
                Value::Null,
            ))
        }
        _ => None,
    }
}

pub(crate) fn response_to_chat_message(output: Option<&Value>) -> Value {
    let mut text_chunks = Vec::<String>::new();
    let mut refusal_chunks = Vec::<String>::new();
    let mut tool_calls = Vec::<Value>::new();
    let mut annotations = Vec::<Value>::new();
    let mut tool_state = ToolStreamState::default();

    if let Some(items) = output.and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for part in content {
                            match part.get("type").and_then(Value::as_str) {
                                Some("output_text") => {
                                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                                        text_chunks.push(text.to_string());
                                    }
                                    // OpenAI chat.completion surfaces URL/file citations as
                                    // `message.annotations`. On this backend they live under
                                    // each output_text part; flatten across parts.
                                    if let Some(arr) =
                                        part.get("annotations").and_then(Value::as_array)
                                    {
                                        annotations.extend(arr.iter().cloned());
                                    }
                                }
                                Some("refusal") => {
                                    if let Some(text) = part.get("refusal").and_then(Value::as_str)
                                    {
                                        refusal_chunks.push(text.to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("function_call") | Some("custom_tool_call") => {
                    let call_id = item.get("call_id").and_then(Value::as_str);
                    let item_id = item.get("id").and_then(Value::as_str);
                    if let Some(cursor_id) =
                        tool_state.ensure_cursor_tool_call_id(&[call_id, item_id])
                    {
                        let args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("input").and_then(Value::as_str))
                            .unwrap_or("");
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("custom_tool");
                        tool_calls.push(json!({
                            "id": cursor_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args,
                            }
                        }));
                    }
                }
                _ => {}
            }
        }
    }

    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert(
        "content".to_string(),
        if text_chunks.is_empty() {
            if refusal_chunks.is_empty() {
                Value::Null
            } else {
                Value::String(refusal_chunks.join(""))
            }
        } else {
            Value::String(text_chunks.join(""))
        },
    );
    if !refusal_chunks.is_empty() {
        message.insert(
            "refusal".to_string(),
            Value::String(refusal_chunks.join("")),
        );
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    if !annotations.is_empty() {
        message.insert("annotations".to_string(), Value::Array(annotations));
    }
    Value::Object(message)
}

#[derive(Default)]
pub(crate) struct SseParser {
    buf: String,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let Some(index) = find_sse_delimiter(&self.buf) else {
                break;
            };
            let frame = self.buf[..index].to_string();
            let drain_len = if self.buf[index..].starts_with("\r\n\r\n") {
                4
            } else {
                2
            };
            self.buf.drain(..index + drain_len);
            if let Some(data) = parse_sse_frame_data(&frame) {
                out.push(data);
            }
        }
        out
    }
}

pub(crate) fn find_sse_delimiter(input: &str) -> Option<usize> {
    let lf = input.find("\n\n");
    let crlf = input.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub(crate) fn parse_sse_frame_data(frame: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in frame.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("event:") || trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            lines.push(rest.trim_start().to_string());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Accumulates per-item snapshots and text deltas observed during an SSE stream
/// so non-streaming aggregators can produce a complete `response.output[]` even
/// when the upstream terminal `response.completed` / `response.incomplete`
/// snapshot ships with an empty `output` array.
///
/// The ChatGPT/Codex backend emits fully-aggregated items via
/// `response.output_item.done` and streams message text via
/// `response.output_text.delta`, but the terminal snapshot's `output` is
/// observed empty in production. Without this accumulator, non-stream clients
/// get `message.content: null` on `/v1/chat/completions` and `output: []` on
/// `/v1/responses` despite tokens being generated.
#[derive(Default)]
pub(crate) struct ResponseOutputAccumulator {
    items: Vec<Value>,
    text_chunks: Vec<String>,
}

impl ResponseOutputAccumulator {
    pub(crate) fn observe(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    self.items.push(item.clone());
                }
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.text_chunks.push(delta.to_string());
                }
            }
            _ => {}
        }
    }

    /// If the snapshot already has a non-empty `output` array, leave it alone.
    /// Otherwise, fill from accumulated per-item snapshots; if those are also
    /// empty but text deltas arrived, synthesize a single message item.
    pub(crate) fn finalize(&self, snapshot: &mut Value) {
        let snapshot_has_output = snapshot
            .get("output")
            .and_then(Value::as_array)
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
        if snapshot_has_output {
            return;
        }

        let Some(object) = snapshot.as_object_mut() else {
            return;
        };

        if !self.items.is_empty() {
            object.insert("output".to_string(), Value::Array(self.items.clone()));
            return;
        }

        if !self.text_chunks.is_empty() {
            let synthetic = json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.text_chunks.join(""),
                }],
            });
            object.insert("output".to_string(), Value::Array(vec![synthetic]));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        map_response_event_to_chat_chunk, response_to_chat_message, ResponseOutputAccumulator,
        SseParser, ToolStreamState,
    };

    #[test]
    fn response_to_chat_message_includes_tool_calls() {
        let output = json!([
            {
                "type":"message",
                "content":[{"type":"output_text","text":"Done"}]
            },
            {
                "type":"function_call",
                "id":"call_abc123",
                "call_id":"call_abc123",
                "name":"Shell",
                "arguments":"{\"command\":\"pwd\"}"
            }
        ]);

        let message = response_to_chat_message(Some(&output));

        assert_eq!(message["role"], json!("assistant"));
        assert_eq!(message["content"], json!("Done"));
        assert_eq!(message["tool_calls"][0]["function"]["name"], json!("Shell"));
    }

    #[test]
    fn reasoning_delta_maps_to_reasoning_content() {
        let mut tool_state = ToolStreamState::default();
        let event = json!({
            "type":"response.reasoning_summary_text.delta",
            "delta":"thinking about it"
        });
        let line = map_response_event_to_chat_chunk(&event, &mut tool_state, "cmp-1", 1, "gpt-5.4");
        let Some(line) = line else {
            panic!("reasoning delta should produce a chunk");
        };
        assert!(
            line.contains("\"reasoning_content\":\"thinking about it\""),
            "chunk missing reasoning_content: {line}"
        );
    }

    #[test]
    fn sse_parser_extracts_data_frames() {
        let mut parser = SseParser::default();
        let events = parser.feed("event: message\ndata: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn response_to_chat_message_preserves_refusal() {
        let output = json!([
            {
                "type":"message",
                "content":[{"type":"refusal","refusal":"I cannot do that."}]
            }
        ]);

        let message = response_to_chat_message(Some(&output));

        assert_eq!(message["content"], json!("I cannot do that."));
        assert_eq!(message["refusal"], json!("I cannot do that."));
    }

    #[test]
    fn refusal_content_part_maps_to_chat_delta_refusal() {
        let mut tool_state = ToolStreamState::default();
        let event = json!({
            "type": "response.content_part.added",
            "part": {"type": "refusal", "refusal": "I can't do that."}
        });
        let Some(line) =
            map_response_event_to_chat_chunk(&event, &mut tool_state, "cmp-1", 1, "gpt-5.4")
        else {
            panic!("refusal content_part should yield a chunk");
        };
        assert!(
            line.contains("\"refusal\":\"I can't do that.\""),
            "chunk missing refusal delta: {line}"
        );
    }

    #[test]
    fn content_part_output_text_is_not_mapped_as_refusal() {
        // Non-refusal content_parts must not yield a chat chunk (text goes through output_text.delta).
        let mut tool_state = ToolStreamState::default();
        let event = json!({
            "type": "response.content_part.added",
            "part": {"type": "output_text", "text": ""}
        });
        assert!(
            map_response_event_to_chat_chunk(&event, &mut tool_state, "cmp-1", 1, "gpt-5.4")
                .is_none()
        );
    }

    #[test]
    fn accumulator_fills_output_from_item_done_when_snapshot_empty() {
        let mut accumulator = ResponseOutputAccumulator::default();
        accumulator.observe(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "content": [{"type": "output_text", "text": "Hello!"}]
            }
        }));
        let mut snapshot = json!({ "output": [] });
        accumulator.finalize(&mut snapshot);
        assert_eq!(snapshot["output"][0]["content"][0]["text"], json!("Hello!"));
        let message = response_to_chat_message(snapshot.get("output"));
        assert_eq!(message["content"], json!("Hello!"));
    }

    #[test]
    fn accumulator_synthesizes_message_from_deltas_when_no_items() {
        let mut accumulator = ResponseOutputAccumulator::default();
        for chunk in ["Hel", "lo!"] {
            accumulator.observe(&json!({
                "type": "response.output_text.delta",
                "delta": chunk
            }));
        }
        let mut snapshot = json!({});
        accumulator.finalize(&mut snapshot);
        let message = response_to_chat_message(snapshot.get("output"));
        assert_eq!(message["content"], json!("Hello!"));
    }

    #[test]
    fn accumulator_leaves_nonempty_snapshot_output_alone() {
        let mut accumulator = ResponseOutputAccumulator::default();
        accumulator.observe(&json!({
            "type": "response.output_text.delta",
            "delta": "stray"
        }));
        let mut snapshot = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "canonical"}]
            }]
        });
        accumulator.finalize(&mut snapshot);
        assert_eq!(
            snapshot["output"][0]["content"][0]["text"],
            json!("canonical")
        );
    }

    #[test]
    fn aggregate_message_annotations_preserved() {
        let output = json!([
            {
                "type":"message",
                "content":[{
                    "type":"output_text",
                    "text":"see source",
                    "annotations":[{"type":"url_citation","url":"https://example.com"}]
                }]
            }
        ]);
        let message = response_to_chat_message(Some(&output));
        assert_eq!(message["content"], json!("see source"));
        assert_eq!(
            message["annotations"][0]["url"],
            json!("https://example.com")
        );
    }
}
