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

const DEFAULT_CONTROL_PLANE_URL: &str = "http://127.0.0.1:8088/v1/agents";

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
        /// Базовый URL control plane (по умолчанию http://127.0.0.1:8088/v1/agents)
        #[arg(long, env = "CONTROL_PLANE_BASE_URL")]
        base_url: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    static ENV_GUARD: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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
            Some(Commands::CreateAgent { hostname, base_url }) => {
                assert!(hostname.is_none());
                assert!(base_url.is_none());
            }
            _ => panic!("ожидалась подкоманда create-agent"),
        }
    }

    #[test]
    fn parse_create_agent_with_hostname() {
        let cli = Cli::parse_from(["control-plane", "create-agent", "dns-a"]);
        match cli.command {
            Some(Commands::CreateAgent { hostname, base_url }) => {
                assert_eq!(hostname.as_deref(), Some("dns-a"));
                assert!(base_url.is_none());
            }
            _ => panic!("ожидалась подкоманда create-agent"),
        }
    }

    #[test]
    fn parse_create_agent_with_base_url() {
        let cli = Cli::parse_from([
            "control-plane",
            "create-agent",
            "--base-url",
            "http://cp.local/v1/agents",
        ]);
        match cli.command {
            Some(Commands::CreateAgent { hostname, base_url }) => {
                assert!(hostname.is_none());
                assert_eq!(base_url.as_deref(), Some("http://cp.local/v1/agents"));
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

    #[test]
    fn resolve_base_url_defaults() {
        let _guard = ENV_GUARD.lock().unwrap();
        let original = std::env::var("AGENT_CONTROL_PLANE").ok();
        std::env::remove_var("AGENT_CONTROL_PLANE");

        let resolved = super::resolve_base_url(None).unwrap();
        assert_eq!(resolved, DEFAULT_CONTROL_PLANE_URL);

        if let Some(value) = original {
            std::env::set_var("AGENT_CONTROL_PLANE", value);
        } else {
            std::env::remove_var("AGENT_CONTROL_PLANE");
        }
    }

    #[test]
    fn resolve_base_url_prefers_agent_env() {
        let _guard = ENV_GUARD.lock().unwrap();
        let original = std::env::var("AGENT_CONTROL_PLANE").ok();
        std::env::set_var("AGENT_CONTROL_PLANE", "http://cp.internal/v1/agents");

        let resolved = super::resolve_base_url(None).unwrap();
        assert_eq!(resolved, "http://cp.internal/v1/agents");

        if let Some(value) = original {
            std::env::set_var("AGENT_CONTROL_PLANE", value);
        } else {
            std::env::remove_var("AGENT_CONTROL_PLANE");
        }
    }

    #[tokio::test]
    async fn create_agent_via_http_calls_register_endpoint() {
        use axum::{routing::post, Json, Router};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let recorded = Arc::new(Mutex::new(Vec::<RegisterRequest>::new()));
        let shared = recorded.clone();

        let app = Router::new().route(
            "/v1/agents/register",
            post(move |Json(payload): Json<RegisterRequest>| {
                let shared = shared.clone();
                async move {
                    let mut guard = shared.lock().await;
                    guard.push(payload.clone());
                    drop(guard);
                    Json(dns_agent::control_plane::RegisterResponse {
                        agent_id: 7,
                        auth_token: "secret".into(),
                        lease_duration_ms: 15_000,
                        heartbeat_timeout_ms: 5_000,
                    })
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://{}/v1/agents", addr);
        let response = super::create_agent_via_http(
            &base_url,
            RegisterRequest {
                hostname: Some("manual".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(response.agent_id, 7);
        assert_eq!(response.auth_token, "secret");

        let guard = recorded.lock().await;
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].hostname.as_deref(), Some("manual"));

        server.abort();
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
    let migrate_only = cli.migrate_only;
    let command = cli.command;

    if migrate_only && command.is_some() {
        bail!("--migrate-only нельзя совмещать с подкомандами");
    }

    if let Some(Commands::CreateAgent { hostname, base_url }) = command {
        return run_create_agent(hostname, base_url).await;
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

    if migrate_only {
        warn!("запрошен только прогон миграций, завершаем работу");
        return Ok(());
    }

    let timer = TimerService::new();
    let store = AgentStore::new(pool, timer.clone());

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

fn resolve_base_url(base_url: Option<String>) -> Result<String> {
    let resolved = base_url
        .or_else(|| std::env::var("AGENT_CONTROL_PLANE").ok())
        .unwrap_or_else(|| DEFAULT_CONTROL_PLANE_URL.to_string());

    let trimmed = resolved.trim();
    if trimmed.is_empty() {
        bail!("control plane URL не может быть пустым");
    }

    Ok(trimmed.to_string())
}

async fn create_agent_via_http(
    base_url: &str,
    request: RegisterRequest,
) -> Result<dns_agent::control_plane::RegisterResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("создание HTTP-клиента для control plane")?;

    let endpoint = if base_url.ends_with("/register") {
        base_url.to_string()
    } else {
        format!("{}/register", base_url.trim_end_matches('/'))
    };

    let response = client
        .post(&endpoint)
        .json(&request)
        .send()
        .await
        .with_context(|| format!("запрос регистрации агента в {endpoint}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<недоступно>".to_string());
        bail!("control plane вернул статус {status}: {body}");
    }

    response
        .json::<dns_agent::control_plane::RegisterResponse>()
        .await
        .context("разбор ответа регистрации агента")
}

async fn run_create_agent(hostname: Option<String>, base_url: Option<String>) -> Result<()> {
    let base_url = resolve_base_url(base_url)?;
    let request = RegisterRequest { hostname };
    let response = create_agent_via_http(&base_url, request).await?;

    println!("Создан агент:");
    println!("  ID: {}", response.agent_id);
    println!("  Token: {}", response.auth_token);
    println!("  Lease duration (ms): {}", response.lease_duration_ms);
    println!(
        "  Heartbeat timeout (ms): {}",
        response.heartbeat_timeout_ms
    );
    println!("\nСохраните значения в AGENT_ID и AGENT_AUTH_TOKEN для агента.");

    Ok(())
}
