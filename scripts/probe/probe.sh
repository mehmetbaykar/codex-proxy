#!/usr/bin/env bash
#
# Probe every OpenAI-spec field against the live ChatGPT/Codex backend
# to derive an empirical accept/reject matrix. Output: matrix.tsv next to
# this script. Used to keep src/normalization.rs allowlist grounded in
# real backend behavior instead of guessed parity with OpenAI docs.
#
# Usage:
#   CODEX_PROBE_TOKEN=<access_token> CODEX_PROBE_ACCOUNT_ID=<uuid> \
#     bash scripts/probe/probe.sh
#
# The token is never written to disk or logged. On each run, matrix.tsv
# is overwritten with columns: <field>\t<http_status>\t<verdict>\t<message_snippet>.

set -u
: "${CODEX_PROBE_TOKEN:?CODEX_PROBE_TOKEN env var required (chatgpt.com access_token)}"
: "${CODEX_PROBE_ACCOUNT_ID:?CODEX_PROBE_ACCOUNT_ID env var required (ChatGPT account UUID)}"

URL="${CODEX_PROBE_URL:-https://chatgpt.com/backend-api/codex/responses}"
MODEL="${CODEX_PROBE_MODEL:-gpt-5.4}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
MATRIX="$HERE/matrix.tsv"
trap 'rm -rf "$WORK"' EXIT

run_probe() {
  local name="$1" extra="$2"
  local sid="pb-$(printf '%08x' $((RANDOM * RANDOM)))"
  local body
  if printf '%s' "$extra" | grep -q '"include"'; then
    body="{\"model\":\"$MODEL\",\"input\":[{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}],\"stream\":true,\"store\":false,\"instructions\":\"one word: ok\""
  else
    body="{\"model\":\"$MODEL\",\"input\":[{\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}],\"stream\":true,\"store\":false,\"include\":[\"reasoning.encrypted_content\"],\"instructions\":\"one word: ok\""
  fi
  [ -n "$extra" ] && body="$body,$extra"
  body="$body}"
  local hdr="$WORK/$name.hdr"
  local out="$WORK/$name.out"
  printf '%s' "$body" >"$WORK/$name.body"
  curl -sS -o "$out" -D "$hdr" -m 4 --no-buffer \
    -H "Authorization: Bearer $CODEX_PROBE_TOKEN" \
    -H "ChatGPT-Account-Id: $CODEX_PROBE_ACCOUNT_ID" \
    -H "session_id: $sid" \
    -H "originator: codex-proxy" \
    -H "user-agent: codex-proxy-probe/0.1" \
    -H "content-type: application/json" \
    -H "accept: text/event-stream" \
    -X POST "$URL" --data "@$WORK/$name.body" 2>/dev/null || true
  local status
  status=$(head -1 "$hdr" 2>/dev/null | awk '{print $2}')
  local verdict="?"
  case "$status" in
    200) verdict="accept" ;;
    400) verdict="reject" ;;
    401) verdict="auth-fail" ;;
    "") verdict="timeout-accept" ;;
    *) verdict="review" ;;
  esac
  local snippet
  if [ "$status" = "400" ]; then
    snippet=$(head -c 400 "$out" | tr -d '\n\r\t' | head -c 200)
  else
    snippet=$(head -c 80 "$out" | tr -d '\n\r\t' | head -c 60)
  fi
  printf '%s\t%s\t%s\t%s\n' "$name" "$status" "$verdict" "$snippet"
  echo "  [$verdict] $name" >&2
}

declare -a PROBES=(
  "baseline||"
  "temperature|\"temperature\":0.7"
  "top_p|\"top_p\":0.9"
  "max_output_tokens|\"max_output_tokens\":20"
  "max_tokens|\"max_tokens\":20"
  "max_completion_tokens|\"max_completion_tokens\":20"
  "n|\"n\":2"
  "seed|\"seed\":42"
  "stop|\"stop\":[\"zzz\"]"
  "logprobs|\"logprobs\":true"
  "top_logprobs|\"top_logprobs\":2"
  "user|\"user\":\"probe-user\""
  "safety_identifier|\"safety_identifier\":\"probe-sid\""
  "prompt_cache_key_short|\"prompt_cache_key\":\"pck-9ch\""
  "prompt_cache_key_long|\"prompt_cache_key\":\"$(printf 'x%.0s' $(seq 1 100))\""
  "metadata|\"metadata\":{\"k\":\"v\"}"
  "service_tier_standard|\"service_tier\":\"standard\""
  "service_tier_flex|\"service_tier\":\"flex\""
  "service_tier_auto|\"service_tier\":\"auto\""
  "service_tier_priority|\"service_tier\":\"priority\""
  "service_tier_default|\"service_tier\":\"default\""
  "parallel_tool_calls|\"parallel_tool_calls\":false"
  "text_json_object|\"text\":{\"format\":{\"type\":\"json_object\"}}"
  "text_json_schema|\"text\":{\"format\":{\"type\":\"json_schema\",\"name\":\"t\",\"schema\":{\"type\":\"object\"}}}"
  "text_text|\"text\":{\"format\":{\"type\":\"text\"}}"
  "truncation_auto|\"truncation\":\"auto\""
  "truncation_disabled|\"truncation\":\"disabled\""
  "background|\"background\":true"
  "modalities|\"modalities\":[\"text\"]"
  "audio|\"audio\":{\"voice\":\"alloy\",\"format\":\"pcm16\"}"
  "response_format_legacy|\"response_format\":{\"type\":\"json_object\"}"
  "reasoning_effort_legacy|\"reasoning_effort\":\"high\""
  "reasoning_minimal|\"reasoning\":{\"effort\":\"minimal\"}"
  "reasoning_low|\"reasoning\":{\"effort\":\"low\"}"
  "reasoning_medium|\"reasoning\":{\"effort\":\"medium\"}"
  "reasoning_high|\"reasoning\":{\"effort\":\"high\"}"
  "reasoning_xhigh|\"reasoning\":{\"effort\":\"xhigh\"}"
  "reasoning_summary_auto|\"reasoning\":{\"effort\":\"low\",\"summary\":\"auto\"}"
  "reasoning_summary_detailed|\"reasoning\":{\"effort\":\"low\",\"summary\":\"detailed\"}"
  "reasoning_summary_concise|\"reasoning\":{\"effort\":\"low\",\"summary\":\"concise\"}"
  "include_summary|\"include\":[\"reasoning.encrypted_content\",\"reasoning.summary\"]"
  "include_usage|\"include\":[\"reasoning.encrypted_content\",\"usage\"]"
  "include_filesearch|\"include\":[\"reasoning.encrypted_content\",\"file_search_call.results\"]"
  "include_websources|\"include\":[\"reasoning.encrypted_content\",\"web_search_call.action.sources\"]"
  "stream_options|\"stream_options\":{\"include_usage\":true}"
  "prompt_cache_retention|\"prompt_cache_retention\":\"24h\""
  "context_management|\"context_management\":{\"type\":\"compaction\",\"compact_threshold\":2000}"
  "conversation|\"conversation\":\"conv_000000000000000000000\""
  "frequency_penalty|\"frequency_penalty\":0.5"
  "presence_penalty|\"presence_penalty\":0.5"
  "max_tool_calls|\"max_tool_calls\":3"
  "tool_function|\"tools\":[{\"type\":\"function\",\"name\":\"lookup\",\"parameters\":{\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"}}}}]"
  "tool_web_search|\"tools\":[{\"type\":\"web_search\"}]"
  "tool_web_search_preview|\"tools\":[{\"type\":\"web_search_preview\"}]"
  "tool_file_search|\"tools\":[{\"type\":\"file_search\",\"vector_store_ids\":[\"vs_0000000000000000000\"]}]"
  "tool_code_interpreter|\"tools\":[{\"type\":\"code_interpreter\",\"container\":{\"type\":\"auto\"}}]"
  "tool_image_generation|\"tools\":[{\"type\":\"image_generation\"}]"
  "tool_computer_use|\"tools\":[{\"type\":\"computer_use_preview\",\"display_width\":1024,\"display_height\":768,\"environment\":\"browser\"}]"
  "tool_mcp|\"tools\":[{\"type\":\"mcp\",\"server_label\":\"t\",\"server_url\":\"https://example.com/mcp\"}]"
  "tool_shell|\"tools\":[{\"type\":\"shell\"}]"
  "tool_apply_patch|\"tools\":[{\"type\":\"apply_patch\"}]"
  "tool_local_shell|\"tools\":[{\"type\":\"local_shell\"}]"
  "tool_custom_unknown|\"tools\":[{\"type\":\"my_custom_tool_xxx\"}]"
  "tool_choice_auto|\"tool_choice\":\"auto\""
  "tool_choice_none|\"tool_choice\":\"none\""
  "tool_choice_required|\"tool_choice\":\"required\""
  "web_search_options|\"web_search_options\":{\"search_context_size\":\"high\"}"
)

echo "probing ${#PROBES[@]} fields serially against $URL ..." >&2
: >"$MATRIX"
for entry in "${PROBES[@]}"; do
  name="${entry%%|*}"
  rest="${entry#*|}"
  case "$entry" in
    *"||"*) extra="" ;;
    *) extra="$rest" ;;
  esac
  run_probe "$name" "$extra" >>"$MATRIX"
done

echo "wrote $MATRIX" >&2
sort -o "$MATRIX" "$MATRIX"
awk -F'\t' '{print $3}' "$MATRIX" | sort | uniq -c >&2
