use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};

/// A domain specific representation of the different types of tasks the agent can
/// lease from the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Dns,
    Http,
    Tcp,
    Ping,
    Trace,
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskKind::Dns => write!(f, "dns"),
            TaskKind::Http => write!(f, "http"),
            TaskKind::Tcp => write!(f, "tcp"),
            TaskKind::Ping => write!(f, "ping"),
            TaskKind::Trace => write!(f, "trace"),
        }
    }
}

/// Registration response returned by the control plane.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RegisterResponse {
    pub agent_id: u64,
    pub lease_duration_ms: u64,
    pub heartbeat_timeout_ms: u64,
    pub auth_token: String,
}

/// Heartbeat acknowledgement used to refresh the heartbeat deadline.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HeartbeatResponse {
    pub agent_id: u64,
    pub next_deadline_ms: u64,
}

/// Representation of a leased task.
/// Specification describing how a task should be executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskSpec {
    Dns {
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server: Option<String>,
    },
    Http {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    Tcp(TcpSpec),
    Ping(PingSpec),
    Trace(TraceSpec),
}

/// Specification for TCP checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpSpec {
    pub host: String,
    pub port: u16,
}

/// Specification for ping checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingSpec {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_sec: Option<u32>,
}

/// Specification for traceroute checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSpec {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_hops: Option<u8>,
}

/// Observation produced while processing a lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub name: String,
    pub value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Structured report for a lease outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseReport {
    pub lease_id: u64,
    #[serde(default)]
    pub observations: Vec<Observation>,
}

/// Representation of a leased task.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Lease {
    pub lease_id: u64,
    pub task_id: u64,
    pub kind: TaskKind,
    pub lease_until_ms: u64,
    pub spec: TaskSpec,
}

/// Batched set of leases returned by the control plane when claiming work.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ClaimResponse {
    pub leases: Vec<Lease>,
}

/// Response payload describing the outcome of a lease extension request.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ExtendOutcome {
    pub lease_id: u64,
    pub new_deadline_ms: u64,
}

/// Response payload acknowledging lease reports.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ReportResponse {
    pub acknowledged: usize,
}

/// Request payload for claiming new leases.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaimRequest {
    pub agent_id: u64,
    pub capacities: HashMap<TaskKind, usize>,
}

/// Request payload for extending existing leases.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExtendRequest {
    pub agent_id: u64,
    pub lease_ids: Vec<u64>,
    pub extend_by_ms: u64,
}

/// Request payload for reporting completed leases back to the control plane.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReportRequest {
    pub agent_id: u64,
    pub completed: Vec<LeaseReport>,
    pub cancelled: Vec<LeaseReport>,
}

/// Heartbeat payload sent to the control plane.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HeartbeatRequest {
    pub agent_id: u64,
}

/// Request payload used when registering the agent with the control plane.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct RegisterRequest {
    pub hostname: Option<String>,
}

/// Errors produced by the control plane client.
#[derive(Error, Debug)]
pub enum ControlPlaneError {
    #[error("agent is not registered")]
    NotRegistered,
    #[error("control plane rejected registration response")]
    InvalidRegistration,
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
}

/// Transport level errors. When surfaced they result in retries governed by the
/// FSM backoff state machine.
#[derive(Error, Debug)]
pub enum TransportError {
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected status code: {status}")]
    Http { status: u16 },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("backend error: {0}")]
    Backend(String),
}

impl TransportError {
    fn network(err: impl std::error::Error) -> Self {
        Self::Network(err.to_string())
    }
}

#[async_trait]
pub trait ControlPlaneTransport: Send + Sync {
    async fn register(&self, request: &RegisterRequest)
        -> Result<RegisterResponse, TransportError>;
    async fn heartbeat(
        &self,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, TransportError>;
    async fn claim(&self, request: &ClaimRequest) -> Result<ClaimResponse, TransportError>;
    async fn extend(&self, request: &ExtendRequest) -> Result<Vec<ExtendOutcome>, TransportError>;
    async fn report(&self, request: &ReportRequest) -> Result<ReportResponse, TransportError>;
    fn set_auth_headers(&self, _agent_id: String, _token: String) {}
}

/// HTTP transport that speaks JSON over REST to the control plane.
#[derive(Clone)]
pub struct HttpControlPlaneTransport {
    client: reqwest::Client,
    base_url: reqwest::Url,
    auth_headers: Arc<RwLock<Option<(String, String)>>>,
}

impl HttpControlPlaneTransport {
    pub fn new(base_url: &str) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .user_agent("codecrafters-agent/0.1.0")
            .build()
            .map_err(TransportError::network)?;
        let base_url = reqwest::Url::parse(base_url)
            .map_err(|err| TransportError::InvalidEndpoint(err.to_string()))?;
        Ok(Self {
            client,
            base_url,
            auth_headers: Arc::new(RwLock::new(None)),
        })
    }

    fn endpoint(&self, path: &str) -> Result<reqwest::Url, TransportError> {
        self.base_url
            .join(path)
            .map_err(|err| TransportError::InvalidEndpoint(err.to_string()))
    }

    fn apply_auth_headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Ok(guard) = self.auth_headers.read() {
            if let Some((agent_id, token)) = guard.as_ref() {
                return builder
                    .header("X-Agent-Id", agent_id.clone())
                    .header("X-Agent-Token", token.clone());
            }
        }
        builder
    }
}

#[async_trait]
impl ControlPlaneTransport for HttpControlPlaneTransport {
    async fn register(
        &self,
        request: &RegisterRequest,
    ) -> Result<RegisterResponse, TransportError> {
        let url = self.endpoint("register")?;
        let response = self
            .apply_auth_headers(self.client.post(url))
            .json(request)
            .send()
            .await
            .map_err(TransportError::network)?;
        if !response.status().is_success() {
            return Err(TransportError::Http {
                status: response.status().as_u16(),
            });
        }
        response.json().await.map_err(TransportError::network)
    }

    async fn heartbeat(
        &self,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, TransportError> {
        let url = self.endpoint("heartbeat")?;
        let response = self
            .apply_auth_headers(self.client.post(url))
            .json(request)
            .send()
            .await
            .map_err(TransportError::network)?;
        if !response.status().is_success() {
            return Err(TransportError::Http {
                status: response.status().as_u16(),
            });
        }
        response.json().await.map_err(TransportError::network)
    }

    async fn claim(&self, request: &ClaimRequest) -> Result<ClaimResponse, TransportError> {
        let url = self.endpoint("claim")?;
        let response = self
            .apply_auth_headers(self.client.post(url))
            .json(request)
            .send()
            .await
            .map_err(TransportError::network)?;
        if !response.status().is_success() {
            return Err(TransportError::Http {
                status: response.status().as_u16(),
            });
        }
        response.json().await.map_err(TransportError::network)
    }

    async fn extend(&self, request: &ExtendRequest) -> Result<Vec<ExtendOutcome>, TransportError> {
        let url = self.endpoint("extend")?;
        let response = self
            .apply_auth_headers(self.client.post(url))
            .json(request)
            .send()
            .await
            .map_err(TransportError::network)?;
        if !response.status().is_success() {
            return Err(TransportError::Http {
                status: response.status().as_u16(),
            });
        }
        response.json().await.map_err(TransportError::network)
    }

    async fn report(&self, request: &ReportRequest) -> Result<ReportResponse, TransportError> {
        let url = self.endpoint("report")?;
        let response = self
            .apply_auth_headers(self.client.post(url))
            .json(request)
            .send()
            .await
            .map_err(TransportError::network)?;
        if !response.status().is_success() {
            return Err(TransportError::Http {
                status: response.status().as_u16(),
            });
        }
        response.json().await.map_err(TransportError::network)
    }

    fn set_auth_headers(&self, agent_id: String, token: String) {
        if let Ok(mut guard) = self.auth_headers.write() {
            *guard = Some((agent_id, token));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentOperation {
    Register,
    Heartbeat,
    Claim,
    Extend,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Unregistered,
    Active,
}

/// Backoff policy used by the FSM when retrying control plane operations.
#[derive(Debug, Clone)]
pub struct BackoffPolicy {
    base: Duration,
    max: Duration,
    multiplier: f64,
}

impl BackoffPolicy {
    pub fn new(base: Duration, max: Duration, multiplier: f64) -> Self {
        Self {
            base,
            max,
            multiplier,
        }
    }

    fn delay_for(&self, attempts: u32) -> Duration {
        let pow = self.multiplier.powi(attempts as i32);
        let delay = self.base.mul_f64(pow);
        if delay > self.max {
            self.max
        } else {
            delay
        }
    }
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self::new(Duration::from_millis(100), Duration::from_secs(5), 2.0)
    }
}

#[derive(Debug, Clone)]
pub struct AgentBootstrap {
    pub agent_id: u64,
    pub auth_token: String,
}

#[derive(Debug)]
struct AgentRuntime {
    lifecycle: Lifecycle,
    agent_id: Option<u64>,
    heartbeat_deadline: Option<Instant>,
    lease_deadlines: HashMap<u64, Instant>,
    lease_duration: Duration,
    heartbeat_timeout: Duration,
    attempts: HashMap<AgentOperation, u32>,
    auth_token: Option<String>,
}

impl AgentRuntime {
    fn new() -> Self {
        Self {
            lifecycle: Lifecycle::Unregistered,
            agent_id: None,
            heartbeat_deadline: None,
            lease_deadlines: HashMap::new(),
            lease_duration: Duration::from_millis(0),
            heartbeat_timeout: Duration::from_millis(0),
            attempts: HashMap::new(),
            auth_token: None,
        }
    }

    fn with_bootstrap(bootstrap: Option<AgentBootstrap>) -> Self {
        let mut runtime = Self::new();
        if let Some(bootstrap) = bootstrap {
            runtime.lifecycle = Lifecycle::Active;
            runtime.agent_id = Some(bootstrap.agent_id);
            runtime.auth_token = Some(bootstrap.auth_token);
        }
        runtime
    }

    fn agent_id(&self) -> Result<u64, ControlPlaneError> {
        self.agent_id.ok_or(ControlPlaneError::NotRegistered)
    }

    fn record_success(&mut self, op: AgentOperation) {
        self.attempts.insert(op, 0);
    }

    fn record_failure(&mut self, op: AgentOperation, policy: &BackoffPolicy) -> Duration {
        let attempts = self.attempts.entry(op).or_insert(0);
        let delay = policy.delay_for(*attempts);
        *attempts = attempts.saturating_add(1);
        delay
    }

    fn apply_registration(&mut self, response: &RegisterResponse) -> Result<(), ControlPlaneError> {
        if response.agent_id == 0 {
            return Err(ControlPlaneError::InvalidRegistration);
        }
        self.agent_id = Some(response.agent_id);
        self.lease_duration = Duration::from_millis(response.lease_duration_ms);
        self.heartbeat_timeout = Duration::from_millis(response.heartbeat_timeout_ms);
        self.heartbeat_deadline = Some(Instant::now() + self.heartbeat_timeout);
        self.lifecycle = Lifecycle::Active;
        self.auth_token = Some(response.auth_token.clone());
        Ok(())
    }

    fn apply_heartbeat(&mut self, response: &HeartbeatResponse) -> Result<(), ControlPlaneError> {
        if Some(response.agent_id) != self.agent_id {
            return Err(ControlPlaneError::NotRegistered);
        }
        self.heartbeat_deadline =
            Some(Instant::now() + Duration::from_millis(response.next_deadline_ms));
        Ok(())
    }

    fn apply_claim(&mut self, claim: &ClaimResponse) -> Result<(), ControlPlaneError> {
        for lease in &claim.leases {
            self.lease_deadlines.insert(
                lease.lease_id,
                Instant::now() + Duration::from_millis(lease.lease_until_ms),
            );
        }
        Ok(())
    }

    fn apply_extend(&mut self, outcomes: &[ExtendOutcome]) -> Result<(), ControlPlaneError> {
        for outcome in outcomes {
            if let Some(deadline) = self.lease_deadlines.get_mut(&outcome.lease_id) {
                *deadline = Instant::now() + Duration::from_millis(outcome.new_deadline_ms);
            }
        }
        Ok(())
    }

    fn apply_report(
        &mut self,
        completed: &[LeaseReport],
        cancelled: &[LeaseReport],
    ) -> Result<(), ControlPlaneError> {
        for lease in completed.iter().chain(cancelled.iter()) {
            self.lease_deadlines.remove(&lease.lease_id);
        }
        Ok(())
    }
}

/// Shared state for the control plane client.
struct ControlPlaneInner<T: ControlPlaneTransport> {
    transport: T,
    policy: BackoffPolicy,
    runtime: Mutex<AgentRuntime>,
}

impl<T: ControlPlaneTransport> ControlPlaneInner<T> {
    fn new(transport: T, policy: BackoffPolicy, bootstrap: Option<AgentBootstrap>) -> Self {
        Self {
            transport,
            policy,
            runtime: Mutex::new(AgentRuntime::with_bootstrap(bootstrap)),
        }
    }
}

pub struct ControlPlaneClient<T: ControlPlaneTransport> {
    inner: Arc<ControlPlaneInner<T>>,
}

impl<T: ControlPlaneTransport> Clone for ControlPlaneClient<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: ControlPlaneTransport> ControlPlaneClient<T> {
    pub fn new(transport: T) -> Self {
        Self::with_policy_and_bootstrap(transport, BackoffPolicy::default(), None)
    }

    pub fn with_policy(transport: T, policy: BackoffPolicy) -> Self {
        Self::with_policy_and_bootstrap(transport, policy, None)
    }

    pub fn with_bootstrap(transport: T, bootstrap: Option<AgentBootstrap>) -> Self {
        Self::with_policy_and_bootstrap(transport, BackoffPolicy::default(), bootstrap)
    }

    pub fn with_policy_and_bootstrap(
        transport: T,
        policy: BackoffPolicy,
        bootstrap: Option<AgentBootstrap>,
    ) -> Self {
        let inner = ControlPlaneInner::new(transport, policy, bootstrap);
        Self {
            inner: Arc::new(inner),
        }
    }

    pub async fn register(
        &self,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, ControlPlaneError> {
        loop {
            let response = self.inner.transport.register(&request).await;
            match response {
                Ok(payload) => {
                    {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.apply_registration(&payload)?;
                        runtime.record_success(AgentOperation::Register);
                    }
                    self.inner
                        .transport
                        .set_auth_headers(payload.agent_id.to_string(), payload.auth_token.clone());
                    return Ok(payload);
                }
                Err(err) => {
                    let delay = {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.record_failure(AgentOperation::Register, &self.inner.policy)
                    };
                    sleep(delay).await;
                    tracing::warn!(?delay, error = %err, "retrying registration");
                }
            }
        }
    }

    pub async fn heartbeat(&self) -> Result<HeartbeatResponse, ControlPlaneError> {
        let agent_id = {
            let runtime = self.inner.runtime.lock().await;
            runtime.agent_id()?
        };
        let request = HeartbeatRequest { agent_id };
        loop {
            let response = self.inner.transport.heartbeat(&request).await;
            match response {
                Ok(payload) => {
                    let mut runtime = self.inner.runtime.lock().await;
                    runtime.apply_heartbeat(&payload)?;
                    runtime.record_success(AgentOperation::Heartbeat);
                    return Ok(payload);
                }
                Err(err) => {
                    let delay = {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.record_failure(AgentOperation::Heartbeat, &self.inner.policy)
                    };
                    sleep(delay).await;
                    tracing::warn!(?delay, error = %err, "retrying heartbeat");
                }
            }
        }
    }

    pub async fn claim(
        &self,
        capacities: &HashMap<TaskKind, usize>,
    ) -> Result<ClaimResponse, ControlPlaneError> {
        let agent_id = {
            let runtime = self.inner.runtime.lock().await;
            runtime.agent_id()?
        };
        loop {
            let request = ClaimRequest {
                agent_id,
                capacities: capacities.clone(),
            };
            let response = self.inner.transport.claim(&request).await;
            match response {
                Ok(payload) => {
                    let mut runtime = self.inner.runtime.lock().await;
                    runtime.apply_claim(&payload)?;
                    runtime.record_success(AgentOperation::Claim);
                    return Ok(payload);
                }
                Err(err) => {
                    let delay = {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.record_failure(AgentOperation::Claim, &self.inner.policy)
                    };
                    sleep(delay).await;
                    tracing::warn!(?delay, error = %err, "retrying claim");
                }
            }
        }
    }

    pub async fn extend(
        &self,
        lease_ids: Vec<u64>,
        extend_by: Duration,
    ) -> Result<Vec<ExtendOutcome>, ControlPlaneError> {
        let agent_id = {
            let runtime = self.inner.runtime.lock().await;
            runtime.agent_id()?
        };
        let request = ExtendRequest {
            agent_id,
            lease_ids,
            extend_by_ms: extend_by.as_millis() as u64,
        };
        loop {
            let response = self.inner.transport.extend(&request).await;
            match response {
                Ok(payload) => {
                    let mut runtime = self.inner.runtime.lock().await;
                    runtime.apply_extend(&payload)?;
                    runtime.record_success(AgentOperation::Extend);
                    return Ok(payload);
                }
                Err(err) => {
                    let delay = {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.record_failure(AgentOperation::Extend, &self.inner.policy)
                    };
                    sleep(delay).await;
                    tracing::warn!(?delay, error = %err, "retrying extend");
                }
            }
        }
    }

    pub async fn report(
        &self,
        completed: Vec<LeaseReport>,
        cancelled: Vec<LeaseReport>,
    ) -> Result<ReportResponse, ControlPlaneError> {
        let agent_id = {
            let runtime = self.inner.runtime.lock().await;
            runtime.agent_id()?
        };
        let request = ReportRequest {
            agent_id,
            completed,
            cancelled,
        };
        loop {
            let response = self.inner.transport.report(&request).await;
            match response {
                Ok(payload) => {
                    let mut runtime = self.inner.runtime.lock().await;
                    let completed = request.completed.clone();
                    let cancelled = request.cancelled.clone();
                    runtime.apply_report(&completed, &cancelled)?;
                    runtime.record_success(AgentOperation::Report);
                    return Ok(payload);
                }
                Err(err) => {
                    let delay = {
                        let mut runtime = self.inner.runtime.lock().await;
                        runtime.record_failure(AgentOperation::Report, &self.inner.policy)
                    };
                    sleep(delay).await;
                    tracing::warn!(?delay, error = %err, "retrying report");
                }
            }
        }
    }
}

/// Information about leases tracked by the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSnapshot {
    pub agent_id: Option<u64>,
    pub heartbeat_deadline: Option<Instant>,
    pub leases: HashMap<u64, Instant>,
    pub auth_token: Option<String>,
}

impl<T: ControlPlaneTransport> ControlPlaneClient<T> {
    pub async fn snapshot(&self) -> AgentSnapshot {
        let runtime = self.inner.runtime.lock().await;
        AgentSnapshot {
            agent_id: runtime.agent_id,
            heartbeat_deadline: runtime.heartbeat_deadline,
            leases: runtime.lease_deadlines.clone(),
            auth_token: runtime.auth_token.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_request_serializes_capacities_as_snake_case() {
        let mut capacities = HashMap::new();
        capacities.insert(TaskKind::Dns, 4);
        capacities.insert(TaskKind::Http, 2);
        let request = ClaimRequest {
            agent_id: 7,
            capacities,
        };
        let json = serde_json::to_value(&request).expect("serialize claim");
        assert_eq!(json["agent_id"], 7);
        let caps = json["capacities"]
            .as_object()
            .expect("serialized capacities should be an object");
        assert_eq!(caps["dns"], 4);
        assert_eq!(caps["http"], 2);
    }

    #[test]
    fn extend_request_serializes_duration_in_millis() {
        let request = ExtendRequest {
            agent_id: 42,
            lease_ids: vec![1, 2, 3],
            extend_by_ms: 1500,
        };
        let json = serde_json::to_value(&request).expect("serialize extend");
        assert_eq!(json["agent_id"], 42);
        assert_eq!(json["extend_by_ms"], 1500);
        assert_eq!(json["lease_ids"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn report_request_serializes_completed() {
        let request = ReportRequest {
            agent_id: 99,
            completed: vec![
                LeaseReport {
                    lease_id: 10,
                    observations: vec![Observation {
                        name: "latency_ms".into(),
                        value: serde_json::json!(12.5),
                        unit: Some("ms".into()),
                    }],
                },
                LeaseReport {
                    lease_id: 20,
                    observations: Vec::new(),
                },
            ],
            cancelled: vec![LeaseReport {
                lease_id: 30,
                observations: Vec::new(),
            }],
        };
        let json = serde_json::to_value(&request).expect("serialize report");
        assert_eq!(json["agent_id"], 99);
        assert_eq!(json["completed"][0]["lease_id"], 10);
        assert_eq!(
            json["completed"][0]["observations"][0]["name"],
            "latency_ms"
        );
        assert_eq!(json["completed"][1]["lease_id"], 20);
        assert_eq!(json["cancelled"][0]["lease_id"], 30);
    }

    #[test]
    fn backoff_policy_scales_exponentially() {
        let policy = BackoffPolicy::new(Duration::from_millis(100), Duration::from_secs(2), 2.0);
        assert_eq!(policy.delay_for(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for(2), Duration::from_millis(400));
        assert_eq!(policy.delay_for(5), Duration::from_secs(2));
    }
}
