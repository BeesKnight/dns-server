use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePoolOptions;
use tracing::{info, warn};

use dns_agent::http;
use dns_agent::runtime::{middleware::SharedRateLimiter, migrations::MIGRATOR, TimerService};
use dns_agent::workers::storage::AgentStore;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let migrate_only = env::args().any(|arg| arg == "--migrate-only");

    let bind_addr: SocketAddr = env::var("CONTROL_PLANE_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()
        .context("не удалось распарсить CONTROL_PLANE_BIND")?;

    let database_url = env::var("CONTROL_PLANE_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://control-plane.db".to_string());

    let pool = SqlitePoolOptions::new()
        .max_connections(
            env::var("CONTROL_PLANE_POOL_SIZE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(5),
        )
        .connect(&database_url)
        .await
        .with_context(|| format!("подключение к базе данных {database_url}"))?;

    MIGRATOR
        .run(&pool)
        .await
        .context("применение SQL миграций из infra/migrations")?;

    if migrate_only {
        warn!("запрошен только прогон миграций, завершаем работу");
        return Ok(());
    }

    let timer = TimerService::new();
    let store = Arc::new(AgentStore::new(pool, timer));
    let limiter = Arc::new(SharedRateLimiter::new());
    let state = http::AppState::new(store, limiter);
    let app = http::router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("не удалось открыть сокет на {bind_addr}"))?;
    info!(%bind_addr, "control plane слушает HTTP");

    axum::serve(listener, app.into_make_service())
        .await
        .context("ошибка при обслуживании HTTP-сервера")?;

    Ok(())
}
