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

## Документация
- [Обзор проекта](docs/overview.md)
- [Архитектура и код](docs/code.md)
- [Инфраструктура и CI/CD](docs/ci_cd.md)
- [Runbook](docs/runbook.md)
