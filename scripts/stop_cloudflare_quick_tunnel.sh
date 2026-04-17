#!/usr/bin/env bash
set -euo pipefail

CONTAINER_NAME="${CONTAINER_NAME:-codex-proxy-cloudflared}"
docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
