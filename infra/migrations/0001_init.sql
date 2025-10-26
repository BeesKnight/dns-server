CREATE TABLE IF NOT EXISTS sequences (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);

INSERT INTO sequences(name, value) VALUES ('agent_id', 0)
    ON CONFLICT(name) DO NOTHING;
INSERT INTO sequences(name, value) VALUES ('lease_id', 0)
    ON CONFLICT(name) DO NOTHING;
INSERT INTO sequences(name, value) VALUES ('task_id', 0)
    ON CONFLICT(name) DO NOTHING;
INSERT INTO sequences(name, value) VALUES ('metric_id', 0)
    ON CONFLICT(name) DO NOTHING;

CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY,
    hostname TEXT NOT NULL UNIQUE,
    auth_token TEXT NOT NULL,
    lease_duration_ms INTEGER NOT NULL,
    heartbeat_timeout_ms INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_heartbeat TIMESTAMP,
    disabled BOOLEAN NOT NULL DEFAULT 0,
    revoked BOOLEAN NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    spec TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS leases (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    spec TEXT NOT NULL,
    lease_until TIMESTAMP NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(agent_id, task_id)
);

CREATE TABLE IF NOT EXISTS agent_metrics (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL,
    payload TEXT NOT NULL,
    recorded_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_configs (
    revision TEXT PRIMARY KEY,
    issued_at TIMESTAMP NOT NULL,
    expires_at TIMESTAMP NOT NULL,
    payload TEXT NOT NULL,
    signature TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS concurrency_windows (
    scope TEXT NOT NULL,
    kind TEXT,
    target INTEGER,
    "limit" INTEGER NOT NULL,
    inflight INTEGER NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP NOT NULL,
    PRIMARY KEY(scope, kind, target)
);

