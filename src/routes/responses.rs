use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::errors::{json_error, upstream_error_response, upstream_open_error_response};
use crate::logging::{log_json_request, log_request_event};
use crate::state::{AppState, ClientStreamOptions, RequestContext, WsSessionState};
use crate::streaming::{SseParser, ToolStreamState};
use crate::upstream::processed_success_headers;

pub(crate) async fn post_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    ctx: Option<axum::extract::Extension<RequestContext>>,
    body: Json<Value>,
) -> Response {
    let normalization_started_at = Instant::now();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let raw_body = body.0;
    let mut normalized = raw_body.clone();
    let request_context = ctx.as_ref().map(|extension| &extension.0);
    if let Err(response) = state
        .codex_adapter
        .normalize_responses_payload(&state, &mut normalized)
        .await
    {
        log_json_request(
            &state,
            request_context,
            Some(&headers),
            "client_in_invalid",
            &raw_body,
            None,
        )
        .await;
        return response;
    }
    let normalization_ms = normalization_started_at.elapsed().as_secs_f64() * 1000.0;
    log_json_request(
        &state,
        request_context,
        Some(&headers),
        "client_in",
        &raw_body,
        Some(&normalized),
    )
    .await;

    if stream {
        return proxy_sse_passthrough(&state, request_context, &headers, &normalized).await;
    }
    aggregate_responses(
        &state,
        request_context,
        &headers,
        &normalized,
        normalization_ms,
    )
    .await
}

pub(crate) async fn post_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    ctx: Option<axum::extract::Extension<RequestContext>>,
    body: Json<Value>,
) -> Response {
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let raw_body = body.0;
    let mut upstream_body = raw_body.clone();
    let request_context = ctx.as_ref().map(|extension| &extension.0);
    let client_options = match state
        .codex_adapter
        .normalize_chat_payload(&state, &mut upstream_body)
        .await
    {
        Ok(options) => options,
        Err(response) => {
            log_json_request(
                &state,
                request_context,
                Some(&headers),
                "client_in_invalid",
                &raw_body,
                None,
            )
            .await;
            return response;
        }
    };
    log_json_request(
        &state,
        request_context,
        Some(&headers),
        "client_in",
        &raw_body,
        Some(&upstream_body),
    )
    .await;

    if stream {
        return stream_chat_chunks(
            &state,
            request_context,
            &headers,
            &upstream_body,
            client_options,
        )
        .await;
    }
    aggregate_chat_completion(
        &state,
        request_context,
        &headers,
        &upstream_body,
        client_options,
    )
    .await
}

pub(crate) async fn ws_responses(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    ctx: Option<axum::extract::Extension<RequestContext>>,
) -> impl IntoResponse {
    let request_context = ctx.map(|extension| extension.0);
    ws.on_upgrade(move |socket| ws_session_loop(state, request_context, headers, socket))
}

async fn ws_session_loop(
    state: AppState,
    request_context: Option<RequestContext>,
    headers: HeaderMap,
    mut socket: WebSocket,
) {
    let session = Arc::new(Mutex::new(WsSessionState::default()));
    while let Some(message) = socket.recv().await {
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                tracing::warn!("websocket recv error: {err}");
                break;
            }
        };
        let Message::Text(text) = message else {
            continue;
        };
        let parsed = serde_json::from_str::<Value>(&text);
        let mut request = match parsed {
            Ok(value) => value,
            Err(_) => {
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"error","message":"Invalid JSON"})
                            .to_string()
                            .into(),
                    ))
                    .await;
                continue;
            }
        };
        let msg_type = request
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if msg_type != "response.create" && msg_type != "response.append" {
            let _ = socket
                .send(Message::Text(
                    json!({"type":"error","message":"unsupported websocket request type"})
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }

        {
            let session_state = session.lock().await;
            match prepare_ws_request(&session_state, request, &msg_type) {
                Ok(prepared) => request = prepared,
                Err(message) => {
                    let _ = socket
                        .send(Message::Text(
                            json!({"type":"error","message":message}).to_string().into(),
                        ))
                        .await;
                    continue;
                }
            }
        }

        request.as_object_mut().map(|object| object.remove("type"));

        if state
            .codex_adapter
            .normalize_responses_payload(&state, &mut request)
            .await
            .is_err()
        {
            let _ = socket
                .send(Message::Text(
                    json!({"type":"error","message":"invalid request payload"})
                        .to_string()
                        .into(),
                ))
                .await;
            continue;
        }

        log_json_request(
            &state,
            request_context.as_ref(),
            Some(&headers),
            "ws_client_in",
            &request,
            Some(&request),
        )
        .await;

        match state
            .codex_adapter
            .open_upstream_response(&state, request_context.as_ref(), &headers, &request)
            .await
        {
            Ok(response) => {
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::default();
                let mut last_output: Option<Value> = None;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(chunk) => {
                            let text = String::from_utf8_lossy(&chunk);
                            for event in parser.feed(&text) {
                                if event == "[DONE]" {
                                    continue;
                                }
                                if let Ok(value) = serde_json::from_str::<Value>(&event) {
                                    if value.get("type").and_then(Value::as_str)
                                        == Some("response.completed")
                                    {
                                        last_output = value
                                            .get("response")
                                            .and_then(|response| response.get("output"))
                                            .cloned();
                                    }
                                }
                                if socket.send(Message::Text(event.into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!("ws upstream stream error: {err}");
                            let _ = socket
                                .send(Message::Text(
                                    json!({"type":"error","message":"upstream stream failure"})
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                            break;
                        }
                    }
                }
                let mut session_state = session.lock().await;
                session_state.last_request = Some(request.clone());
                session_state.last_response_output = last_output;
            }
            Err(err) => {
                tracing::warn!("ws upstream start failed: {err}");
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"error","message":"failed to call upstream"})
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }
}

async fn proxy_sse_passthrough(
    state: &AppState,
    request_context: Option<&RequestContext>,
    headers: &HeaderMap,
    payload: &Value,
) -> Response {
    let response = match state
        .codex_adapter
        .open_upstream_response(state, request_context, headers, payload)
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!("upstream stream open failed: {err}");
            return upstream_open_error_response(&err);
        }
    };
    if !response.status().is_success() {
        return upstream_error_response(response).await;
    }
    let success_headers = processed_success_headers(response.headers());

    let payload_stream = response.bytes_stream();
    let out = async_stream::stream! {
        let mut parser = SseParser::default();
        futures::pin_mut!(payload_stream);
        while let Some(item) = payload_stream.next().await {
            match item {
                Ok(chunk) => {
                    let text = String::from_utf8_lossy(&chunk);
                    for event in parser.feed(&text) {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {event}\n\n")));
                    }
                }
                Err(err) => {
                    tracing::warn!("upstream sse read err: {err}");
                    break;
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(out));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response.headers_mut().extend(success_headers);
    response
}

async fn aggregate_responses(
    state: &AppState,
    request_context: Option<&RequestContext>,
    headers: &HeaderMap,
    payload: &Value,
    normalization_ms: f64,
) -> Response {
    let aggregate_started_at = Instant::now();
    let upstream_open_started_at = Instant::now();
    let (mut stream, success_headers) = match state
        .codex_adapter
        .open_upstream_response(state, request_context, headers, payload)
        .await
    {
        Ok(response) => {
            if !response.status().is_success() {
                return upstream_error_response(response).await;
            }
            let success_headers = processed_success_headers(response.headers());
            (
                response.bytes_stream(),
                success_headers,
            )
        }
        Err(err) => return upstream_open_error_response(&err),
    };
    let upstream_open_ms = upstream_open_started_at.elapsed().as_secs_f64() * 1000.0;
    let mut parser = SseParser::default();
    let mut chunk_count = 0usize;
    let mut event_count = 0usize;
    let mut bytes_received = 0usize;
    let mut first_chunk_ms: Option<f64> = None;
    let mut first_event_ms: Option<f64> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                chunk_count += 1;
                bytes_received += chunk.len();
                if first_chunk_ms.is_none() {
                    first_chunk_ms = Some(aggregate_started_at.elapsed().as_secs_f64() * 1000.0);
                }
                let text = String::from_utf8_lossy(&chunk);
                for event in parser.feed(&text) {
                    if event == "[DONE]" {
                        continue;
                    }
                    event_count += 1;
                    if first_event_ms.is_none() {
                        first_event_ms =
                            Some(aggregate_started_at.elapsed().as_secs_f64() * 1000.0);
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&event) {
                        if let Some(failure) = upstream_terminal_error_response(&value) {
                            return failure;
                        }
                        // `response.incomplete` is a valid terminal (e.g. hit max_output_tokens
                        // or safety cutoff). Upstream carries the partial response object with
                        // `status:"incomplete"` + `incomplete_details`; return it instead of 502.
                        if matches!(
                            value.get("type").and_then(Value::as_str),
                            Some("response.completed") | Some("response.incomplete")
                        ) {
                            if let Some(response) = value.get("response") {
                                let response_completed_ms =
                                    aggregate_started_at.elapsed().as_secs_f64() * 1000.0;
                                let end_to_end_ms = request_context
                                    .map(|context| {
                                        context.request_started_at.elapsed().as_secs_f64() * 1000.0
                                    })
                                    .unwrap_or(response_completed_ms);
                                log_request_event(
                                    state,
                                    crate::config::UPSTREAM_LOG_FILE,
                                    json!({
                                        "ts": crate::files::now_unix(),
                                        "phase": "responses_aggregate_complete",
                                        "request_id": request_context.map(|context| context.request_id.clone()),
                                        "path": request_context.map(|context| context.path.clone()),
                                        "route_kind": request_context.map(|context| context.route_kind),
                                        "service_tier": payload.get("service_tier").and_then(Value::as_str),
                                        "normalization_ms": normalization_ms,
                                        "upstream_open_ms": upstream_open_ms,
                                        "first_chunk_ms": first_chunk_ms,
                                        "first_event_ms": first_event_ms,
                                        "response_completed_ms": response_completed_ms,
                                        "end_to_end_ms": end_to_end_ms,
                                        "chunk_count": chunk_count,
                                        "event_count": event_count,
                                        "bytes_received": bytes_received,
                                        "output_tokens": response.get("usage").and_then(|usage| usage.get("output_tokens")).and_then(Value::as_i64),
                                        "total_tokens": response.get("usage").and_then(|usage| usage.get("total_tokens")).and_then(Value::as_i64),
                                    }),
                                )
                                .await;
                                let mut downstream_response =
                                    Json(response.clone()).into_response();
                                downstream_response.headers_mut().extend(success_headers.clone());
                                if let Ok(header_value) = HeaderValue::from_str(&format!(
                                    "normalize={normalization_ms:.1}; open={upstream_open_ms:.1}; first_chunk={}; completed={response_completed_ms:.1}; total={end_to_end_ms:.1}",
                                    first_chunk_ms
                                        .map(|value| format!("{value:.1}"))
                                        .unwrap_or_else(|| "na".to_string()),
                                )) {
                                    downstream_response
                                        .headers_mut()
                                        .insert("x-proxy-timing-ms", header_value);
                                }
                                return downstream_response;
                            }
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    let failed_total_ms = request_context
        .map(|context| context.request_started_at.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or_else(|| aggregate_started_at.elapsed().as_secs_f64() * 1000.0);
    log_request_event(
        state,
        crate::config::UPSTREAM_LOG_FILE,
        json!({
            "ts": crate::files::now_unix(),
            "phase": "responses_aggregate_incomplete",
            "request_id": request_context.map(|context| context.request_id.clone()),
            "path": request_context.map(|context| context.path.clone()),
            "route_kind": request_context.map(|context| context.route_kind),
            "service_tier": payload.get("service_tier").and_then(Value::as_str),
            "normalization_ms": normalization_ms,
            "upstream_open_ms": upstream_open_ms,
            "first_chunk_ms": first_chunk_ms,
            "first_event_ms": first_event_ms,
            "end_to_end_ms": failed_total_ms,
            "chunk_count": chunk_count,
            "event_count": event_count,
            "bytes_received": bytes_received,
        }),
    )
    .await;
    json_error(
        StatusCode::BAD_GATEWAY,
        "Could not build final response object",
    )
}

async fn stream_chat_chunks(
    state: &AppState,
    request_context: Option<&RequestContext>,
    headers: &HeaderMap,
    payload: &Value,
    client_options: ClientStreamOptions,
) -> Response {
    let response = match state
        .codex_adapter
        .open_upstream_response(state, request_context, headers, payload)
        .await
    {
        Ok(response) => response,
        Err(err) => return upstream_open_error_response(&err),
    };
    if !response.status().is_success() {
        return upstream_error_response(response).await;
    }
    let success_headers = processed_success_headers(response.headers());
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-5.4")
        .to_string();
    let created = crate::files::now_unix();
    let chunk_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let adapter = state.codex_adapter.clone();

    let payload_stream = response.bytes_stream();
    let out = async_stream::stream! {
        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!(
            "data: {}\n\n",
            json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":Value::Null}]
            })
        )));
        let mut parser = SseParser::default();
        let mut tool_state = ToolStreamState::default();
        let mut saw_done = false;
        futures::pin_mut!(payload_stream);
        while let Some(item) = payload_stream.next().await {
            match item {
                Ok(chunk) => {
                    let text = String::from_utf8_lossy(&chunk);
                    for event in parser.feed(&text) {
                        if event == "[DONE]" {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<Value>(&event) {
                            if let Some(line) = adapter.map_response_event_to_chat_chunk(
                                &value,
                                &mut tool_state,
                                &chunk_id,
                                created,
                                &model,
                            ) {
                                yield Ok(Bytes::from(line));
                            }

                            if let Some(error_chunk) =
                                chat_terminal_error_chunk(&value, &chunk_id, created, &model)
                            {
                                yield Ok(Bytes::from(error_chunk));
                                yield Ok(Bytes::from("data: [DONE]\n\n".to_string()));
                                return;
                            }

                            if matches!(
                                value.get("type").and_then(Value::as_str),
                                Some("response.completed") | Some("response.incomplete")
                            ) {
                                saw_done = true;
                                let finish_reason = chat_completion_finish_reason(value.get("response"));
                                let usage = value
                                    .get("response")
                                    .and_then(|response| response.get("usage"))
                                    .cloned()
                                    .unwrap_or_else(|| json!({}));
                                let mut final_chunk = json!({
                                    "id": chunk_id,
                                    "object":"chat.completion.chunk",
                                    "created":created,
                                    "model":model,
                                    "choices":[{"index":0,"delta":{},"finish_reason":finish_reason}],
                                });
                                if client_options.include_usage {
                                    final_chunk["usage"] = build_chat_usage(&usage);
                                }
                                yield Ok(Bytes::from(format!("data: {final_chunk}\n\n")));
                                yield Ok(Bytes::from("data: [DONE]\n\n".to_string()));
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!("chat stream read error: {err}");
                    break;
                }
            }
        }
        if !saw_done {
            yield Ok(Bytes::from(chat_stream_error_chunk(&chunk_id, created, &model, "Upstream stream terminated before response.completed")));
            yield Ok(Bytes::from("data: [DONE]\n\n".to_string()));
        }
    };

    let mut response = Response::new(Body::from_stream(out));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response.headers_mut().extend(success_headers);
    response
}

async fn aggregate_chat_completion(
    state: &AppState,
    request_context: Option<&RequestContext>,
    headers: &HeaderMap,
    payload: &Value,
    _client_options: ClientStreamOptions, // include_usage is stream-only per OpenAI spec; non-stream always returns usage
) -> Response {
    let (mut stream, success_headers) = match state
        .codex_adapter
        .open_upstream_response(state, request_context, headers, payload)
        .await
    {
        Ok(response) => {
            if !response.status().is_success() {
                return upstream_error_response(response).await;
            }
            let success_headers = processed_success_headers(response.headers());
            (
                response.bytes_stream(),
                success_headers,
            )
        }
        Err(err) => return upstream_open_error_response(&err),
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-5.4");
    let created = crate::files::now_unix();
    let mut parser = SseParser::default();
    let mut completed_response: Option<Value> = None;

    while let Some(item) = stream.next().await {
        if let Ok(chunk) = item {
            let text = String::from_utf8_lossy(&chunk);
            for event in parser.feed(&text) {
                if event == "[DONE]" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&event) {
                    if let Some(failure) = upstream_terminal_error_response(&value) {
                        return failure;
                    }
                    if matches!(
                        value.get("type").and_then(Value::as_str),
                        Some("response.completed") | Some("response.incomplete")
                    ) {
                        completed_response = value.get("response").cloned();
                    }
                }
            }
        }
    }

    let Some(response) = completed_response else {
        return json_error(
            StatusCode::BAD_GATEWAY,
            "Could not build final chat completion object",
        );
    };
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    let finish_reason = chat_completion_finish_reason(Some(&response));
    let completion = json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4().simple()),
        "object":"chat.completion",
        "created": created,
        "model": model,
        "choices":[{
            "index":0,
            "message":state.codex_adapter.response_to_chat_message(response.get("output")),
            "finish_reason":finish_reason
        }],
        "usage": build_chat_usage(&usage),
    });
    let mut downstream = Json(completion).into_response();
    downstream.headers_mut().extend(success_headers);
    downstream
}

fn terminal_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "response.failed" | "error"
    ) {
        return None;
    }
    event.get("error")
        .and_then(|value| value.get("message").or(Some(value)))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            event.get("response")
                .and_then(|response| response.get("error"))
                .and_then(|value| value.get("message").or(Some(value)))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            event.get("response")
                .and_then(|response| response.get("incomplete_details"))
                .map(|value| value.to_string())
        })
        .or_else(|| Some(format!("Upstream terminal event: {event_type}")))
}

fn upstream_terminal_error_response(event: &Value) -> Option<Response> {
    terminal_error_message(event)
        .map(|message| json_error(StatusCode::BAD_GATEWAY, &message))
}

fn chat_stream_error_chunk(chunk_id: &str, created: i64, model: &str, message: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "error"
            }],
            "error": {
                "message": message,
                "type": "server_error"
            }
        })
    )
}

fn chat_terminal_error_chunk(
    event: &Value,
    chunk_id: &str,
    created: i64,
    model: &str,
) -> Option<String> {
    terminal_error_message(event)
        .map(|message| chat_stream_error_chunk(chunk_id, created, model, &message))
}

/// Build an OpenAI-shaped chat `usage` from the upstream Responses-API usage,
/// preserving `prompt_tokens_details.cached_tokens` and
/// `completion_tokens_details.reasoning_tokens` (both documented on
/// chat.completion per the OpenAI spec) so clients that rely on them keep working.
fn build_chat_usage(usage: &Value) -> Value {
    let prompt_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let completion_tokens = usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0);
    let mut out = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
    });
    if let Some(details) = usage.get("input_tokens_details").cloned() {
        out["prompt_tokens_details"] = details;
    }
    if let Some(details) = usage.get("output_tokens_details").cloned() {
        out["completion_tokens_details"] = details;
    }
    out
}

fn chat_completion_finish_reason(response: Option<&Value>) -> &'static str {
    if response
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        == Some("incomplete")
    {
        return "length";
    }
    if response
        .and_then(|value| value.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("type").and_then(Value::as_str) == Some("refusal")
                            })
                        })
            })
        })
    {
        return "content_filter";
    }
    if response_output_has_tool_calls(response) {
        return "tool_calls";
    }
    "stop"
}

fn response_output_has_tool_calls(response: Option<&Value>) -> bool {
    response
        .and_then(|value| value.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call") | Some("custom_tool_call")
                )
            })
        })
}

fn prepare_ws_request(
    session_state: &WsSessionState,
    mut request: Value,
    msg_type: &str,
) -> Result<Value, &'static str> {
    if session_state.last_request.is_none() {
        if msg_type != "response.create" {
            return Err("response.create must be first");
        }
        if request
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return Err("missing model in response.create request");
        }
        if request.get("input").is_none() {
            request["input"] = json!([]);
        }
        return Ok(request);
    }

    if !request
        .get("input")
        .map(|value| value.is_array())
        .unwrap_or(false)
    {
        return Err("websocket request requires array field: input");
    }
    let prev_id = request
        .get("previous_response_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if prev_id.is_empty() {
        let mut merged = session_state
            .last_request
            .as_ref()
            .and_then(|value| value.get("input"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(output) = session_state
            .last_response_output
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
        {
            merged.extend(output);
        }
        let next = request
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        merged.extend(next);
        request["input"] = Value::Array(merged);
    }

    if request.get("model").is_none()
        && let Some(model) = session_state
            .last_request
            .as_ref()
            .and_then(|value| value.get("model"))
            .cloned()
    {
        request["model"] = model;
    }
    if request.get("instructions").is_none()
        && let Some(instructions) = session_state
            .last_request
            .as_ref()
            .and_then(|value| value.get("instructions"))
            .cloned()
    {
        request["instructions"] = instructions;
    }

    Ok(request)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use anyhow::Result;
    use axum::Router;
    use axum::body::{Body, Bytes, to_bytes};
    use axum::http::{HeaderValue, Request, StatusCode, header::CONTENT_TYPE};
    use axum::http::header::HeaderName;
    use axum::response::Response;
    use axum::routing::post;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tower::util::ServiceExt;

    use crate::routes::build_router;
    use crate::state::WsSessionState;
    use crate::test_support::test_state;

    async fn mock_upstream() -> Response {
        let stream = async_stream::stream! {
            yield Ok::<Bytes, Infallible>(Bytes::from(
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":7,\"total_tokens\":12}}}\n\n"
            ));
            yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
        };
        let mut response = Response::new(Body::from_stream(stream));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        response
    }

    async fn mock_upstream_with_headers() -> Response {
        let mut response = mock_upstream().await;
        response.headers_mut().insert(
            HeaderName::from_static("x-ratelimit-remaining-requests"),
            HeaderValue::from_static("42"),
        );
        response.headers_mut().insert(
            HeaderName::from_static("x-request-id"),
            HeaderValue::from_static("upstream-req-1"),
        );
        response.headers_mut().insert(
            HeaderName::from_static("openai-processing-ms"),
            HeaderValue::from_static("123"),
        );
        response
    }

    async fn mock_upstream_failed() -> Response {
        let stream = async_stream::stream! {
            yield Ok::<Bytes, Infallible>(Bytes::from(
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n"
            ));
            yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
        };
        let mut response = Response::new(Body::from_stream(stream));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        response
    }

    async fn mock_upstream_incomplete() -> Response {
        let stream = async_stream::stream! {
            yield Ok::<Bytes, Infallible>(Bytes::from(
                "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"refusal\",\"refusal\":\"blocked\"}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n"
            ));
            yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
        };
        let mut response = Response::new(Body::from_stream(stream));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        response
    }

    async fn capture_upstream_headers(headers: axum::http::HeaderMap) -> Response {
        let body = json!({
            "type":"response.completed",
            "response":{
                "status":"completed",
                "output":[{
                    "type":"message",
                    "content":[{
                        "type":"output_text",
                        "text": json!({
                            "received_session_id": headers.get("session_id").and_then(|v| v.to_str().ok()),
                            "received_account_id": headers.get("ChatGPT-Account-Id").and_then(|v| v.to_str().ok()),
                            "received_request_id": headers.get("x-request-id").and_then(|v| v.to_str().ok()),
                            "received_client_request_id": headers.get("x-client-request-id").and_then(|v| v.to_str().ok()),
                        }).to_string()
                    }]
                }],
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }
        });
        let stream = async_stream::stream! {
            yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {body}\n\n")));
            yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
        };
        let mut response = Response::new(Body::from_stream(stream));
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        response
    }

    #[tokio::test]
    async fn non_streaming_responses_route_aggregates_completed_response() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app = Router::new().route("/backend-api/codex/responses", post(mock_upstream));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "input":"hello",
                    "stream":false
                })
                .to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["status"], json!("completed"));
        assert_eq!(json["usage"]["output_tokens"], json!(7));

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn non_streaming_responses_route_returns_failed_terminal_as_bad_gateway() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app =
                Router::new().route("/backend-api/codex/responses", post(mock_upstream_failed));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "input":"hello",
                    "stream":false
                })
                .to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["message"], json!("boom"));

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn non_streaming_chat_route_returns_incomplete_terminal_with_length_finish_reason() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app =
                Router::new().route("/backend-api/codex/responses", post(mock_upstream_incomplete));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "messages":[{"role":"user","content":"hello"}],
                    "stream":false
                })
                .to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["choices"][0]["finish_reason"], json!("length"));
        assert_eq!(json["choices"][0]["message"]["refusal"], json!("blocked"));

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn successful_converted_response_preserves_important_upstream_headers() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/backend-api/codex/responses", post(mock_upstream_with_headers));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "input":"hello",
                    "stream":false
                })
                .to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.headers()["x-ratelimit-remaining-requests"], "42");
        assert_eq!(response.headers()["llm_provider-openai-processing-ms"], "123");

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn upstream_request_includes_session_account_and_request_headers() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/backend-api/codex/responses", post(capture_upstream_headers));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .header("x-request-id", "proxy-req-123")
            .header("x-client-request-id", "client-req-789")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "input":"hello",
                    "stream":false
                })
                .to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        let received: Value = serde_json::from_str(
            json["output"][0]["content"][0]["text"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected output text"))?,
        )?;
        assert!(received["received_session_id"].as_str().is_some());
        assert_eq!(received["received_account_id"], json!("acct-test"));
        assert!(received["received_request_id"].as_str().is_some());
        assert_eq!(received["received_client_request_id"], json!("client-req-789"));

        server.abort();
        Ok(())
    }

    #[test]
    fn websocket_append_merges_prior_input_and_output_when_previous_id_missing() {
        let session_state = WsSessionState {
            last_request: Some(json!({
                "model":"gpt-5.4",
                "instructions":"Keep it terse",
                "input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}]
            })),
            last_response_output: Some(json!([
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}
            ])),
        };
        let request = json!({
            "type":"response.append",
            "input":[{"role":"user","content":[{"type":"input_text","text":"again"}]}]
        });

        let Ok(prepared) =
            super::prepare_ws_request(&session_state, request, "response.append")
        else {
            panic!("append should prepare successfully");
        };

        assert_eq!(prepared["model"], json!("gpt-5.4"));
        assert_eq!(prepared["instructions"], json!("Keep it terse"));
        assert_eq!(prepared["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn non_streaming_responses_route_returns_incomplete_terminal_as_response_object() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/backend-api/codex/responses", post(mock_upstream_incomplete));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"model":"gpt-5.4","input":"hello","stream":false}).to_string(),
            ))?;

        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["status"], json!("incomplete"));
        assert_eq!(
            json["incomplete_details"]["reason"],
            json!("max_output_tokens")
        );

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn non_stream_chat_always_returns_usage_with_token_details() -> Result<()> {
        async fn mock() -> Response {
            let body = json!({
                "type":"response.completed",
                "response":{
                    "status":"completed",
                    "output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],
                    "usage":{
                        "input_tokens":10,
                        "input_tokens_details":{"cached_tokens":3},
                        "output_tokens":5,
                        "output_tokens_details":{"reasoning_tokens":2},
                        "total_tokens":15
                    }
                }
            });
            let stream = async_stream::stream! {
                yield Ok::<Bytes, Infallible>(Bytes::from(format!("data: {body}\n\n")));
                yield Ok::<Bytes, Infallible>(Bytes::from("data: [DONE]\n\n"));
            };
            let mut response = Response::new(Body::from_stream(stream));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            response
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let app = Router::new().route("/backend-api/codex/responses", post(mock));
            axum::serve(listener, app).await.ok();
        });

        let state = test_state(format!("http://{addr}/backend-api/codex/responses")).await?;
        let app = build_router(state);
        // stream_options.include_usage=false should NOT strip usage on a non-stream response
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "model":"gpt-5.4",
                    "messages":[{"role":"user","content":"hello"}],
                    "stream":false,
                    "stream_options":{"include_usage":false}
                })
                .to_string(),
            ))?;
        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        let json: Value = serde_json::from_slice(&body)?;
        assert_eq!(json["usage"]["prompt_tokens"], json!(10));
        assert_eq!(json["usage"]["completion_tokens"], json!(5));
        assert_eq!(json["usage"]["total_tokens"], json!(15));
        assert_eq!(
            json["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(3)
        );
        assert_eq!(
            json["usage"]["completion_tokens_details"]["reasoning_tokens"],
            json!(2)
        );

        server.abort();
        Ok(())
    }
}
