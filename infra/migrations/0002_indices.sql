CREATE INDEX IF NOT EXISTS idx_agents_last_heartbeat ON agents(last_heartbeat);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_leases_status ON leases(status);
CREATE INDEX IF NOT EXISTS idx_leases_deadline ON leases(lease_until);
CREATE INDEX IF NOT EXISTS idx_metrics_recorded ON agent_metrics(recorded_at);
CREATE INDEX IF NOT EXISTS idx_concurrency_expires ON concurrency_windows(expires_at);

