# Control Plane Deployment Guide

## Prerequisites
- Database available and migrated using the SQL files in `infra/migrations`.
- Environment variables
  - `CONTROL_PLANE_BIND` (listen address, default `0.0.0.0:8080`).
  - `CONTROL_PLANE_DATABASE_URL` (SQLite, Postgres or MySQL DSN supported by SQLx).

## Rolling restart
1. Apply migrations (idempotent):
   ```bash
   sqlx migrate run --source infra/migrations --database-url "$CONTROL_PLANE_DATABASE_URL"
   ```
2. Restart the service unit (systemd example):
   ```bash
   systemctl restart dns-control-plane.service
   ```
3. Confirm health:
   - `GET /config` returns 200 with an `ETag` header.
   - `POST /heartbeat` with a known agent succeeds (200) and updates
     `agents.last_heartbeat`.

## Dashboards & alerts
- **Latency**: monitor P50/P95 timings per route (target: spec in
  `docs/control-plane-api.md`).
- **Rate limiting**: chart `ERR_RATE_LIMITED` responses split by endpoint and
  agent. Alert when a single agent produces >50 429s in 5 minutes.
- **Concurrency**: visualize `concurrency_windows` as stacked graphs
  (`limit` vs `inflight`). Alert when `limit` drops below 2 for more than 5
  minutes.
- **Heartbeat freshness**: track `now() - agents.last_heartbeat`. Alert when
  >90 seconds for >5% of active agents.

## Troubleshooting
- **401/403 responses**: rotate agent credentials via `/register` and verify the
  updated `auth_token` in `agents`.
- **Missing leases**: ensure new work has been inserted into `tasks` with
  `status = 'queued'` and check `lease_until` deadlines.
- **Metrics backlog**: confirm the retention job is running; the background
  `TimerService` logs warnings if cleanup fails. Manual cleanup:
  ```sql
  DELETE FROM agent_metrics WHERE recorded_at < datetime('now', '-30 days');
  DELETE FROM concurrency_windows WHERE expires_at < CURRENT_TIMESTAMP;
  ```
