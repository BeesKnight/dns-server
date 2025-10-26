use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::header::RETRY_AFTER;
use http::HeaderValue;
use tokio::sync::Mutex;
use tracing::warn;

use crate::workers::storage::{AgentIdentity, StorageError};

#[derive(Debug, Clone, Copy)]
pub struct RateLimitPolicy {
    pub capacity: u32,
    pub burst: u32,
    pub interval: Duration,
}

#[derive(Clone, Default)]
pub struct SharedRateLimiter {
    inner: Arc<Mutex<HashMap<String, RateWindow>>>,
}

impl SharedRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn check(&self, key: &str, policy: &RateLimitPolicy) -> Result<(), RateLimitError> {
        let mut guard = self.inner.lock().await;
        let now = tokio::time::Instant::now();
        let window = guard
            .entry(key.to_string())
            .or_insert_with(|| RateWindow::new(policy.burst, now));
        window.refill(policy, now);
        if window.try_acquire(policy) {
            Ok(())
        } else {
            let retry_after = window.retry_after(policy);
            Err(RateLimitError { retry_after })
        }
    }
}

#[derive(Debug)]
struct RateWindow {
    tokens: f64,
    last_refill: tokio::time::Instant,
}

impl RateWindow {
    fn new(burst: u32, now: tokio::time::Instant) -> Self {
        Self {
            tokens: burst as f64,
            last_refill: now,
        }
    }

    fn refill(&mut self, policy: &RateLimitPolicy, now: tokio::time::Instant) {
        if policy.interval.is_zero() {
            self.tokens = policy.burst as f64;
            self.last_refill = now;
            return;
        }
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        let refill_rate = policy.capacity as f64 / policy.interval.as_secs_f64();
        let added = refill_rate * elapsed.as_secs_f64();
        self.tokens = (self.tokens + added).min(policy.burst as f64);
        self.last_refill = now;
    }

    fn try_acquire(&mut self, policy: &RateLimitPolicy) -> bool {
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else if policy.burst == 0 {
            false
        } else if self.tokens + 1.0e-9 >= 1.0 {
            self.tokens = 0.0;
            true
        } else {
            false
        }
    }

    fn retry_after(&self, policy: &RateLimitPolicy) -> Duration {
        if policy.capacity == 0 || policy.interval.is_zero() {
            return policy.interval;
        }
        let needed = (1.0f64 - self.tokens).max(0.0);
        let refill_rate = policy.capacity as f64 / policy.interval.as_secs_f64();
        let seconds = (needed / refill_rate).max(0.0);
        let nanos = (seconds * 1e9).ceil() as u64;
        let retry = Duration::from_nanos(nanos);
        if retry.is_zero() {
            Duration::from_millis(1)
        } else {
            retry
        }
    }
}

#[derive(Debug)]
pub struct RateLimitError {
    pub retry_after: Duration,
}

#[derive(Clone)]
pub struct RateLimitContext {
    pub limiter: Arc<SharedRateLimiter>,
    pub policy: RateLimitPolicy,
    pub endpoint: &'static str,
}

#[derive(Debug)]
pub struct RateLimitRejection {
    pub endpoint: &'static str,
    pub retry_after: Duration,
}

impl IntoResponse for RateLimitRejection {
    fn into_response(self) -> Response {
        self.into_response_with_body()
    }
}

pub async fn enforce_rate_limit(
    State(ctx): State<RateLimitContext>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, RateLimitRejection> {
    let agent_id = req
        .extensions()
        .get::<AgentIdentity>()
        .map(|identity| identity.agent_id)
        .unwrap_or(0);
    let key = if agent_id == 0 {
        format!("{}:anonymous", ctx.endpoint)
    } else {
        format!("{}:{}", ctx.endpoint, agent_id)
    };
    match ctx.limiter.check(&key, &ctx.policy).await {
        Ok(()) => Ok(next.run(req).await),
        Err(err) => Err(RateLimitRejection {
            endpoint: ctx.endpoint,
            retry_after: err.retry_after,
        }),
    }
}

impl RateLimitRejection {
    pub fn into_response_with_body(self) -> Response {
        let payload = crate::http::error::ErrorEnvelope {
            error: "ERR_RATE_LIMITED".to_string(),
            message: format!("rate limit exceeded for {}", self.endpoint),
            retryable: true,
        };
        let mut response = (StatusCode::TOO_MANY_REQUESTS, axum::Json(payload)).into_response();
        if let Ok(value) = HeaderValue::from_str(&self.retry_after.as_secs().to_string()) {
            response.headers_mut().insert(RETRY_AFTER, value);
        }
        response
    }
}

#[async_trait]
pub trait AuthBackend: Send + Sync + 'static {
    async fn authenticate(&self, agent_id: u64, token: &str)
        -> Result<AgentIdentity, StorageError>;
}

#[derive(Clone)]
pub struct AuthContext<B: AuthBackend> {
    backend: Arc<B>,
    exempt: Arc<HashSet<&'static str>>,
}

impl<B: AuthBackend> AuthContext<B> {
    pub fn new(backend: Arc<B>, exempt: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            backend,
            exempt: Arc::new(exempt.into_iter().collect()),
        }
    }
}

#[derive(Debug)]
pub struct AuthRejection {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let payload = crate::http::error::ErrorEnvelope {
            error: self.code.to_string(),
            message: self.message,
            retryable: self.retryable,
        };
        (self.status, axum::Json(payload)).into_response()
    }
}

pub async fn require_auth<B>(
    State(ctx): State<AuthContext<B>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, AuthRejection>
where
    B: AuthBackend,
{
    if ctx.exempt.contains(req.uri().path()) {
        return Ok(next.run(req).await);
    }

    let headers = req.headers();
    let agent_id_header = headers.get("X-Agent-Id").ok_or_else(|| AuthRejection {
        status: StatusCode::UNAUTHORIZED,
        code: "ERR_UNAUTHORIZED",
        message: "missing X-Agent-Id header".to_string(),
        retryable: false,
    })?;
    let token_header = headers.get("X-Agent-Token").ok_or_else(|| AuthRejection {
        status: StatusCode::UNAUTHORIZED,
        code: "ERR_UNAUTHORIZED",
        message: "missing X-Agent-Token header".to_string(),
        retryable: false,
    })?;

    let agent_id = agent_id_header
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AuthRejection {
            status: StatusCode::BAD_REQUEST,
            code: "ERR_VALIDATION",
            message: "X-Agent-Id must be a valid integer".to_string(),
            retryable: false,
        })?;
    let token = token_header.to_str().map_err(|_| AuthRejection {
        status: StatusCode::BAD_REQUEST,
        code: "ERR_VALIDATION",
        message: "X-Agent-Token must be valid UTF-8".to_string(),
        retryable: false,
    })?;

    match ctx.backend.authenticate(agent_id, token).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            Ok(next.run(req).await)
        }
        Err(err) => Err(map_auth_error(err)),
    }
}

fn map_auth_error(err: StorageError) -> AuthRejection {
    match err {
        StorageError::AgentNotFound => AuthRejection {
            status: StatusCode::UNAUTHORIZED,
            code: "ERR_UNAUTHORIZED",
            message: "agent credentials are unknown".to_string(),
            retryable: false,
        },
        StorageError::AgentRevoked => AuthRejection {
            status: StatusCode::FORBIDDEN,
            code: "ERR_FORBIDDEN",
            message: "agent has been revoked".to_string(),
            retryable: false,
        },
        StorageError::AgentDisabled => AuthRejection {
            status: StatusCode::FORBIDDEN,
            code: "ERR_FORBIDDEN",
            message: "agent is disabled".to_string(),
            retryable: false,
        },
        StorageError::Forbidden(_) => AuthRejection {
            status: StatusCode::UNAUTHORIZED,
            code: "ERR_UNAUTHORIZED",
            message: "invalid credentials".to_string(),
            retryable: false,
        },
        other => {
            warn!(error = %other, "authentication backend error");
            AuthRejection {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "ERR_BACKEND",
                message: "authentication backend failure".to_string(),
                retryable: true,
            }
        }
    }
}

#[async_trait]
impl AuthBackend for crate::workers::storage::AgentStore {
    async fn authenticate(
        &self,
        agent_id: u64,
        token: &str,
    ) -> Result<AgentIdentity, StorageError> {
        self.authenticate_credentials(agent_id, token).await
    }
}
