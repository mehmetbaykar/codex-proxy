#!/usr/bin/env bash
set -euo pipefail

PORT="${PORT:-3000}"
TARGET_URL="${TARGET_URL:-}"
CONTAINER_NAME="${CONTAINER_NAME:-codex-proxy-cloudflared}"
declare -a DOCKER_ARGS
DOCKER_ARGS=()

if [[ -z "${TARGET_URL}" ]]; then
  case "$(uname -s)" in
    Darwin|MINGW*|MSYS*|CYGWIN*)
      TARGET_URL="http://host.docker.internal:${PORT}"
      ;;
    *)
      TARGET_URL="http://127.0.0.1:${PORT}"
      DOCKER_ARGS=(--network host --add-host host.docker.internal:host-gateway)
      ;;
  esac
fi

declare -a CMD
CMD=(docker run --rm --name "${CONTAINER_NAME}")
if [[ ${#DOCKER_ARGS[@]} -gt 0 ]]; then
  CMD+=("${DOCKER_ARGS[@]}")
fi
CMD+=(cloudflare/cloudflared:latest tunnel --url "${TARGET_URL}")

exec "${CMD[@]}"
