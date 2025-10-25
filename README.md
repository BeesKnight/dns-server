# DNS Agent Control Plane Contract Updates

This project implements a DNS agent that communicates with a control plane. The
following payloads document the extended contract that includes task
specifications and observation reporting.

## Claim response lease schema

```json
{
  "lease_id": 123,
  "task_id": 456,
  "kind": "dns",
  "lease_until_ms": 1500,
  "spec": {
    "kind": "tcp",
    "host": "203.0.113.5",
    "port": 443
  }
}
```

The `spec` field is a discriminated union encoded with `kind` and contains
variant-specific data:

- `dns`: `{ "query": string, "server": string | null }`
- `http`: `{ "url": string, "method": string | null }`
- `tcp`: `{ "host": string, "port": number }`
- `ping`: `{ "host": string, "count": number | null }`
- `trace`: `{ "host": string, "max_hops": number | null }`

## Report request schema

```json
{
  "agent_id": 7,
  "completed": [
    {
      "lease_id": 123,
      "observations": [
        { "name": "latency_ms", "value": 42.7, "unit": "ms" }
      ]
    }
  ],
  "cancelled": [
    {
      "lease_id": 456,
      "observations": [
        { "name": "cancelled", "value": true }
      ]
    }
  ]
}
```

Observation values are arbitrary JSON values and may optionally include a `unit`
string.

## Quick check orchestration (`POST /v1/jobs/checks`)

The jobs service now exposes a lightweight endpoint for ad-hoc checks that does
not require creating a `site` beforehand. The API accepts a JSON payload:

```json
{
  "url": "https://example.com",
  "template": "quick",
  "kinds": ["http"],
  "dns_server": "1.1.1.1"
}
```

- `url` (**required**) — HTTP/HTTPS target that will be used to derive specs for
  HTTP/ping/tcp/dns/trace tasks.
- `template` (optional, defaults to `quick`) — predefined bundle of kinds. The
  templates mirror the `sites-svc` behaviour (`quick`, `full_site_health`,
  `network_deep`).
- `kinds` (optional) — explicit override of check kinds. The list is sanitized
  and merged with the template.
- `dns_server` (optional) — IPv4/IPv6 override for DNS checks.

Response example:

```json
{
  "check_id": "9c89f7d4-1b7d-4c65-8d17-0c5401f47446",
  "queued": ["http"],
  "cached": [],
  "skipped": []
}
```

Immediately after enqueuing, the service publishes Server-Sent Events to the
`check-upd:{check_id}` channel:

- `check.start` — includes request metadata (`url`, `template`, `queued`,
  `cached`, `skipped`, requester id/IP).
- `status` — mirrors `queued/cached/skipped` for UI progress bars.
- `result` (optional) — emitted instantly for cached kinds with
  `{"source":"cache"}` to allow UI to display historical data.

Flood protection:

- Per-user and per-IP rate limits (`QUICK_RATE_PER_USER`, `QUICK_RATE_PER_IP`,
  window `QUICK_RATE_WINDOW`).
- Short-term deduplication of specs (`QUICK_DEDUP_TTL`) based on URL+template.
- URL and DNS server validation with informative 4xx responses.

Every request inserts a transient record into `checks` and a matching row in
`quick_checks` that stores the request metadata and cache key. Agents continue
reporting through the existing `/v1/agents/report` pipeline.

## Документация
- [Обзор проекта](docs/overview.md)
- [Архитектура и код](docs/code.md)
- [Инфраструктура и CI/CD](docs/ci_cd.md)
- [Runbook](docs/runbook.md)
