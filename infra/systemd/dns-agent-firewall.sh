#!/usr/bin/env bash
set -euo pipefail

if ! command -v iptables >/dev/null 2>&1; then
  echo "iptables is required to apply firewall rules" >&2
  exit 1
fi

LISTEN_ADDRESS="${AGENT_DNS_LISTEN:-0.0.0.0:2053}"
LISTEN_PORT="${LISTEN_ADDRESS##*:}"

API_HOST="api-gw.example.com"
API_PORT=443
API_IP=$(getent ahosts "${API_HOST}" | awk 'NR==1 { print $1 }')

if [[ -z "${API_IP}" ]]; then
  echo "Failed to resolve ${API_HOST}" >&2
  exit 1
fi

iptables -C INPUT -p udp --dport "${LISTEN_PORT}" -j ACCEPT 2>/dev/null || \
  iptables -I INPUT -p udp --dport "${LISTEN_PORT}" -j ACCEPT
iptables -C OUTPUT -p tcp -d "${API_IP}" --dport "${API_PORT}" -j ACCEPT 2>/dev/null || \
  iptables -I OUTPUT -p tcp -d "${API_IP}" --dport "${API_PORT}" -j ACCEPT
