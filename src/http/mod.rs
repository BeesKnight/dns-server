pub mod error;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::Serialize;

use crate::control_plane::{
    ClaimRequest, ClaimResponse, ExtendOutcome, ExtendRequest, HeartbeatRequest, HeartbeatResponse,
    RegisterRequest, RegisterResponse, ReportRequest, ReportResponse,
};
use crate::runtime::middleware::{
    enforce_rate_limit, require_auth, AuthContext, RateLimitContext, RateLimitPolicy,
    SharedRateLimiter,
};
use crate::runtime::telemetry::http_trace_layer;
use crate::workers::storage::{AgentIdentity, AgentStore, MetricsEnvelope};

use self::error::{ApiError, ApiResult};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<AgentStore>,
    pub limiter: Arc<SharedRateLimiter>,
}

impl AppState {
    pub fn new(store: Arc<AgentStore>, limiter: Arc<SharedRateLimiter>) -> Self {
        Self { store, limiter }
    }
}

pub fn router(state: AppState) -> Router {
    let auth_ctx = AuthContext::new(state.store.clone(), ["/register"]);

    let heartbeat_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 6,
                burst: 3,
                interval: Duration::from_secs(60),
            },
            endpoint: "heartbeat",
        },
        enforce_rate_limit,
    );
    let claim_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 12,
                burst: 6,
                interval: Duration::from_secs(60),
            },
            endpoint: "claim",
        },
        enforce_rate_limit,
    );
    let extend_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 30,
                burst: 10,
                interval: Duration::from_secs(60),
            },
            endpoint: "extend",
        },
        enforce_rate_limit,
    );
    let report_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 12,
                burst: 6,
                interval: Duration::from_secs(60),
            },
            endpoint: "report",
        },
        enforce_rate_limit,
    );
    let metrics_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 60,
                burst: 10,
                interval: Duration::from_secs(60),
            },
            endpoint: "metrics",
        },
        enforce_rate_limit,
    );
    let config_layer = middleware::from_fn_with_state(
        RateLimitContext {
            limiter: state.limiter.clone(),
            policy: RateLimitPolicy {
                capacity: 6,
                burst: 2,
                interval: Duration::from_secs(3600),
            },
            endpoint: "config",
        },
        enforce_rate_limit,
    );

    let protected = Router::new()
        .route("/heartbeat", post(heartbeat).route_layer(heartbeat_layer))
        .route("/claim", post(claim).route_layer(claim_layer))
        .route("/extend", post(extend).route_layer(extend_layer))
        .route("/report", post(report).route_layer(report_layer))
        .route("/metrics", post(metrics).route_layer(metrics_layer))
        .route("/config", get(config).route_layer(config_layer))
        .layer(middleware::from_fn_with_state(auth_ctx, require_auth));

    Router::new()
        .route("/register", post(register))
        .merge(protected)
        .with_state(state)
        .layer(http_trace_layer())
}

async fn register(
    State(state): State<AppState>,
    connect: Option<ConnectInfo<SocketAddr>>,
    Json(payload): Json<RegisterRequest>,
) -> ApiResult<Json<RegisterResponse>> {
    let peer = connect
        .as_ref()
        .map(|info| info.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let limiter_key = payload.hostname.clone().unwrap_or_else(|| peer.clone());
    let policy = RateLimitPolicy {
        capacity: 1,
        burst: 1,
        interval: Duration::from_secs(600),
    };
    state
        .limiter
        .check(&format!("register:{}", limiter_key), &policy)
        .await
        .map_err(|err| ApiError::rate_limited("register", err.retry_after))?;
    let response = state.store.register(payload).await?;
    Ok(Json(response))
}

async fn heartbeat(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    Json(payload): Json<HeartbeatRequest>,
) -> ApiResult<Json<HeartbeatResponse>> {
    if payload.agent_id != identity.agent_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ERR_FORBIDDEN",
            "agent identifier mismatch",
            false,
        ));
    }
    let response = state.store.touch_heartbeat(payload.agent_id).await?;
    Ok(Json(response))
}

async fn claim(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    Json(payload): Json<ClaimRequest>,
) -> ApiResult<Json<ClaimResponse>> {
    if payload.agent_id != identity.agent_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ERR_FORBIDDEN",
            "agent identifier mismatch",
            false,
        ));
    }
    let response = state.store.claim(payload).await?;
    Ok(Json(response))
}

#[derive(Debug, Serialize)]
struct ExtendResponseBody {
    outcomes: Vec<ExtendOutcome>,
    revoked: Vec<u64>,
}

async fn extend(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    Json(payload): Json<ExtendRequest>,
) -> ApiResult<Json<ExtendResponseBody>> {
    if payload.agent_id != identity.agent_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ERR_FORBIDDEN",
            "agent identifier mismatch",
            false,
        ));
    }
    let (outcomes, revoked) = state.store.extend(payload).await?;
    Ok(Json(ExtendResponseBody { outcomes, revoked }))
}

async fn report(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    Json(payload): Json<ReportRequest>,
) -> ApiResult<Json<ReportResponse>> {
    if payload.agent_id != identity.agent_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ERR_FORBIDDEN",
            "agent identifier mismatch",
            false,
        ));
    }
    let response = state.store.report(payload).await?;
    Ok(Json(response))
}

#[derive(Debug, Serialize)]
struct MetricsAccepted {
    accepted: bool,
    id: String,
}

async fn metrics(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    Json(mut payload): Json<MetricsEnvelope>,
) -> Result<Response, ApiError> {
    payload.agent_id = identity.agent_id;
    let record_id = state.store.ingest_metrics(payload).await?;
    let body = MetricsAccepted {
        accepted: true,
        id: record_id,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

async fn config(
    State(state): State<AppState>,
    Extension(identity): Extension<AgentIdentity>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _ = identity;
    let maybe_etag = headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('\"').to_string());
    let Some(config) = state.store.latest_config().await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "ERR_NOT_FOUND",
            "configuration bundle not found",
            true,
        ));
    };

    if let Some(tag) = maybe_etag {
        if tag == config.revision {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
    }

    let mut response = Json(&config).into_response();
    if let Ok(value) = HeaderValue::from_str(&config.revision) {
        response.headers_mut().insert(http::header::ETAG, value);
    }
    Ok(response)
}
