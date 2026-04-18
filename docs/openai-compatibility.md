# OpenAI Compatibility Matrix

This proxy intentionally exposes a narrow, stable OpenAI-compatible facade over
the ChatGPT Codex upstream at `chatgpt.com/backend-api/codex/responses`.

It follows the LiteLLM ChatGPT/Codex adapter pattern behind the facade:

- upstream requests are always normalized into SSE-backed Codex requests
- upstream `store` is forced to `false`
- upstream `stream` is forced to `true`
- `reasoning.encrypted_content` is always included upstream
- a hard allowlist is applied after normalization

The public route surface below is authoritative for this repository.

## Route Contract

| route | surfaced | backing | id source | persists across requests | persists across restarts | allowed follow-up operations | unsupported error behavior |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `GET /v1/models`, `GET /models` | mounted | proxy-local-durable | proxy-generated static model ids | yes | yes | `GET /v1/models/{model_id}` | n/a |
| `GET /v1/models/{model_id}`, `GET /models/{model_id}` | mounted | proxy-local-durable | proxy-generated static model ids | yes | yes | none | unknown model -> `404` `invalid_request_error`-style envelope |
| `POST /v1/files`, `POST /files` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | list, retrieve, content, delete, `file_id` reuse in request normalization | malformed multipart -> `400`, missing fields -> `400` |
| `GET /v1/files`, `GET /files` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | retrieve, content, delete | internal storage failure -> `500` |
| `GET /v1/files/{file_id}`, `GET /files/{file_id}` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | content, delete | missing/deleted/expired file -> `404` |
| `GET /v1/files/{file_id}/content`, `GET /files/{file_id}/content` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | none | missing/deleted/expired file -> `404` |
| `DELETE /v1/files/{file_id}`, `DELETE /files/{file_id}` | mounted | proxy-local-durable | proxy-generated `file-*` ids | yes | yes | none | missing/deleted/expired file -> `404` |
| `POST /v1/responses`, `POST /responses` | mounted | stateless-emulation | upstream response payload ids | no durable lifecycle guarantee | no | same-request stream or final aggregate only | incomplete aggregate -> `502` |
| `POST /v1/chat/completions`, `POST /chat/completions` | mounted | stateless-emulation | proxy-generated `chatcmpl-*` ids | no durable lifecycle guarantee | no | same-request stream or final aggregate only | incomplete aggregate -> `502` |
| `GET /v1/responses/ws`, `GET /responses/ws` | mounted | proxy-local-ephemeral | websocket session-local continuity state | yes, within one socket | no | `response.create`, `response.append` | invalid websocket payload -> websocket `error` event |
| `GET/DELETE /v1/responses/{response_id}`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error` |
| `POST /v1/responses/{response_id}/cancel`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error` |
| `GET /v1/responses/{response_id}/input_items`, aliases | mounted | unsupported | response ids would refer to upstream create-time objects without durable semantics | no | no | none | `501` `upstream_capability_error` |
| `POST /v1/embeddings`, `/moderations`, `/images`, `/audio`, `/batches`, `/uploads`, aliases | mounted | unsupported | n/a | n/a | n/a | none | `501` `unsupported_route_error` |
| all other routes | unmounted | unsupported | n/a | n/a | n/a | none | `404` `invalid_request_error` |

## Behavioral Notes

- `previous_response_id` is treated as a create-time continuity hint only.
- `/v1/responses/ws` continuity is proxy-local and ephemeral; it does not imply
  durable response lifecycle support.
- `/v1/files*` is fully proxy-owned state and survives process restarts so long
  as the state root is preserved.
- `/v1/models*` is proxy-generated from the configured supported model list.
