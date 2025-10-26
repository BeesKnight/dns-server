use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sqlx::sqlite::SqlitePoolOptions;
use tracing::{info, warn};

use dns_agent::control_plane::RegisterRequest;
use dns_agent::http;
use dns_agent::runtime::{middleware::SharedRateLimiter, migrations::MIGRATOR, TimerService};
use dns_agent::workers::storage::AgentStore;

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Применить миграции и завершить работу
    #[arg(long)]
    migrate_only: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Создать запись агента вручную и вывести креды
    CreateAgent {
        /// Необязательное имя хоста для агента
        #[arg(value_name = "HOSTNAME")]
        hostname: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_to_serving() {
        let cli = Cli::parse_from(["control-plane"]);
        assert!(!cli.migrate_only);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_create_agent_without_hostname() {
        let cli = Cli::parse_from(["control-plane", "create-agent"]);
        match cli.command {
            Some(Commands::CreateAgent { hostname }) => assert!(hostname.is_none()),
            _ => panic!("ожидалась подкоманда create-agent"),
        }
    }

    #[test]
    fn parse_create_agent_with_hostname() {
        let cli = Cli::parse_from(["control-plane", "create-agent", "dns-a"]);
        match cli.command {
            Some(Commands::CreateAgent { hostname }) => {
                assert_eq!(hostname.as_deref(), Some("dns-a"));
            }
            _ => panic!("ожидалась подкоманда create-agent"),
        }
    }

    #[test]
    fn parse_migrate_only_flag() {
        let cli = Cli::parse_from(["control-plane", "--migrate-only"]);
        assert!(cli.migrate_only);
        assert!(cli.command.is_none());
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    if cli.migrate_only && cli.command.is_some() {
        bail!("--migrate-only нельзя совмещать с подкомандами");
    }

    let bind_addr: SocketAddr = std::env::var("CONTROL_PLANE_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()
        .context("не удалось распарсить CONTROL_PLANE_BIND")?;

    let database_url = std::env::var("CONTROL_PLANE_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://control-plane.db".to_string());

    let pool = SqlitePoolOptions::new()
        .max_connections(
            std::env::var("CONTROL_PLANE_POOL_SIZE")
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

    if cli.migrate_only {
        warn!("запрошен только прогон миграций, завершаем работу");
        return Ok(());
    }

    let timer = TimerService::new();
    let store = AgentStore::new(pool, timer.clone());

    if let Some(Commands::CreateAgent { hostname }) = cli.command {
        let response = store.register(RegisterRequest { hostname }).await?;

        println!("Создан агент:");
        println!("  ID: {}", response.agent_id);
        println!("  Token: {}", response.auth_token);
        println!("  Lease duration (ms): {}", response.lease_duration_ms);
        println!(
            "  Heartbeat timeout (ms): {}",
            response.heartbeat_timeout_ms
        );
        println!("\nСохраните значения в AGENT_ID и AGENT_AUTH_TOKEN для агента.");

        return Ok(());
    }

    let store = Arc::new(store);
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
