# DNS Agent Control Plane

This repository now ships a full HTTP control plane for DNS agents. The server
exposes the `/register`, `/heartbeat`, `/claim`, `/extend`, `/report`, `/metrics`
and `/config` endpoints described in [`docs/control-plane-api.md`](docs/control-plane-api.md)
and persists agent state, leases and metrics in a SQL database.

## Quick start

1. **Install dependencies**: the control plane uses SQLite by default. No extra
   services are required for local development.
2. **Run migrations**:
   ```bash
   cargo run --bin control_plane -- --migrate-only # see docs/control-plane-deploy.md
   ```
   or apply the SQL scripts in [`infra/migrations`](infra/migrations) manually.
3. **Start the server**:
   ```bash
   CONTROL_PLANE_BIND=0.0.0.0:8080 \
   CONTROL_PLANE_DATABASE_URL=sqlite://control-plane.db \
   cargo run --bin control_plane
   ```
4. **Call the API** using any HTTP client. The integration tests in
   [`tests/control_plane_api.rs`](tests/control_plane_api.rs) demonstrate both
   happy-path and rate-limit scenarios.

By default the server writes structured traces via `tower-http` and stores data
under the SQLite file configured by `CONTROL_PLANE_DATABASE_URL`.

## Database schema

The schema is defined in [`infra/migrations`](infra/migrations):

- `agents` — registered agents, authentication tokens, heartbeat deadlines.
- `tasks`/`leases` — queued work and active leases with TTL-indexed deadlines.
- `agent_metrics` — accepted metric envelopes (retained for 30 days).
- `agent_configs` — signed configuration bundles served via `/config`.
- `concurrency_windows` — persisted snapshots of dispatcher concurrency state
  (used by `ConcurrencyController::with_persistence`).

`AgentStore` wraps a `SqlitePool` (any SQLx-compatible database can be used) and
provides `register`, `heartbeat`, `claim`, `extend`, `report`, `ingest_metrics`
and `latest_config` helpers. Background retention jobs purge stale metrics and
expired concurrency windows via the shared `TimerService`.

## Authentication & rate limiting

All endpoints except `/register` require the `X-Agent-Id` and `X-Agent-Token`
headers. The middleware in `runtime::middleware` validates credentials using
`AgentStore::authenticate_credentials`, attaches the agent identity to the
request, and enforces per-endpoint quotas:

| Endpoint   | Limit                                     |
|------------|-------------------------------------------|
| `/register`| 1 request per hostname or IP every 10 min |
| `/heartbeat` | 6/minute with burst 3                   |
| `/claim`   | 12/minute with burst 6                    |
| `/extend`  | 30/minute with burst 10                   |
| `/report`  | 12/minute with burst 6                    |
| `/metrics` | 60/minute with burst 10                   |
| `/config`  | 6/hour with burst 2                       |

429 responses include `ERR_RATE_LIMITED` and a `Retry-After` header. Authentication
failures surface `ERR_UNAUTHORIZED`/`ERR_FORBIDDEN` JSON envelopes consistent with
the published API contract.

## Observability & monitoring

- **Tracing**: HTTP requests emit spans with method/URI metadata via
  `runtime::telemetry::http_trace_layer`.
- **Metrics**: `ConcurrencyController::with_persistence` writes snapshots into
  `concurrency_windows`, enabling dashboards around queue depth and available
  window sizes. Timer lag metrics continue to be published via the existing
  `metrics` crate counters.
- **Recommended alerts**:
  - Heartbeat freshness (`agents.last_heartbeat` lag > 2× timeout).
  - Excessive rate-limit events (`ERR_RATE_LIMITED` count per agent).
  - Concurrency window collapse (`concurrency_windows.limit` below 1).

The new `/metrics` endpoint accepts aggregated counters/gauges which can feed
into Prometheus or any metrics pipeline using the stored JSON envelopes.

## Testing & CI

Integration and smoke tests live in `tests/`:

- `control_plane_api.rs` exercises the API (positive/negative flows and a simple
  throughput benchmark).
- Additional unit tests cover agent registration and concurrency persistence.

Run the full suite with:

```bash
make test
```

The `ci` target hooks into the same command to simplify pipeline integration.

## Deployment

Operational runbooks and playbooks remain under `docs/` and `infra/`. A dedicated
control plane deploy guide is provided in
[`docs/control-plane-deploy.md`](docs/control-plane-deploy.md) outlining service
restart procedures, health checks and suggested dashboard widgets for the new
endpoints.
