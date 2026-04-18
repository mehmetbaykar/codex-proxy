# Observed SSE event types — `chatgpt.com/backend-api/codex/responses`

Derived 2026-04-18 against `gpt-5.4` by running four diverse prompts through
the live backend and collecting every distinct `"type":"..."` string. Used as
ground truth for the chat-completions facade event mapper in
`src/streaming.rs::map_response_event_to_chat_chunk`.

## Prompts

1. `reasoning`   — "Think step by step. What is 17*23?" with `reasoning.summary="detailed"`
2. `function_call` — Tool-call-forcing prompt with a single `{type:"function",name:"get_weather"}` tool
3. `refusal`    — Request for copyrighted book text verbatim
4. `web_search` — "Latest OpenAI model released in 2026?" with `{type:"web_search"}` tool

## Observed events

| Event                                       | Seen in prompts        | Notes |
|---------------------------------------------|------------------------|-------|
| `response.created`                          | all                    | opening frame |
| `response.in_progress`                      | all                    | intermediate heartbeat |
| `response.output_item.added`                | all                    | item.type ∈ {`message`, `reasoning`, `function_call`} |
| `response.content_part.added`               | reasoning, refusal, web_search | part.type ∈ {`output_text`, `refusal`} |
| `response.output_text.delta`                | all (when text output) | streaming token |
| `response.output_text.done`                 | all (when text output) | text item complete |
| `response.content_part.done`                | all (when text output) | part final value |
| `response.output_item.done`                 | all                    | item final value incl. annotations |
| `response.function_call_arguments.delta`    | function_call          | streaming JSON argument |
| `response.function_call_arguments.done`     | function_call          | final arguments |
| `response.web_search_call.in_progress`      | web_search             | tool lifecycle |
| `response.web_search_call.searching`        | web_search             | tool mid-action |
| `response.web_search_call.completed`        | web_search             | tool done with sources |
| `response.completed`                        | all                    | terminal; carries final response + usage |

## NOT observed (despite triggering prompts)

Backend does not appear to emit these for `gpt-5.4` on the probe prompts:

- `response.refusal.delta` / `response.refusal.done`
  → refusal arrives as `response.content_part.added` with `part.type:"refusal"`, never a standalone delta stream.
- `response.reasoning_summary_text.delta` / `response.reasoning_summary_text.done`
  → even with `reasoning.summary:"detailed"`, summary text was not streamed in probe.
- `response.reasoning_summary_part.added` / `response.reasoning_summary_part.done`
- `response.output_text.annotation.added`
  → annotations (URL citations) appear on `response.output_item.done` message items' `content[].annotations` instead.
- `response.image_generation_call.*`, `response.code_interpreter_call.*`, `response.mcp_call.*`
  → those tool types are also rejected at request time on `gpt-5.4` (see `matrix.tsv`).
- `response.failed`, `response.incomplete`
  → not observed on hot path. `response.incomplete` would fire on hard token cut-offs; worth handling defensively even if not observed here.

## Header-level finding

`session_id` header is capped at 64 characters upstream (the backend uses it as an internal
`prompt_cache_key`). Sending anything longer yields:

```
400 {"error":{"message":"Invalid 'prompt_cache_key': string too long. Expected a string
  with maximum length 64, but got a string with length N instead.",
  "type":"invalid_request_error","param":"prompt_cache_key","code":"string_above_max_length"}}
```

The proxy's request-id is a 36-char UUID, well under the cap.
