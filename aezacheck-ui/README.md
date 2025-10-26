# AezaCheck UI

This package contains the frontend shell for the AezaCheck operator console. It
is implemented with React + TypeScript (Vite) and renders live telemetry from
the jobs control plane.

## Map telemetry stream

The map view subscribes to the jobs service Server-Sent Events (SSE) endpoint
`/v1/map/events`. When creating an `EventSource`, append the `access_token`
query parameter – browsers omit `Authorization` headers for EventSource
connections. Example:

```ts
const source = new EventSource(`${apiBase}/v1/map/events?access_token=${token}`);
```

The stream sends keepalive comments every 15 seconds. Individual events are
JSON payloads with these types:

| Type           | Description                                                                  |
| -------------- | ---------------------------------------------------------------------------- |
| `agent.online` | Agent heartbeat/registration. `data.agent` contains `id`, `ip`, `geo`.       |
| `check.start`  | A check has been leased. `data.source/target.geo` contain the geolocation.   |
| `check.result` | Intermediate result with status (`ok`, `error`, `cancelled`, etc.).          |
| `check.done`   | Final completion marker. Includes `status` and the originating source geo.   |

The SLA for the UI is sub-second delivery from publish to UI receipt and a
maximum keepalive interval of 15 seconds.

## Local development

Install dependencies and run the dev server:

```bash
npm install
npm run dev
```

### Runtime configuration

The frontend reads the following environment variables (all prefixed with
`VITE_`). Create a `.env.local` file or export them in your shell:

| Variable | Description | Default |
| --- | --- | --- |
| `VITE_API_BASE` | Base URL for API requests in production builds. | `/v1` |
| `VITE_API_PROXY_TARGET` | Target URL for the dev server proxy (`/api/*`). When set, the dev server forwards API calls to this host. | `http://localhost:8088` |
| `VITE_USE_PROXY` | Force-enable the `/api` proxy even without `VITE_API_PROXY_TARGET`. Set to `true` when the backend lives on a different origin. | `false` |
| `VITE_API_TIMEOUT` | Request timeout in milliseconds for the typed API client. | `15000` |
| `VITE_API_RETRY_LIMIT` | Number of automatic retries for idempotent requests. | `2` |
| `VITE_DEV_PORT` | Port for `npm run dev`. | `5173` |

In development the UI automatically calls `/api/*`; configure the proxy target to
forward requests to your backend, or set `VITE_API_BASE` to a full URL to bypass
the proxy.

### Agent operations console

The `/app/agents` route exposes the agent operations dashboard:

- Virtualised list for large agent inventories with search, status filtering and
  optimistic status changes.
- Task status board with aggregated progress per state.
- Scrollable interaction history with incremental pagination.
- Request cancellation buttons and error notifications for observability.

### Testing

Unit tests use [Vitest](https://vitest.dev):

```bash
npm run test
```

End-to-end smoke tests use [Playwright](https://playwright.dev). They start a dev
server automatically and stub backend responses:

```bash
npm run test:e2e
```

### Manual verification

1. `npm run dev` and open `http://localhost:5173/app/agents`.
2. Inspect that the agent list paginates and virtualises while filtering works.
3. Trigger a status change from the dropdown and confirm the UI updates
   optimistically.
4. Click “Загрузить ещё” in the history panel and verify new items append
   without losing previous entries.
