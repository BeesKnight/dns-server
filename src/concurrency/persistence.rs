use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::warn;

use crate::control_plane::TaskKind;

use super::{kind_label, TargetId};

#[derive(Clone)]
pub struct ConcurrencyStateStore {
    pool: SqlitePool,
    ttl: Duration,
}

impl ConcurrencyStateStore {
    pub fn new(pool: SqlitePool, ttl: Duration) -> Self {
        Self { pool, ttl }
    }

    async fn upsert(
        &self,
        scope: &str,
        kind: Option<&str>,
        target: Option<u64>,
        limit: usize,
        inflight: usize,
    ) -> Result<(), sqlx::Error> {
        let expires_at = Utc::now()
            + ChronoDuration::from_std(self.ttl).unwrap_or_else(|_| ChronoDuration::seconds(300));
        sqlx::query(
            "INSERT INTO concurrency_windows (scope, kind, target, \"limit\", inflight, updated_at, expires_at)\n             VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP, ?)\n             ON CONFLICT(scope, kind, target) DO UPDATE SET\n             \"limit\" = excluded.\"limit\", inflight = excluded.inflight, updated_at = CURRENT_TIMESTAMP, expires_at = excluded.expires_at",
        )
        .bind(scope)
        .bind(kind)
        .bind(target.map(|value| value as i64))
        .bind(limit as i64)
        .bind(inflight as i64)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn cleanup(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM concurrency_windows WHERE expires_at < CURRENT_TIMESTAMP")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct ConcurrencyPersistence {
    tx: mpsc::UnboundedSender<PersistCommand>,
}

enum Scope {
    Global,
    Kind(String),
    Target { kind: String, target: u64 },
}

struct PersistedWindow {
    scope: Scope,
    limit: usize,
    inflight: usize,
}

enum PersistCommand {
    Update(PersistedWindow),
}

impl ConcurrencyPersistence {
    pub fn new(store: ConcurrencyStateStore) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(PersistCommand::Update(window)) = rx.recv().await {
                if let Err(err) = persist_window(&store, window).await {
                    warn!(error = %err, "failed to persist concurrency snapshot");
                }
            }
        });
        Self { tx }
    }

    pub fn record_global(&self, limit: usize, inflight: usize) {
        let _ = self.tx.send(PersistCommand::Update(PersistedWindow {
            scope: Scope::Global,
            limit,
            inflight,
        }));
    }

    pub fn record_kind(&self, kind: TaskKind, limit: usize, inflight: usize) {
        let _ = self.tx.send(PersistCommand::Update(PersistedWindow {
            scope: Scope::Kind(kind_label(kind).to_string()),
            limit,
            inflight,
        }));
    }

    pub fn record_target(&self, kind: TaskKind, target: TargetId, limit: usize, inflight: usize) {
        let _ = self.tx.send(PersistCommand::Update(PersistedWindow {
            scope: Scope::Target {
                kind: kind_label(kind).to_string(),
                target,
            },
            limit,
            inflight,
        }));
    }
}

async fn persist_window(
    store: &ConcurrencyStateStore,
    window: PersistedWindow,
) -> Result<(), sqlx::Error> {
    match window.scope {
        Scope::Global => {
            store
                .upsert("global", None, None, window.limit, window.inflight)
                .await?;
        }
        Scope::Kind(kind) => {
            store
                .upsert("kind", Some(&kind), None, window.limit, window.inflight)
                .await?;
        }
        Scope::Target { kind, target } => {
            store
                .upsert(
                    "target",
                    Some(&kind),
                    Some(target),
                    window.limit,
                    window.inflight,
                )
                .await?;
        }
    }
    store.cleanup().await?;
    Ok(())
}
