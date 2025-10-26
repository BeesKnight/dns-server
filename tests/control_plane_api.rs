use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

use dns_agent::control_plane::{ClaimRequest, RegisterRequest, TaskKind, TaskSpec};
use dns_agent::http::{self, error::ErrorEnvelope};
use dns_agent::runtime::{middleware::SharedRateLimiter, migrations::MIGRATOR, TimerService};
use dns_agent::workers::storage::AgentStore;

async fn setup_app() -> (Router, Arc<AgentStore>) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    MIGRATOR.run(&pool).await.expect("migrations");
    let timer = TimerService::new();
    let store = Arc::new(AgentStore::new(pool.clone(), timer));
    store
        .enqueue_task(
            TaskKind::Dns,
            &TaskSpec::Dns {
                query: "example.com".into(),
                server: None,
            },
        )
        .await
        .expect("seed task");
    sqlx::query(
        "INSERT INTO agent_configs (revision, issued_at, expires_at, payload, signature) VALUES (?, ?, ?, ?, ?)",
    )
    .bind("v1")
    .bind(chrono::Utc::now())
    .bind(chrono::Utc::now() + chrono::Duration::hours(1))
    .bind("{}")
    .bind("sig")
    .execute(&pool)
    .await
    .expect("insert config");

    let limiter = Arc::new(SharedRateLimiter::new());
    let app_state = http::AppState::new(store.clone(), limiter);
    (http::router(app_state), store)
}

#[tokio::test]
async fn register_and_heartbeat_flow() {
    let (app, _) = setup_app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from(json!(RegisterRequest::default()).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let register: dns_agent::control_plane::RegisterResponse =
        serde_json::from_slice(&bytes).unwrap();

    let heartbeat = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/heartbeat")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::from(
                    json!(dns_agent::control_plane::HeartbeatRequest {
                        agent_id: register.agent_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(heartbeat.status(), StatusCode::OK);

    let claim_body = json!(ClaimRequest {
        agent_id: register.agent_id,
        capacities: [(TaskKind::Dns, 1)].into_iter().collect(),
    });
    let claim = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/claim")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::from(claim_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(claim.status(), StatusCode::OK);
    let bytes = body::to_bytes(claim.into_body(), usize::MAX).await.unwrap();
    let response: dns_agent::control_plane::ClaimResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response.leases.len(), 1);

    // Config should be served
    let config_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(config_response.status(), StatusCode::OK);
    let etag = config_response.headers().get(header::ETAG).cloned();
    assert!(etag.is_some());
}

#[tokio::test]
async fn rate_limit_is_enforced() {
    let (app, _) = setup_app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let register: dns_agent::control_plane::RegisterResponse =
        serde_json::from_slice(&bytes).unwrap();

    for _ in 0..3 {
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/heartbeat")
                    .header("content-type", "application/json")
                    .header("x-agent-id", register.agent_id.to_string())
                    .header("x-agent-token", register.auth_token.clone())
                    .body(Body::from(
                        json!(dns_agent::control_plane::HeartbeatRequest {
                            agent_id: register.agent_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    let limited = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/heartbeat")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::from(
                    json!(dns_agent::control_plane::HeartbeatRequest {
                        agent_id: register.agent_id
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = body::to_bytes(limited.into_body(), usize::MAX)
        .await
        .unwrap();
    let envelope: ErrorEnvelope = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope.error, "ERR_RATE_LIMITED");
}

#[tokio::test]
async fn config_not_modified_uses_etag() {
    let (app, _) = setup_app().await;
    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = body::to_bytes(register.into_body(), usize::MAX)
        .await
        .unwrap();
    let register: dns_agent::control_plane::RegisterResponse =
        serde_json::from_slice(&bytes).unwrap();

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag = first.headers().get(header::ETAG).unwrap().to_str().unwrap();

    let second = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token)
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn claim_throughput_smoke() {
    let (app, store) = setup_app().await;
    for _ in 0..10 {
        store
            .enqueue_task(
                TaskKind::Dns,
                &TaskSpec::Dns {
                    query: "bench.example".into(),
                    server: None,
                },
            )
            .await
            .unwrap();
    }

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/register")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = body::to_bytes(register.into_body(), usize::MAX)
        .await
        .unwrap();
    let register: dns_agent::control_plane::RegisterResponse =
        serde_json::from_slice(&bytes).unwrap();

    let start = Instant::now();
    for _ in 0..5 {
        let claim_body = json!(ClaimRequest {
            agent_id: register.agent_id,
            capacities: [(TaskKind::Dns, 2)].into_iter().collect(),
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/claim")
                    .header("content-type", "application/json")
                    .header("x-agent-id", register.agent_id.to_string())
                    .header("x-agent-token", register.auth_token.clone())
                    .body(Body::from(claim_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5));
}
