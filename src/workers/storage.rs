use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, SqlitePool};
use tokio::sync::RwLock;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use crate::concurrency::{ConcurrencyPersistence, ConcurrencyStateStore};
use crate::control_plane::{
    ClaimRequest, ClaimResponse, ExtendOutcome, ExtendRequest, HeartbeatResponse, Lease,
    LeaseReport, RegisterRequest, RegisterResponse, ReportRequest, ReportResponse, TaskKind,
    TaskSpec,
};
use crate::runtime::TimerService;

const DEFAULT_LEASE_DURATION_MS: i64 = 60_000;
const DEFAULT_HEARTBEAT_TIMEOUT_MS: i64 = 45_000;
const METRICS_RETENTION: i64 = 30; // days

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsEnvelope {
    pub agent_id: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub counters: Vec<MetricSample>,
    #[serde(default)]
    pub gauges: Vec<MetricSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSample {
    pub name: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigEnvelope {
    pub revision: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub config: serde_json::Value,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub agent_id: u64,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("agent not found")]
    AgentNotFound,
    #[error("agent revoked")]
    AgentRevoked,
    #[error("agent disabled")]
    AgentDisabled,
    #[error("lease not found")]
    LeaseNotFound,
    #[error("task not found")]
    TaskNotFound,
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type StorageResult<T> = std::result::Result<T, StorageError>;

#[derive(Clone)]
pub struct AgentStore {
    pool: SqlitePool,
    timer: TimerService,
    maintenance_guard: Arc<RwLock<()>>,
}

impl AgentStore {
    pub fn new(pool: SqlitePool, timer: TimerService) -> Self {
        let store = Self {
            pool,
            timer: timer.clone(),
            maintenance_guard: Arc::new(RwLock::new(())),
        };
        store.spawn_retention();
        store
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    fn spawn_retention(&self) {
        let pool = self.pool.clone();
        let timer = self.timer.clone();
        let guard = self.maintenance_guard.clone();
        tokio::spawn(async move {
            loop {
                if timer.sleep(StdDuration::from_secs(3600)).await.is_err() {
                    break;
                }
                let _lock = guard.read().await;
                if let Err(err) = cleanup_metrics(&pool).await {
                    warn!(error = %err, "failed to cleanup metrics");
                }
                if let Err(err) = cleanup_concurrency_windows(&pool).await {
                    warn!(error = %err, "failed to cleanup concurrency windows");
                }
            }
        });
    }

    async fn next_sequence_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        name: &str,
    ) -> StorageResult<i64> {
        let row = sqlx::query(
            "INSERT INTO sequences(name, value) VALUES (?, 1)\n             ON CONFLICT(name) DO UPDATE SET value = value + 1\n             RETURNING value",
        )
        .bind(name)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.try_get::<i64, _>("value")?)
    }

    async fn agent_defaults(&self) -> (i64, i64) {
        (DEFAULT_LEASE_DURATION_MS, DEFAULT_HEARTBEAT_TIMEOUT_MS)
    }

    #[instrument(skip(self, request))]
    pub async fn register(&self, request: RegisterRequest) -> StorageResult<RegisterResponse> {
        let hostname = match request.hostname {
            Some(value) if !value.is_empty() => value,
            _ => hostname::get()
                .map(|s| s.to_string_lossy().into_owned())
                .map_err(|err| StorageError::Other(anyhow!(err)))?,
        };

        let mut conn = self.pool.acquire().await?;
        if let Some(row) = sqlx::query(
            "SELECT id, lease_duration_ms, heartbeat_timeout_ms FROM agents WHERE hostname = ?",
        )
        .bind(&hostname)
        .fetch_optional(&mut *conn)
        .await?
        {
            let agent_id: i64 = row.try_get("id")?;
            let token = Uuid::new_v4().to_string();
            sqlx::query(
                "UPDATE agents SET auth_token = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            )
            .bind(&token)
            .bind(agent_id)
            .execute(&mut *conn)
            .await?;
            let lease_duration_ms: i64 = row.try_get("lease_duration_ms")?;
            let heartbeat_timeout_ms: i64 = row.try_get("heartbeat_timeout_ms")?;
            return Ok(RegisterResponse {
                agent_id: agent_id as u64,
                lease_duration_ms: lease_duration_ms as u64,
                heartbeat_timeout_ms: heartbeat_timeout_ms as u64,
                auth_token: token,
            });
        }
        drop(conn);
        let (lease_duration_ms, heartbeat_timeout_ms) = self.agent_defaults().await;
        let mut tx = self.pool.begin().await?;
        let agent_id = self.next_sequence_tx(&mut tx, "agent_id").await?;
        let token = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO agents (id, hostname, auth_token, lease_duration_ms, heartbeat_timeout_ms)\n             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(agent_id)
        .bind(&hostname)
        .bind(&token)
        .bind(lease_duration_ms)
        .bind(heartbeat_timeout_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        info!(hostname, agent_id, "registered agent");
        Ok(RegisterResponse {
            agent_id: agent_id as u64,
            lease_duration_ms: lease_duration_ms as u64,
            heartbeat_timeout_ms: heartbeat_timeout_ms as u64,
            auth_token: token,
        })
    }

    #[instrument(skip(self))]
    pub async fn touch_heartbeat(&self, agent_id: u64) -> StorageResult<HeartbeatResponse> {
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query("SELECT heartbeat_timeout_ms, revoked, disabled FROM agents WHERE id = ?")
                .bind(agent_id as i64)
                .fetch_optional(&mut *tx)
                .await?;
        let row = row.ok_or(StorageError::AgentNotFound)?;
        let revoked: i64 = row.try_get("revoked")?;
        if revoked != 0 {
            return Err(StorageError::AgentRevoked);
        }
        let disabled: i64 = row.try_get("disabled")?;
        if disabled != 0 {
            return Err(StorageError::AgentDisabled);
        }
        sqlx::query(
            "UPDATE agents SET last_heartbeat = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(agent_id as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let heartbeat_timeout_ms: i64 = row.try_get("heartbeat_timeout_ms")?;
        Ok(HeartbeatResponse {
            agent_id,
            next_deadline_ms: heartbeat_timeout_ms as u64,
        })
    }

    pub async fn authenticate_credentials(
        &self,
        agent_id: u64,
        token: &str,
    ) -> StorageResult<AgentIdentity> {
        let mut conn = self.pool.acquire().await?;
        let row = sqlx::query("SELECT auth_token, revoked, disabled FROM agents WHERE id = ?")
            .bind(agent_id as i64)
            .fetch_optional(&mut *conn)
            .await?;
        let row = row.ok_or(StorageError::AgentNotFound)?;
        let stored: String = row.try_get("auth_token")?;
        if stored != token {
            return Err(StorageError::Forbidden("invalid token"));
        }
        let revoked: i64 = row.try_get("revoked")?;
        if revoked != 0 {
            return Err(StorageError::AgentRevoked);
        }
        let disabled: i64 = row.try_get("disabled")?;
        if disabled != 0 {
            return Err(StorageError::AgentDisabled);
        }
        Ok(AgentIdentity {
            agent_id,
            token: stored,
        })
    }

    #[instrument(skip(self, request))]
    pub async fn claim(&self, request: ClaimRequest) -> StorageResult<ClaimResponse> {
        let mut tx = self.pool.begin().await?;
        let agent_row = sqlx::query("SELECT lease_duration_ms FROM agents WHERE id = ?")
            .bind(request.agent_id as i64)
            .fetch_optional(&mut *tx)
            .await?;
        let agent_row = agent_row.ok_or(StorageError::AgentNotFound)?;
        let lease_duration_ms: i64 = agent_row.try_get("lease_duration_ms")?;
        let mut leases = Vec::new();
        let lease_deadline = Utc::now() + Duration::milliseconds(lease_duration_ms);

        for (kind, capacity) in request.capacities.into_iter() {
            if capacity == 0 {
                continue;
            }
            let rows = sqlx::query(
                "SELECT id, spec FROM tasks WHERE kind = ? AND status = 'queued' ORDER BY id LIMIT ?",
            )
            .bind(kind_label(kind))
            .bind(capacity as i64)
            .fetch_all(&mut *tx)
            .await?;
            for row in rows {
                let task_id: i64 = row.try_get("id")?;
                let spec_json: String = row.try_get("spec")?;
                let spec: TaskSpec = serde_json::from_str(&spec_json)?;
                let lease_id = self.next_sequence_tx(&mut tx, "lease_id").await?;
                sqlx::query(
                    "INSERT INTO leases (id, agent_id, task_id, kind, spec, lease_until, status)\n                     VALUES (?, ?, ?, ?, ?, ?, 'leased')",
                )
                .bind(lease_id)
                .bind(request.agent_id as i64)
                .bind(task_id)
                .bind(kind_label(kind))
                .bind(&spec_json)
                .bind(lease_deadline)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE tasks SET status = 'leased', updated_at = CURRENT_TIMESTAMP WHERE id = ?")
                    .bind(task_id)
                    .execute(&mut *tx)
                    .await?;
                leases.push(Lease {
                    lease_id: lease_id as u64,
                    task_id: task_id as u64,
                    kind,
                    lease_until_ms: lease_duration_ms as u64,
                    spec,
                });
            }
        }

        tx.commit().await?;
        Ok(ClaimResponse { leases })
    }

    #[instrument(skip(self, request))]
    pub async fn extend(
        &self,
        request: ExtendRequest,
    ) -> StorageResult<(Vec<ExtendOutcome>, Vec<u64>)> {
        let mut tx = self.pool.begin().await?;
        let mut outcomes = Vec::new();
        let mut revoked = Vec::new();
        let new_deadline = Utc::now() + Duration::milliseconds(request.extend_by_ms as i64);
        for lease_id in request.lease_ids {
            let row = sqlx::query("SELECT status FROM leases WHERE id = ? AND agent_id = ?")
                .bind(lease_id as i64)
                .bind(request.agent_id as i64)
                .fetch_optional(&mut *tx)
                .await?;
            match row {
                Some(row) => {
                    let status: String = row.try_get("status")?;
                    if status == "leased" {
                        sqlx::query(
                            "UPDATE leases SET lease_until = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                        )
                        .bind(new_deadline)
                        .bind(lease_id as i64)
                        .execute(&mut *tx)
                        .await?;
                        outcomes.push(ExtendOutcome {
                            lease_id,
                            new_deadline_ms: request.extend_by_ms,
                        });
                    } else {
                        revoked.push(lease_id);
                    }
                }
                None => revoked.push(lease_id),
            }
        }
        tx.commit().await?;
        Ok((outcomes, revoked))
    }

    #[instrument(skip(self, request))]
    pub async fn report(&self, request: ReportRequest) -> StorageResult<ReportResponse> {
        let mut tx = self.pool.begin().await?;
        let mut acknowledged = 0usize;
        for report in request.completed.iter() {
            acknowledged += self
                .finalize_lease(&mut tx, request.agent_id, report, "completed")
                .await?;
        }
        for report in request.cancelled.iter() {
            acknowledged += self
                .finalize_lease(&mut tx, request.agent_id, report, "cancelled")
                .await?;
        }
        tx.commit().await?;
        Ok(ReportResponse { acknowledged })
    }

    #[instrument(skip(self, metrics))]
    pub async fn ingest_metrics(&self, metrics: MetricsEnvelope) -> StorageResult<String> {
        let mut tx = self.pool.begin().await?;
        let metric_id = self.next_sequence_tx(&mut tx, "metric_id").await?;
        let payload = serde_json::to_string(&metrics)?;
        sqlx::query(
            "INSERT INTO agent_metrics (id, agent_id, payload, recorded_at) VALUES (?, ?, ?, ?)",
        )
        .bind(metric_id)
        .bind(metrics.agent_id as i64)
        .bind(&payload)
        .bind(metrics.timestamp)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Uuid::new_v4().to_string())
    }

    #[instrument(skip(self))]
    pub async fn latest_config(&self) -> StorageResult<Option<AgentConfigEnvelope>> {
        let mut conn = self.pool.acquire().await?;
        let row = sqlx::query(
            "SELECT revision, issued_at, expires_at, payload, signature\n             FROM agent_configs\n             ORDER BY issued_at DESC\n             LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = row {
            let payload: String = row.try_get("payload")?;
            let config = serde_json::from_str(&payload)?;
            let envelope = AgentConfigEnvelope {
                revision: row.try_get("revision")?,
                issued_at: row.try_get("issued_at")?,
                expires_at: row.try_get("expires_at")?,
                config,
                signature: row.try_get("signature")?,
            };
            return Ok(Some(envelope));
        }
        Ok(None)
    }

    #[instrument(skip(self, kind, spec))]
    pub async fn enqueue_task(&self, kind: TaskKind, spec: &TaskSpec) -> StorageResult<u64> {
        let mut tx = self.pool.begin().await?;
        let task_id = self.next_sequence_tx(&mut tx, "task_id").await?;
        let payload = serde_json::to_string(spec)?;
        sqlx::query("INSERT INTO tasks (id, kind, spec, status) VALUES (?, ?, ?, 'queued')")
            .bind(task_id)
            .bind(kind_label(kind))
            .bind(&payload)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(task_id as u64)
    }

    async fn finalize_lease(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        agent_id: u64,
        report: &LeaseReport,
        status: &str,
    ) -> StorageResult<usize> {
        let row = sqlx::query("SELECT id FROM leases WHERE id = ? AND agent_id = ?")
            .bind(report.lease_id as i64)
            .bind(agent_id as i64)
            .fetch_optional(&mut **tx)
            .await?;
        if row.is_none() {
            return Ok(0);
        }
        sqlx::query("UPDATE leases SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(status)
            .bind(report.lease_id as i64)
            .execute(&mut **tx)
            .await?;
        Ok(1)
    }

    pub fn concurrency_persistence(&self, ttl: StdDuration) -> ConcurrencyPersistence {
        let store = self.concurrency_store(ttl);
        ConcurrencyPersistence::new(store)
    }

    pub fn concurrency_store(&self, ttl: StdDuration) -> ConcurrencyStateStore {
        ConcurrencyStateStore::new(self.pool.clone(), ttl)
    }
}

fn kind_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Dns => "dns",
        TaskKind::Http => "http",
        TaskKind::Tcp => "tcp",
        TaskKind::Ping => "ping",
        TaskKind::Trace => "trace",
    }
}

async fn cleanup_metrics(pool: &SqlitePool) -> Result<()> {
    let cutoff = Utc::now() - Duration::days(METRICS_RETENTION);
    sqlx::query("DELETE FROM agent_metrics WHERE recorded_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

async fn cleanup_concurrency_windows(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM concurrency_windows WHERE expires_at < CURRENT_TIMESTAMP")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_rotates_token() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        {
            let mut conn = pool.acquire().await.expect("acquire");
            crate::runtime::migrations::MIGRATOR
                .run(&mut conn)
                .await
                .expect("migrations");
        }
        let timer = TimerService::new();
        let store = AgentStore::new(pool.clone(), timer);
        let first = store
            .register(RegisterRequest {
                hostname: Some("agent.test".into()),
            })
            .await
            .expect("register");
        let second = store
            .register(RegisterRequest {
                hostname: Some("agent.test".into()),
            })
            .await
            .expect("register again");
        assert_eq!(first.agent_id, second.agent_id);
        assert_ne!(first.auth_token, second.auth_token);
    }
}
