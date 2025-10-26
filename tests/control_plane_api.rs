use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePoolOptions;
use tower::ServiceExt;

use dns_agent::control_plane::{
    ClaimRequest, ClaimResponse, ExtendRequest, HeartbeatRequest, LeaseReport, RegisterRequest,
    RegisterResponse, ReportRequest, ReportResponse, TaskKind, TaskSpec,
};
use dns_agent::http::{self, error::ErrorEnvelope};
use dns_agent::runtime::{middleware::SharedRateLimiter, migrations::MIGRATOR, TimerService};
use dns_agent::workers::storage::{AgentStore, MetricSample, MetricsEnvelope};

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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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
                    json!(HeartbeatRequest {
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
    let response: ClaimResponse = serde_json::from_slice(&bytes).unwrap();
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
async fn register_endpoint_accessible_under_versioned_namespace() {
    let (app, _) = setup_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agents/register")
                .header("content-type", "application/json")
                .body(Body::from(json!(RegisterRequest::default()).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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
                        json!(HeartbeatRequest {
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
                    json!(HeartbeatRequest {
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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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

#[tokio::test]
async fn extend_report_and_metrics_flow() {
    let (app, store) = setup_app().await;

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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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
    let claim: ClaimResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(claim.leases.len(), 1);
    let lease_id = claim.leases[0].lease_id;

    let extend_body = json!(ExtendRequest {
        agent_id: register.agent_id,
        lease_ids: vec![lease_id],
        extend_by_ms: 30_000,
    });
    let extend = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/extend")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::from(extend_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(extend.status(), StatusCode::OK);
    let bytes = body::to_bytes(extend.into_body(), usize::MAX)
        .await
        .unwrap();
    let extend_response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(extend_response["revoked"], json!([]));
    assert_eq!(extend_response["outcomes"].as_array().unwrap().len(), 1);

    let report_body = json!(ReportRequest {
        agent_id: register.agent_id,
        completed: vec![LeaseReport {
            lease_id,
            observations: Vec::new(),
        }],
        cancelled: Vec::new(),
    });
    let report = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/report")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token.clone())
                .body(Body::from(report_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(report.status(), StatusCode::OK);
    let bytes = body::to_bytes(report.into_body(), usize::MAX)
        .await
        .unwrap();
    let acknowledged: ReportResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(acknowledged.acknowledged, 1);

    let metrics_body = json!(MetricsEnvelope {
        agent_id: 0,
        timestamp: Utc::now(),
        counters: vec![MetricSample {
            name: "http.latency_p50".into(),
            value: 12.3,
            unit: Some("ms".into()),
        }],
        gauges: Vec::new(),
    });
    let metrics = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/metrics")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token)
                .body(Body::from(metrics_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::ACCEPTED);
    let bytes = body::to_bytes(metrics.into_body(), usize::MAX)
        .await
        .unwrap();
    let accepted: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(accepted["accepted"], Value::Bool(true));

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_metrics")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn extend_rejects_mismatched_agent() {
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
    let register: RegisterResponse = serde_json::from_slice(&bytes).unwrap();

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
    let claim: ClaimResponse = serde_json::from_slice(&bytes).unwrap();
    let lease_id = claim.leases[0].lease_id;

    let extend_body = json!(ExtendRequest {
        agent_id: register.agent_id + 1,
        lease_ids: vec![lease_id],
        extend_by_ms: 1_000,
    });
    let extend = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/extend")
                .header("content-type", "application/json")
                .header("x-agent-id", register.agent_id.to_string())
                .header("x-agent-token", register.auth_token)
                .body(Body::from(extend_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(extend.status(), StatusCode::FORBIDDEN);
    let bytes = body::to_bytes(extend.into_body(), usize::MAX)
        .await
        .unwrap();
    let envelope: ErrorEnvelope = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope.error, "ERR_FORBIDDEN");
}

#[tokio::test]
async fn protected_route_requires_headers() {
    let (app, _) = setup_app().await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let envelope: ErrorEnvelope = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope.error, "ERR_UNAUTHORIZED");
}
