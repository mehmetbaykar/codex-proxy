# OpenAI Compatibility Matrix

This proxy exposes a narrow, stable, **empirically-grounded** OpenAI-compatible
facade over the ChatGPT Codex upstream at
`chatgpt.com/backend-api/codex/responses`. Target = official OpenAI API. LiteLLM
is consulted only as a conversion reference — not a parity ceiling.

Every field-level forward/drop decision below is backed by live probe data in
`scripts/probe/matrix.tsv` (regenerate with
`CODEX_PROBE_TOKEN=... CODEX_PROBE_ACCOUNT_ID=... scripts/probe/probe.sh`).

## Pipeline invariants

- Upstream requests are always normalized into SSE-backed Codex requests.
- Upstream `store` is forced to `false`.
- Upstream `stream` is forced to `true`.
- `reasoning.encrypted_content` is always present in `include`.
- A hard allowlist is applied after normalization; anything outside is dropped.

## Route contract

| route | surfaced | backing | id source | persists across requests | persists across restarts | allowed follow-up operations | unsupported error behavior |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `GET /v1/models`, `GET /models` | mounted | proxy-local-durable | proxy-generated static model ids | yes | yes | `GET /v1/models/{model_id}` | n/a |
| `GET /v1/models/{model_id}`, `GET /models/{model_id}` | mounted | proxy-local-durable | proxy-generated static model ids | yes | yes | none | unknown model -> `404` `invalid_request_error`-style envelope |
| `POST /v1/files`, `POST /files` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | list, retrieve, content, delete, `file_id` reuse in request normalization | malformed multipart -> `400`, missing fields -> `400` |
| `GET /v1/files`, `GET /files` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | retrieve, content, delete | internal storage failure -> `500` |
| `GET /v1/files/{file_id}`, `GET /files/{file_id}` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | content, delete | missing/deleted/expired file -> `404` |
| `GET /v1/files/{file_id}/content`, `GET /files/{file_id}/content` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | none | missing/deleted/expired file -> `404` |
| `DELETE /v1/files/{file_id}`, `DELETE /files/{file_id}` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | none | missing/deleted/expired file -> `404` |
| `POST /v1/responses`, `POST /responses` | mounted | stateless-emulation | upstream response payload ids | no durable lifecycle guarantee | no | same-request stream or final aggregate only | incomplete aggregate -> `502`; upstream `response.incomplete` returns full object with `status:"incomplete"` |
| `POST /v1/chat/completions`, `POST /chat/completions` | mounted | stateless-emulation | proxy-generated `chatcmpl-*` ids | no durable lifecycle guarantee | no | same-request stream or final aggregate only | incomplete aggregate -> `502`; upstream `response.incomplete` maps to `finish_reason:"length"` |
| `GET /v1/responses/ws`, `GET /responses/ws` | mounted | proxy-local-ephemeral | websocket session-local continuity state | yes, within one socket | no | `response.create`, `response.append` | invalid websocket payload -> websocket `error` event |
| `GET/DELETE /v1/responses/{response_id}`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error`, code `upstream_capability` |
| `POST /v1/responses/{response_id}/cancel`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error`, code `upstream_capability` |
| `GET /v1/responses/{response_id}/input_items`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error`, code `upstream_capability` |
| `POST /v1/embeddings`, `/moderations`, `/images`, `/audio`, `/batches`, `/uploads`, aliases | mounted | unsupported | n/a | n/a | n/a | none | `501` `unsupported_route_error`, code `unsupported_route` |
| all other routes | unmounted | unsupported | n/a | n/a | n/a | none | `404` `invalid_request_error` |

## Field-level support (from probe)

### Top-level request fields — forwarded

| field | notes |
| --- | --- |
| `model` | one of `SUPPORTED_MODELS` (see `src/config.rs`) |
| `input` | normalized from `messages` / string / array forms |
| `instructions` | merged from `system`/`developer` messages when present |
| `tools` | filtered to accepted `type`s; see tools table below |
| `tool_choice` | `"auto"`, `"none"` |
| `store` | forced to `false` |
| `stream` | forced to `true` upstream; proxy aggregates for `stream:false` clients |
| `include` | filtered to accepted values (see below); `reasoning.encrypted_content` auto-added |
| `reasoning` | `effort` ∈ `{none, low, medium, high, xhigh}`; `summary` ∈ `{auto, detailed, concise}`. `effort:"minimal"` (OpenAI canonical) silently clamped to `"low"` — backend rejects `minimal` |
| `text` | only `text.format.type:"text"` forwarded |
| `parallel_tool_calls` | bool |
| `previous_response_id` | must be a valid ID from a prior `response.completed` |
| `prompt_cache_key` | any string (shared 64-char cap with `session_id` header) |
| `service_tier` | only `"priority"` survives; any other value dropped |

### Top-level request fields — dropped (backend 400s on each)

`temperature`, `top_p`, `max_tokens`, `max_output_tokens`, `max_completion_tokens`,
`n`, `seed`, `stop`, `logprobs`, `top_logprobs`, `user`, `safety_identifier`,
`metadata`, `stream_options`, `frequency_penalty`, `presence_penalty`,
`max_tool_calls`, `modalities`, `audio`, `background`, `response_format` (legacy),
`truncation`, `prompt_cache_retention`, `conversation`, `web_search_options` (see
G15 below).

Backend rejection shape: `400 {"detail":"Unsupported parameter: X"}`.

### Tools — accepted types

`function`, `web_search`, `image_generation`.

### Tools — rejected types (backend 400)

`web_search_preview`, `file_search`, `code_interpreter`, `computer_use_preview`,
`mcp`, `shell`, `apply_patch`, `local_shell`, unknown custom types.

### `include` — accepted values

`reasoning.encrypted_content`, `file_search_call.results`,
`web_search_call.results`, `web_search_call.action.sources`,
`message.input_image.image_url`, `computer_call_output.output.image_url`,
`code_interpreter_call.outputs`, `message.output_text.logprobs`.

All other values (e.g. `reasoning.summary`, `usage`) are stripped with a debug log.

### `response_format` / `text.format` handling

- `response_format:{type:"text"}` → mapped to `text.format:{type:"text"}`.
- `response_format:{type:"json_object"}` or `{type:"json_schema"}` → **dropped
  silently with debug log**. Backend rejects both (`400` on `text.format.type`).
  Structured-output emulation via prompt injection is out of scope V1.
- Direct `text.format.type` != `"text"` → `text.format` stripped.

### `reasoning` value clamping

- `reasoning.effort:"minimal"` → clamped to `"low"` (backend rejects `minimal`).
  Canonical OpenAI spec keeps `minimal`; this is the only value-level divergence.

### `web_search_options` migration (G15)

`web_search_options:{search_context_size, user_location}` is the canonical
OpenAI field. Backend rejects it but DOES accept `{type:"web_search"}` in
`tools`. The proxy migrates the field into a tool entry (preserving
`search_context_size` and `user_location`) and drops the original key.

## SSE events

### Proxy forwards these upstream events unchanged on `/v1/responses` (passthrough)

`response.created`, `response.in_progress`, `response.output_item.added`,
`response.content_part.added`, `response.output_text.delta`,
`response.output_text.done`, `response.content_part.done`,
`response.output_item.done`, `response.function_call_arguments.delta`,
`response.function_call_arguments.done`, `response.web_search_call.in_progress`,
`response.web_search_call.searching`, `response.web_search_call.completed`,
`response.completed`, plus any terminal `response.incomplete` / `response.failed` / `error`.

### Chat-completions facade — event mapping

| Upstream event | Chat-stream delta |
| --- | --- |
| `response.output_text.delta` | `choices[0].delta.content` |
| `response.reasoning_summary_text.delta` / `response.reasoning_text.delta` | `choices[0].delta.reasoning_content` (for compatibility with reasoning-aware clients) |
| `response.content_part.added` where `part.type == "refusal"` | `choices[0].delta.refusal` |
| `response.output_item.added` where `item.type ∈ {function_call, custom_tool_call}` | `choices[0].delta.tool_calls[]` init (id, type, name, "") |
| `response.function_call_arguments.delta` / `response.custom_tool_call_input.delta` | `choices[0].delta.tool_calls[].function.arguments` |
| `response.function_call_arguments.done` / `response.custom_tool_call_input.done` (only when no prior deltas) | full-arg single chunk |
| `response.completed` / `response.incomplete` | final chunk with `finish_reason` + `usage` (see below) |

### `finish_reason` mapping

- `status:"completed"` → `"stop"`
- `status:"completed"` with a refusal content_part → `"content_filter"`
- `status:"completed"` with a tool call → `"tool_calls"`
- `status:"incomplete"` → `"length"`

### Usage on chat.completion

Non-stream: always returned (OpenAI spec — `stream_options.include_usage` is
stream-only). Stream: emitted on the final chunk only when
`stream_options.include_usage=true`.

Fields exposed:
- `prompt_tokens`, `completion_tokens`, `total_tokens`.
- `prompt_tokens_details` (copied from upstream `input_tokens_details`, e.g.
  `cached_tokens`).
- `completion_tokens_details` (copied from upstream `output_tokens_details`,
  e.g. `reasoning_tokens`).

## Error envelope

Every 4xx/5xx emitted by the proxy is shaped:

```json
{
  "error": {
    "message": "...",
    "type": "invalid_request_error | authentication_error | ...",
    "code": "unsupported_route | upstream_capability | <upstream code> | null",
    "param": "<field name> | null"
  }
}
```

Upstream bodies get normalized on the way out: `{"detail":"..."}` is translated
into the envelope above (type inferred from HTTP status); structured
`{"error":{...}}` bodies pass through unchanged. This keeps the OpenAI Python
SDK's typed exception mapping working end-to-end.

## Header notes

- Proxy's `session_id` header (and upstream-reused `prompt_cache_key`) is capped
  at 64 characters upstream. Our request-id UUIDs are 36 chars, well under.
- Rate-limit headers (`x-ratelimit-*`, `x-request-id`) are forwarded from
  upstream 200 responses. Other upstream headers are prefixed `llm_provider-*`.
