#!/usr/bin/env bash
set -euo pipefail

LISTEN_ADDRESS="${AGENT_DNS_LISTEN:-127.0.0.1:2053}"
LISTEN_HOST="${LISTEN_ADDRESS%%:*}"
LISTEN_PORT="${LISTEN_ADDRESS##*:}"

sleep 2

if command -v dig >/dev/null 2>&1; then
  dig +tries=1 +time=2 @"${LISTEN_HOST}" -p "${LISTEN_PORT}" example.com > /dev/null
else
  echo "dig not available; skipping DNS health check" >&2
fi
