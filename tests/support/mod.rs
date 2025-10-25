#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::anyhow;
use async_trait::async_trait;
use futures::future::{self, Either};
use hdrhistogram::Histogram;
use serde_json::json;
use thiserror::Error;
use tokio::{
    select,
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex, Notify},
    task::JoinHandle,
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;

use dns_agent::concurrency::{ConcurrencyController, ConcurrencyLimits, ConcurrencyPermit};
use dns_agent::control_plane::{
    ExtendOutcome as AgentExtendOutcome, LeaseReport, Observation, TaskKind, TaskSpec,
};
use dns_agent::lease_extender::{
    spawn_lease_extender, LeaseExtendClient, LeaseExtendUpdate, LeaseExtenderClient,
    LeaseExtenderConfig, LeaseExtenderHandle,
};
use dns_agent::runtime::TimerService;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum TaskType {
    Dns,
    Http,
    Tcp,
    Ping,
    Trace,
}

impl TaskType {
    pub const ALL: [TaskType; 5] = [
        TaskType::Dns,
        TaskType::Http,
        TaskType::Tcp,
        TaskType::Ping,
        TaskType::Trace,
    ];
}

impl From<TaskType> for TaskKind {
    fn from(value: TaskType) -> Self {
        match value {
            TaskType::Dns => TaskKind::Dns,
            TaskType::Http => TaskKind::Http,
            TaskType::Tcp => TaskKind::Tcp,
            TaskType::Ping => TaskKind::Ping,
            TaskType::Trace => TaskKind::Trace,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: u64,
    pub kind: TaskType,
    pub enqueued_at: Instant,
    pub spec: TaskSpec,
}

impl Task {
    pub fn new(id: u64, kind: TaskType) -> Self {
        Self {
            id,
            kind,
            enqueued_at: Instant::now(),
            spec: default_spec(id, kind),
        }
    }
}

fn default_spec(id: u64, kind: TaskType) -> TaskSpec {
    match kind {
        TaskType::Dns => TaskSpec::Dns {
            query: format!("example-{id}.com"),
            server: None,
        },
        TaskType::Http => TaskSpec::Http {
            url: format!("https://example.com/{id}"),
            method: Some("GET".into()),
        },
        TaskType::Tcp => TaskSpec::Tcp(dns_agent::control_plane::TcpSpec {
            host: "127.0.0.1".into(),
            port: 80,
        }),
        TaskType::Ping => TaskSpec::Ping(dns_agent::control_plane::PingSpec {
            host: "127.0.0.1".into(),
            count: Some(4),
            interval_ms: None,
            timeout_ms: None,
            rate_limit_per_sec: None,
        }),
        TaskType::Trace => TaskSpec::Trace(dns_agent::control_plane::TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(6),
        }),
    }
}

#[derive(Clone, Debug)]
pub struct LeasedTask {
    pub lease_id: u64,
    pub task: Task,
    pub lease_until: Instant,
    pub leased_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BackendOp {
    Register,
    Heartbeat,
    Claim,
    Extend,
    Report,
}

#[derive(Debug, Clone)]
pub struct BackendBehavior {
    pub lease_duration: Duration,
    pub heartbeat_timeout: Duration,
    pub operation_delays: HashMap<BackendOp, Duration>,
    pub revoked_tasks: HashSet<u64>,
}

impl Default for BackendBehavior {
    fn default() -> Self {
        Self {
            lease_duration: Duration::from_millis(150),
            heartbeat_timeout: Duration::from_millis(400),
            operation_delays: HashMap::new(),
            revoked_tasks: HashSet::new(),
        }
    }
}

impl BackendBehavior {
    pub fn set_delay(&mut self, op: BackendOp, delay: Option<Duration>) {
        match delay {
            Some(value) => {
                self.operation_delays.insert(op, value);
            }
            None => {
                self.operation_delays.remove(&op);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub u64);

#[derive(Debug, Clone)]
pub struct AgentRegistration {
    pub agent_id: AgentId,
    pub lease_duration: Duration,
    pub heartbeat_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct HeartbeatAck {
    pub agent_id: AgentId,
    pub next_deadline: Instant,
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub agent_id: AgentId,
    pub capacities: BTreeMap<TaskType, usize>,
}

#[derive(Debug, Clone)]
pub struct ClaimBatch {
    pub leases: Vec<LeasedTask>,
}

#[derive(Debug, Clone)]
pub struct ExtendRequest {
    pub agent_id: AgentId,
    pub lease_ids: Vec<u64>,
    pub extend_by: Duration,
}

#[derive(Debug, Clone)]
pub struct ExtendOutcome {
    pub lease_id: u64,
    pub new_deadline: Instant,
}

#[derive(Debug, Clone)]
pub struct ReportRequest {
    pub agent_id: AgentId,
    pub completed: Vec<LeaseReport>,
    pub cancelled: Vec<LeaseReport>,
}

#[derive(Debug)]
pub struct BackendSnapshot {
    pub pending: HashMap<TaskType, Vec<u64>>,
    pub active_leases: HashMap<u64, (TaskType, Instant)>,
    pub revoked: HashSet<u64>,
}

#[derive(Error, Debug)]
pub enum BackendError {
    #[error("agent {0:?} is not registered")]
    UnknownAgent(AgentId),
    #[error("heartbeat timeout for agent {agent_id:?}")]
    HeartbeatTimeout { agent_id: AgentId },
    #[error("lease {lease_id} expired")]
    LeaseExpired { lease_id: u64 },
    #[error("task {task_id} revoked")]
    TaskRevoked { task_id: u64 },
}

struct LeaseState {
    lease: LeasedTask,
    agent: AgentId,
}

enum BackendRequest {
    Register {
        respond_to: oneshot::Sender<Result<AgentRegistration, BackendError>>,
    },
    Heartbeat {
        agent: AgentId,
        respond_to: oneshot::Sender<Result<HeartbeatAck, BackendError>>,
    },
    Claim {
        request: ClaimRequest,
        respond_to: oneshot::Sender<Result<ClaimBatch, BackendError>>,
    },
    Extend {
        request: ExtendRequest,
        respond_to: oneshot::Sender<Result<Vec<ExtendOutcome>, BackendError>>,
    },
    Report {
        request: ReportRequest,
        respond_to: oneshot::Sender<Result<(), BackendError>>,
    },
}

enum ControlCommand {
    Enqueue(Vec<Task>),
    Shutdown,
    Snapshot(oneshot::Sender<BackendSnapshot>),
}

#[derive(Clone)]
pub struct MockBackend {
    request_tx: mpsc::Sender<BackendRequest>,
    behavior_rx: watch::Receiver<BackendBehavior>,
}

impl MockBackend {
    pub fn new() -> (Self, MockBackendControl, JoinHandle<()>) {
        let (request_tx, request_rx) = mpsc::channel(1024);
        let (control_tx, control_rx) = mpsc::channel(128);
        let (behavior_tx, behavior_rx) = watch::channel(BackendBehavior::default());
        let driver = MockBackendDriver::new(request_rx, control_rx, behavior_rx.clone());
        let join = tokio::spawn(driver.run());
        (
            Self {
                request_tx,
                behavior_rx,
            },
            MockBackendControl {
                control_tx,
                behavior_tx,
            },
            join,
        )
    }

    pub fn behavior(&self) -> BackendBehavior {
        self.behavior_rx.borrow().clone()
    }

    pub async fn register(&self) -> Result<AgentRegistration, BackendError> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(BackendRequest::Register { respond_to: tx })
            .await
            .expect("backend stopped");
        rx.await.expect("backend stopped")
    }

    pub async fn heartbeat(&self, agent_id: AgentId) -> Result<HeartbeatAck, BackendError> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(BackendRequest::Heartbeat {
                agent: agent_id,
                respond_to: tx,
            })
            .await
            .expect("backend stopped");
        rx.await.expect("backend stopped")
    }

    pub async fn claim(&self, request: ClaimRequest) -> Result<ClaimBatch, BackendError> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(BackendRequest::Claim {
                request,
                respond_to: tx,
            })
            .await
            .expect("backend stopped");
        rx.await.expect("backend stopped")
    }

    pub async fn extend(&self, request: ExtendRequest) -> Result<Vec<ExtendOutcome>, BackendError> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(BackendRequest::Extend {
                request,
                respond_to: tx,
            })
            .await
            .expect("backend stopped");
        rx.await.expect("backend stopped")
    }

    pub async fn report(&self, request: ReportRequest) -> Result<(), BackendError> {
        let (tx, rx) = oneshot::channel();
        self.request_tx
            .send(BackendRequest::Report {
                request,
                respond_to: tx,
            })
            .await
            .expect("backend stopped");
        rx.await.expect("backend stopped")
    }
}

pub struct MockBackendControl {
    control_tx: mpsc::Sender<ControlCommand>,
    behavior_tx: watch::Sender<BackendBehavior>,
}

impl MockBackendControl {
    pub async fn enqueue(&self, task: Task) {
        self.enqueue_many(vec![task]).await;
    }

    pub async fn enqueue_many(&self, tasks: Vec<Task>) {
        let _ = self.control_tx.send(ControlCommand::Enqueue(tasks)).await;
    }

    pub fn update_behavior<F: FnOnce(&mut BackendBehavior)>(&self, update: F) {
        let mut behavior = self.behavior_tx.borrow().clone();
        update(&mut behavior);
        let _ = self.behavior_tx.send(behavior);
    }

    pub async fn snapshot(&self) -> BackendSnapshot {
        let (tx, rx) = oneshot::channel();
        let _ = self.control_tx.send(ControlCommand::Snapshot(tx)).await;
        rx.await.expect("backend stopped")
    }

    pub async fn shutdown(&self) {
        let _ = self.control_tx.send(ControlCommand::Shutdown).await;
    }
}

#[derive(Clone)]
struct BackendExtender {
    backend: MockBackend,
    stats: Arc<PipelineStats>,
    agent: Arc<AsyncMutex<Option<AgentId>>>,
}

impl BackendExtender {
    fn new(backend: MockBackend, stats: Arc<PipelineStats>) -> Self {
        Self {
            backend,
            stats,
            agent: Arc::new(AsyncMutex::new(None)),
        }
    }

    async fn set_agent(&self, agent_id: AgentId) {
        let mut guard = self.agent.lock().await;
        *guard = Some(agent_id);
    }
}

#[async_trait]
impl LeaseExtendClient for BackendExtender {
    async fn extend_leases(
        &self,
        lease_ids: Vec<u64>,
        extend_by: Duration,
    ) -> anyhow::Result<LeaseExtendUpdate> {
        let agent_id = {
            let guard = self.agent.lock().await;
            guard.unwrap_or(AgentId(0))
        };
        let request = ExtendRequest {
            agent_id,
            lease_ids: lease_ids.clone(),
            extend_by,
        };
        match self.backend.extend(request).await {
            Ok(outcomes) => {
                let now = Instant::now();
                let mut converted = Vec::new();
                for outcome in outcomes {
                    self.stats.record_extension();
                    let remaining = outcome
                        .new_deadline
                        .checked_duration_since(now)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    converted.push(AgentExtendOutcome {
                        lease_id: outcome.lease_id,
                        new_deadline_ms: remaining,
                    });
                }
                Ok(LeaseExtendUpdate {
                    outcomes: converted,
                    revoked: Vec::new(),
                })
            }
            Err(BackendError::TaskRevoked { task_id }) => Ok(LeaseExtendUpdate {
                outcomes: Vec::new(),
                revoked: vec![task_id],
            }),
            Err(err) => Err(anyhow!(err)),
        }
    }
}

struct MockBackendDriver {
    request_rx: mpsc::Receiver<BackendRequest>,
    control_rx: mpsc::Receiver<ControlCommand>,
    behavior_rx: watch::Receiver<BackendBehavior>,
}

impl MockBackendDriver {
    fn new(
        request_rx: mpsc::Receiver<BackendRequest>,
        control_rx: mpsc::Receiver<ControlCommand>,
        behavior_rx: watch::Receiver<BackendBehavior>,
    ) -> Self {
        Self {
            request_rx,
            control_rx,
            behavior_rx,
        }
    }

    async fn run(mut self) {
        let mut next_agent_id = 1u64;
        let mut next_lease_id = 1u64;
        let mut queues: HashMap<TaskType, VecDeque<Task>> = HashMap::new();
        let mut leases: HashMap<u64, LeaseState> = HashMap::new();
        let mut agents: HashMap<AgentId, Instant> = HashMap::new();

        loop {
            let next_deadline = leases.values().map(|state| state.lease.lease_until).min();
            let mut sleep_fut = match next_deadline {
                Some(deadline) => Either::Left(Box::pin(time::sleep_until(deadline))),
                None => Either::Right(Box::pin(future::pending())),
            };

            select! {
                _ = async {
                    match &mut sleep_fut {
                        Either::Left(fut) => fut.as_mut().await,
                        Either::Right(fut) => fut.await,
                    }
                } => {
                    let now = Instant::now();
                    let mut requeue = Vec::new();
                    leases.retain(|_, state| {
                        if state.lease.lease_until <= now {
                            requeue.push(state.lease.task.clone());
                            false
                        } else {
                            true
                        }
                    });
                    if !requeue.is_empty() {
                        for task in requeue {
                            queues.entry(task.kind).or_default().push_back(task);
                        }
                    }
                }
                Some(cmd) = self.control_rx.recv() => {
                    match cmd {
                        ControlCommand::Enqueue(tasks) => {
                            for task in tasks {
                                queues.entry(task.kind).or_default().push_back(task);
                            }
                        }
                        ControlCommand::Shutdown => break,
                        ControlCommand::Snapshot(responder) => {
                            let behavior = self.behavior_rx.borrow().clone();
                            let mut pending = HashMap::new();
                            for kind in TaskType::ALL {
                                let queue = queues.get(&kind).cloned().unwrap_or_default();
                                pending.insert(kind, queue.into_iter().map(|task| task.id).collect());
                            }
                            let active = leases
                                .iter()
                                .map(|(&lease_id, state)| (lease_id, (state.lease.task.kind, state.lease.lease_until)))
                                .collect();
                            let _ = responder.send(BackendSnapshot {
                                pending,
                                active_leases: active,
                                revoked: behavior.revoked_tasks.clone(),
                            });
                        }
                    }
                }
                Some(request) = self.request_rx.recv() => {
                    match request {
                        BackendRequest::Register { respond_to } => {
                            let behavior = self.behavior_rx.borrow().clone();
                            let agent_id = AgentId(next_agent_id);
                            next_agent_id += 1;
                            agents.insert(agent_id, Instant::now());
                            let _ = respond_to.send(Ok(AgentRegistration {
                                agent_id,
                                lease_duration: behavior.lease_duration,
                                heartbeat_timeout: behavior.heartbeat_timeout,
                            }));
                        }
                        BackendRequest::Heartbeat { agent, respond_to } => {
                            let behavior = self.behavior_rx.borrow().clone();
                            match agents.get_mut(&agent) {
                                Some(last) => {
                                    let now = Instant::now();
                                    if now.duration_since(*last) > behavior.heartbeat_timeout {
                                        let _ = respond_to.send(Err(BackendError::HeartbeatTimeout { agent_id: agent }));
                                    } else {
                                        *last = now;
                                        let _ = respond_to.send(Ok(HeartbeatAck {
                                            agent_id: agent,
                                            next_deadline: now + behavior.heartbeat_timeout,
                                        }));
                                    }
                                }
                                None => {
                                    let _ = respond_to.send(Err(BackendError::UnknownAgent(agent)));
                                }
                            }
                        }
                        BackendRequest::Claim { request, respond_to } => {
                            if !agents.contains_key(&request.agent_id) {
                                let _ = respond_to.send(Err(BackendError::UnknownAgent(request.agent_id)));
                                continue;
                            }
                            let behavior = self.behavior_rx.borrow().clone();
                            let mut leases_vec = Vec::new();
                            for (kind, capacity) in request.capacities {
                                let queue = queues.entry(kind).or_default();
                                for _ in 0..capacity {
                                    if let Some(task) = queue.pop_front() {
                                        let leased_at = Instant::now();
                                        let mut lease_until = leased_at + behavior.lease_duration;
                                        if let Some(delay) = behavior.operation_delays.get(&BackendOp::Claim) {
                                            lease_until += *delay;
                                        }
                                        let leased = LeasedTask {
                                            lease_id: next_lease_id,
                                            task: task.clone(),
                                            lease_until,
                                            leased_at,
                                        };
                                        leases.insert(
                                            next_lease_id,
                                            LeaseState {
                                                lease: leased.clone(),
                                                agent: request.agent_id,
                                            },
                                        );
                                        next_lease_id += 1;
                                        leases_vec.push(leased);
                                    } else {
                                        break;
                                    }
                                }
                            }
                            let _ = respond_to.send(Ok(ClaimBatch { leases: leases_vec }));
                        }
                        BackendRequest::Extend { request, respond_to } => {
                            let behavior = self.behavior_rx.borrow().clone();
                            let mut outcomes = Vec::new();
                            let mut error = None;
                            for lease_id in &request.lease_ids {
                                if error.is_some() {
                                    break;
                                }
                                match leases.get_mut(lease_id) {
                                    Some(state) => {
                                        if state.agent != request.agent_id {
                                            error = Some(BackendError::UnknownAgent(request.agent_id));
                                        } else if behavior.revoked_tasks.contains(&state.lease.task.id) {
                                            error = Some(BackendError::TaskRevoked { task_id: state.lease.task.id });
                                        } else if state.lease.lease_until <= Instant::now() {
                                            error = Some(BackendError::LeaseExpired { lease_id: *lease_id });
                                        } else {
                                            state.lease.lease_until += request.extend_by;
                                            outcomes.push(ExtendOutcome {
                                                lease_id: *lease_id,
                                                new_deadline: state.lease.lease_until,
                                            });
                                        }
                                    }
                                    None => {
                                        error = Some(BackendError::LeaseExpired { lease_id: *lease_id });
                                    }
                                }
                            }
                            if let Some(err) = error {
                                let _ = respond_to.send(Err(err));
                            } else {
                                let _ = respond_to.send(Ok(outcomes));
                            }
                        }
                        BackendRequest::Report { request, respond_to } => {
                            let behavior = self.behavior_rx.borrow().clone();
                            let mut error = None;
                            for entry in &request.completed {
                                let lease_id = entry.lease_id;
                                if error.is_some() {
                                    break;
                                }
                                if let Some(state) = leases.remove(&lease_id) {
                                    if behavior.revoked_tasks.contains(&state.lease.task.id) {
                                        error = Some(BackendError::TaskRevoked { task_id: state.lease.task.id });
                                    }
                                }
                            }
                            if error.is_none() {
                                for entry in &request.cancelled {
                                    if let Some(state) = leases.remove(&entry.lease_id) {
                                        let _ = state;
                                    }
                                }
                            }
                            if let Some(err) = error {
                                let _ = respond_to.send(Err(err));
                            } else {
                                let _ = respond_to.send(Ok(()));
                            }
                        }
                    }
                }
                else => break,
            }
        }
    }
}

#[derive(Clone)]
pub struct AgentPipelineConfig {
    pub claim_batch_size: usize,
    pub extend_every: Duration,
    pub extend_by: Duration,
    pub heartbeat_interval: Duration,
    pub per_type_concurrency: HashMap<TaskType, usize>,
    pub processing_latency: HashMap<TaskType, Duration>,
}

impl AgentPipelineConfig {
    pub fn new(claim_batch_size: usize) -> Self {
        Self {
            claim_batch_size,
            extend_every: Duration::from_millis(50),
            extend_by: Duration::from_millis(100),
            heartbeat_interval: Duration::from_millis(100),
            per_type_concurrency: TaskType::ALL
                .iter()
                .copied()
                .map(|kind| (kind, 2))
                .collect(),
            processing_latency: TaskType::ALL
                .iter()
                .copied()
                .map(|kind| (kind, Duration::from_millis(40)))
                .collect(),
        }
    }
}

struct WorkerCommand {
    lease: LeasedTask,
    permit: ConcurrencyPermit,
    token: CancellationToken,
    extender: LeaseExtenderClient,
}

fn to_control_plane_lease(lease: &LeasedTask) -> dns_agent::control_plane::Lease {
    let remaining = lease
        .lease_until
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
        .as_millis() as u64;
    dns_agent::control_plane::Lease {
        lease_id: lease.lease_id,
        task_id: lease.task.id,
        kind: lease.task.kind.into(),
        lease_until_ms: remaining,
        spec: lease.task.spec.clone(),
    }
}

async fn process_task(
    agent_id: AgentId,
    kind: TaskType,
    latency: Duration,
    backend: MockBackend,
    stats: Arc<PipelineStats>,
    notify: Arc<Notify>,
    cmd: WorkerCommand,
) {
    let WorkerCommand {
        lease,
        permit,
        token,
        extender,
    } = cmd;
    stats.record_start(kind);
    let start = Instant::now();
    let lease_id = lease.lease_id;
    let task_id = lease.task.id;
    let sleep = time::sleep(latency);
    tokio::pin!(sleep);
    let cancelled = tokio::select! {
        _ = &mut sleep => token.is_cancelled(),
        _ = token.cancelled() => true,
    };

    if cancelled {
        stats.record_cancellation(kind, task_id);
        let _ = backend
            .report(ReportRequest {
                agent_id,
                completed: Vec::new(),
                cancelled: vec![LeaseReport {
                    lease_id,
                    observations: vec![Observation {
                        name: "cancelled".into(),
                        value: json!(true),
                        unit: None,
                    }],
                }],
            })
            .await;
    } else {
        stats.record_completion(kind, start.elapsed(), task_id);
        let latency = start.elapsed().as_secs_f64() * 1000.0;
        let _ = backend
            .report(ReportRequest {
                agent_id,
                completed: vec![LeaseReport {
                    lease_id,
                    observations: vec![Observation {
                        name: "latency_ms".into(),
                        value: json!(latency),
                        unit: Some("ms".into()),
                    }],
                }],
                cancelled: Vec::new(),
            })
            .await;
    }

    let _ = extender.finish(lease_id).await;
    drop(permit);
    notify.notify_waiters();
}

pub struct PipelineStats {
    total_completed: std::sync::atomic::AtomicU64,
    total_extensions: std::sync::atomic::AtomicU64,
    total_cancelled: std::sync::atomic::AtomicU64,
    per_type_completed: HashMap<TaskType, std::sync::atomic::AtomicU64>,
    per_type_current: HashMap<TaskType, std::sync::atomic::AtomicUsize>,
    per_type_peak: HashMap<TaskType, std::sync::atomic::AtomicUsize>,
    per_type_cancelled: HashMap<TaskType, std::sync::atomic::AtomicU64>,
    max_leases: std::sync::atomic::AtomicU64,
    current_leases: std::sync::atomic::AtomicU64,
    histogram: Mutex<Histogram<u64>>,
    completed_ids: Mutex<HashSet<u64>>,
}

impl PipelineStats {
    pub fn new() -> Arc<Self> {
        let histogram = Histogram::new_with_bounds(1, 60_000_000, 3).expect("hist bounds");
        let mut per_type_completed = HashMap::new();
        let mut per_type_current = HashMap::new();
        let mut per_type_peak = HashMap::new();
        let mut per_type_cancelled = HashMap::new();
        for kind in TaskType::ALL {
            per_type_completed.insert(kind, std::sync::atomic::AtomicU64::new(0));
            per_type_current.insert(kind, std::sync::atomic::AtomicUsize::new(0));
            per_type_peak.insert(kind, std::sync::atomic::AtomicUsize::new(0));
            per_type_cancelled.insert(kind, std::sync::atomic::AtomicU64::new(0));
        }
        Arc::new(PipelineStats {
            total_completed: std::sync::atomic::AtomicU64::new(0),
            total_extensions: std::sync::atomic::AtomicU64::new(0),
            total_cancelled: std::sync::atomic::AtomicU64::new(0),
            per_type_completed,
            per_type_current,
            per_type_peak,
            per_type_cancelled,
            max_leases: std::sync::atomic::AtomicU64::new(0),
            current_leases: std::sync::atomic::AtomicU64::new(0),
            histogram: Mutex::new(histogram),
            completed_ids: Mutex::new(HashSet::new()),
        })
    }

    fn update_peak(counter: &std::sync::atomic::AtomicUsize, value: usize) {
        let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
        while current < value {
            match counter.compare_exchange(
                current,
                value,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn record_start(&self, kind: TaskType) {
        if let Some(current) = self.per_type_current.get(&kind) {
            let value = current.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if let Some(peak) = self.per_type_peak.get(&kind) {
                Self::update_peak(peak, value);
            }
        }
        let total = self
            .current_leases
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let mut max_value = self.max_leases.load(std::sync::atomic::Ordering::Relaxed);
        while max_value < total {
            match self.max_leases.compare_exchange(
                max_value,
                total,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => max_value = actual,
            }
        }
    }

    pub fn record_completion(&self, kind: TaskType, latency: Duration, task_id: u64) {
        let is_new = self
            .completed_ids
            .lock()
            .map(|mut set| set.insert(task_id))
            .unwrap_or(true);
        if is_new {
            self.total_completed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(counter) = self.per_type_completed.get(&kind) {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(current) = self.per_type_current.get(&kind) {
            current.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.current_leases
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if is_new {
            if let Ok(mut histogram) = self.histogram.lock() {
                let _ = histogram.record(latency.as_micros() as u64);
            }
        }
    }

    pub fn record_cancellation(&self, kind: TaskType, task_id: u64) {
        let is_new = self
            .completed_ids
            .lock()
            .map(|mut set| set.insert(task_id))
            .unwrap_or(true);
        if is_new {
            self.total_cancelled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(counter) = self.per_type_cancelled.get(&kind) {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(current) = self.per_type_current.get(&kind) {
            current.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.current_leases
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_extension(&self) {
        self.total_extensions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn total_completed(&self) -> u64 {
        self.total_completed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn total_cancelled(&self) -> u64 {
        self.total_cancelled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn total_extensions(&self) -> u64 {
        self.total_extensions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn per_type_completed(&self, kind: TaskType) -> u64 {
        self.per_type_completed
            .get(&kind)
            .map(|value| value.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_default()
    }

    pub fn per_type_cancelled(&self, kind: TaskType) -> u64 {
        self.per_type_cancelled
            .get(&kind)
            .map(|value| value.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_default()
    }

    pub fn peak_inflight(&self, kind: TaskType) -> usize {
        self.per_type_peak
            .get(&kind)
            .map(|value| value.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_default()
    }

    pub fn max_leases(&self) -> u64 {
        self.max_leases.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn histogram(&self) -> Histogram<u64> {
        self.histogram
            .lock()
            .map(|hist| hist.clone())
            .unwrap_or_else(|_| Histogram::new(3).unwrap())
    }
}

pub struct PipelineHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
    heartbeat: JoinHandle<()>,
    worker_handles: Vec<JoinHandle<()>>,
    stats: Arc<PipelineStats>,
    lease_extender: LeaseExtenderHandle,
    timer: TimerService,
}

impl PipelineHandle {
    pub fn stats(&self) -> Arc<PipelineStats> {
        Arc::clone(&self.stats)
    }

    pub fn timer(&self) -> TimerService {
        self.timer.clone()
    }

    pub async fn wait_for_total(&self, expected: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.stats.total_completed() < expected && Instant::now() < deadline {
            if self.timer.sleep(Duration::from_millis(10)).await.is_err() {
                break;
            }
        }
    }

    pub async fn shutdown(self) -> Arc<PipelineStats> {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
        for handle in self.worker_handles {
            let _ = handle.await;
        }
        let _ = self.heartbeat.await;
        self.lease_extender.shutdown().await;
        self.stats
    }
}

pub struct AgentPipeline;

impl AgentPipeline {
    pub fn spawn(backend: MockBackend, config: AgentPipelineConfig) -> PipelineHandle {
        let stats = PipelineStats::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (agent_tx, agent_rx) = watch::channel::<Option<AgentId>>(None);
        let notify = Arc::new(Notify::new());
        let timer = TimerService::new();

        let extender_backend = BackendExtender::new(backend.clone(), stats.clone());
        let extender_config = LeaseExtenderConfig {
            extend_every: config.extend_every,
            extend_by: config.extend_by,
        };
        let (lease_extender_handle, lease_extender_client) =
            spawn_lease_extender(extender_backend.clone(), extender_config);

        let mut worker_handles = Vec::new();
        let mut worker_senders = HashMap::new();

        let per_kind_limits: HashMap<TaskKind, usize> = config
            .per_type_concurrency
            .iter()
            .map(|(kind, limit)| ((*kind).into(), *limit))
            .collect();
        let global_limit = per_kind_limits.values().copied().sum::<usize>().max(1);
        let controller = ConcurrencyController::new(ConcurrencyLimits::new(
            per_kind_limits.clone(),
            global_limit,
        ));

        for (&kind, &concurrency) in &config.per_type_concurrency {
            let (tx, mut rx) = mpsc::channel::<WorkerCommand>(concurrency * 4);
            worker_senders.insert(kind, tx);
            let backend_clone = backend.clone();
            let stats_clone = stats.clone();
            let mut agent_rx = agent_rx.clone();
            let notify_clone = notify.clone();
            let latency = config
                .processing_latency
                .get(&kind)
                .copied()
                .unwrap_or(Duration::from_millis(10));
            let handle = tokio::spawn(async move {
                let agent_id = loop {
                    if let Some(agent) = *agent_rx.borrow() {
                        break agent;
                    }
                    if agent_rx.changed().await.is_err() {
                        return;
                    }
                };
                while let Some(cmd) = rx.recv().await {
                    let backend_task = backend_clone.clone();
                    let stats_task = stats_clone.clone();
                    let notify_task = notify_clone.clone();
                    tokio::spawn(process_task(
                        agent_id,
                        kind,
                        latency,
                        backend_task,
                        stats_task,
                        notify_task,
                        cmd,
                    ));
                }
            });
            worker_handles.push(handle);
        }

        let backend_clone = backend.clone();
        let mut heartbeat_shutdown = shutdown_rx.clone();
        let mut heartbeat_agent_rx = agent_rx.clone();
        let heartbeat_interval = config.heartbeat_interval;
        let timer_heartbeat = timer.clone();
        let heartbeat = tokio::spawn(async move {
            let agent_id = loop {
                if let Some(agent) = *heartbeat_agent_rx.borrow() {
                    break agent;
                }
                if heartbeat_agent_rx.changed().await.is_err() {
                    return;
                }
            };
            loop {
                select! {
                    _ = heartbeat_shutdown.changed() => {
                        if *heartbeat_shutdown.borrow() {
                            break;
                        }
                    }
                    _ = async {
                        let timer = timer_heartbeat.clone();
                        let _ = timer.sleep(heartbeat_interval).await;
                    } => {
                        let _ = backend_clone.heartbeat(agent_id).await;
                    }
                }
            }
        });

        let join = {
            let backend = backend.clone();
            let mut shutdown_rx = shutdown_rx.clone();
            let worker_senders = worker_senders.clone();
            let controller = controller.clone();
            let notify = notify.clone();
            let extender_backend_loop = extender_backend.clone();
            let lease_extender_client_loop = lease_extender_client.clone();
            let timer_worker = timer.clone();
            tokio::spawn(async move {
                let registration = backend.register().await.expect("register succeeded");
                let agent_id = registration.agent_id;
                let _ = agent_tx.send(Some(agent_id));
                extender_backend_loop.set_agent(agent_id).await;
                loop {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    let mut capacities: BTreeMap<TaskType, usize> = BTreeMap::new();
                    let mut total = 0usize;
                    for kind in TaskType::ALL {
                        let available = controller.available(kind.into());
                        if available > 0 {
                            capacities.insert(kind, available);
                            total += available;
                        }
                    }
                    if capacities.is_empty() {
                        select! {
                            _ = notify.notified() => {},
                            _ = shutdown_rx.changed() => {},
                        }
                        continue;
                    }
                    if total > config.claim_batch_size {
                        let mut remaining = config.claim_batch_size;
                        for value in capacities.values_mut() {
                            if remaining == 0 {
                                *value = 0;
                            } else if *value > remaining {
                                *value = remaining;
                                remaining = 0;
                            } else {
                                remaining -= *value;
                            }
                        }
                        capacities.retain(|_, v| *v > 0);
                    }
                    if capacities.is_empty() {
                        select! {
                            _ = notify.notified() => {},
                            _ = shutdown_rx.changed() => {},
                        }
                        continue;
                    }
                    match backend
                        .claim(ClaimRequest {
                            agent_id,
                            capacities: capacities.clone(),
                        })
                        .await
                    {
                        Ok(batch) => {
                            if batch.leases.is_empty() {
                                let timer_clone = timer_worker.clone();
                                let _ = timer_clone.sleep(Duration::from_millis(10)).await;
                                continue;
                            }
                            for lease in batch.leases {
                                if let Some(sender) = worker_senders.get(&lease.task.kind) {
                                    let task_kind: TaskKind = lease.task.kind.into();
                                    let controller_clone = controller.clone();
                                    let permit =
                                        controller_clone.acquire(task_kind, lease.task.id).await;
                                    let token = CancellationToken::new();
                                    let lease_view = to_control_plane_lease(&lease);
                                    let _ =
                                        lease_extender_client_loop.track(&lease_view, &token).await;
                                    let command = WorkerCommand {
                                        lease,
                                        permit,
                                        token,
                                        extender: lease_extender_client_loop.clone(),
                                    };
                                    let _ = sender.send(command).await;
                                }
                            }
                        }
                        Err(_) => {
                            let timer_clone = timer_worker.clone();
                            let _ = timer_clone.sleep(Duration::from_millis(20)).await;
                        }
                    }
                }
                drop(worker_senders);
            })
        };

        PipelineHandle {
            shutdown_tx,
            join,
            heartbeat,
            worker_handles,
            stats,
            lease_extender: lease_extender_handle,
            timer,
        }
    }
}

pub fn build_tasks(counts: &HashMap<TaskType, usize>) -> Vec<Task> {
    let mut tasks = Vec::new();
    let mut next_id = 1u64;
    for kind in TaskType::ALL {
        if let Some(count) = counts.get(&kind) {
            for _ in 0..*count {
                tasks.push(Task::new(next_id, kind));
                next_id += 1;
            }
        }
    }
    tasks
}
