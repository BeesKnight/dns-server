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

Set `VITE_API_BASE_URL` in your `.env.local` so the frontend can reach the API
Gateway (for example, `http://localhost:8088`).
