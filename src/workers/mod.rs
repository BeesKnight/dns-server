use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, warn};

use crate::concurrency::{ConcurrencyController, ConcurrencyLimits};
use crate::control_plane::{
    ControlPlaneClient, ControlPlaneTransport, LeaseReport, Observation, TaskKind,
};
use crate::dispatcher::{DispatchQueues, DispatchedLease, LeaseAssignment};
use crate::lease_extender::LeaseExtenderClient;

mod ping;
mod tcp;
mod trace;

pub use ping::{
    PingEngine, PingFailureReason, PingObservation, PingProbeOutcome, PingProtocol, PingRequest,
    PingWorker,
};
pub use tcp::{TcpAttempt, TcpObservation, TcpWorker};
pub use trace::{
    ParisFlowId, RawSocketCapability, TraceEngine, TraceHop, TraceObservation, TraceProbeOutcome,
    TraceProbeRequest, TraceProtocol, TraceStatus, TraceWorker,
};

/// Trait that exposes the set of supported task kinds.
trait TaskKindExt {
    const ALL: [TaskKind; 5];
}

impl TaskKindExt for TaskKind {
    const ALL: [TaskKind; 5] = [
        TaskKind::Dns,
        TaskKind::Http,
        TaskKind::Tcp,
        TaskKind::Ping,
        TaskKind::Trace,
    ];
}

/// Configuration for the worker pools used to execute leased tasks.
#[derive(Clone, Debug)]
pub struct WorkerPoolsConfig {
    per_kind: HashMap<TaskKind, usize>,
    trace_runtime_workers: usize,
}

impl WorkerPoolsConfig {
    /// Creates a new configuration with sensible defaults.
    pub fn new() -> Self {
        let mut per_kind = HashMap::new();
        per_kind.insert(TaskKind::Dns, 4);
        per_kind.insert(TaskKind::Http, 2);
        per_kind.insert(TaskKind::Tcp, 2);
        per_kind.insert(TaskKind::Ping, 2);
        per_kind.insert(TaskKind::Trace, 1);
        Self {
            per_kind,
            trace_runtime_workers: 2,
        }
    }

    /// Builds a configuration from environment variables.
    pub fn from_env() -> Self {
        let mut config = Self::new();
        for kind in <TaskKind as TaskKindExt>::ALL {
            let key = format!("AGENT_POOL_{}_SIZE", kind.to_string().to_uppercase());
            if let Ok(value) = env::var(&key) {
                if let Ok(parsed) = value.parse::<usize>() {
                    if parsed > 0 {
                        config.per_kind.insert(kind, parsed);
                    }
                }
            }
        }
        if let Ok(value) = env::var("AGENT_TRACE_RUNTIME_WORKERS") {
            if let Ok(parsed) = value.parse::<usize>() {
                if parsed > 0 {
                    config.trace_runtime_workers = parsed;
                }
            }
        }
        config
    }

    /// Returns the configured concurrency for a particular task kind.
    pub fn concurrency(&self, kind: TaskKind) -> usize {
        self.per_kind
            .get(&kind)
            .copied()
            .filter(|value| *value > 0)
            .unwrap_or(1)
    }

    /// Returns the number of worker threads allocated to the trace runtime.
    pub fn trace_runtime_workers(&self) -> usize {
        self.trace_runtime_workers.max(1)
    }

    /// Updates the concurrency for a task kind.
    pub fn set_concurrency(&mut self, kind: TaskKind, value: usize) {
        let value = value.max(1);
        self.per_kind.insert(kind, value);
    }

    pub fn concurrency_limits(&self) -> ConcurrencyLimits {
        let per_kind = self.per_kind.clone();
        let global = per_kind.values().copied().sum::<usize>().max(1);
        ConcurrencyLimits::new(per_kind, global)
    }
}

impl Default for WorkerPoolsConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait used to report completed leases back to the control plane.
#[async_trait]
pub trait ReportSink: Clone + Send + Sync + 'static {
    async fn report(&self, completed: Vec<LeaseReport>, cancelled: Vec<LeaseReport>) -> Result<()>;
}

#[async_trait]
impl<T> ReportSink for ControlPlaneClient<T>
where
    T: ControlPlaneTransport + Send + Sync + 'static,
{
    async fn report(&self, completed: Vec<LeaseReport>, cancelled: Vec<LeaseReport>) -> Result<()> {
        let _ = <ControlPlaneClient<T>>::report(self, completed, cancelled)
            .await
            .map_err(|err| anyhow!(err))?;
        Ok(())
    }
}

/// Trait implemented by worker handlers for each task type.
#[async_trait]
pub trait WorkerHandler: Send + Sync {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport>;
}

/// Outcome reported by worker handlers.
#[derive(Debug)]
pub struct WorkerReport {
    completed: Vec<LeaseReport>,
    cancelled: Vec<LeaseReport>,
}

impl WorkerReport {
    pub fn completed(lease_id: u64) -> Self {
        Self {
            completed: vec![LeaseReport {
                lease_id,
                observations: Vec::new(),
            }],
            cancelled: Vec::new(),
        }
    }

    pub fn completed_many(ids: Vec<u64>) -> Self {
        Self {
            completed: ids
                .into_iter()
                .map(|lease_id| LeaseReport {
                    lease_id,
                    observations: Vec::new(),
                })
                .collect(),
            cancelled: Vec::new(),
        }
    }

    pub fn cancelled(lease_id: u64) -> Self {
        Self {
            completed: Vec::new(),
            cancelled: vec![LeaseReport {
                lease_id,
                observations: Vec::new(),
            }],
        }
    }

    pub fn cancelled_many(ids: Vec<u64>) -> Self {
        Self {
            completed: Vec::new(),
            cancelled: ids
                .into_iter()
                .map(|lease_id| LeaseReport {
                    lease_id,
                    observations: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn merge(mut self, mut other: WorkerReport) -> Self {
        self.completed.append(&mut other.completed);
        self.cancelled.append(&mut other.cancelled);
        self
    }

    pub fn with_observation(self, lease_id: u64, observation: Observation) -> Self {
        self.with_observations(lease_id, std::iter::once(observation))
    }

    pub fn with_observations<I>(mut self, lease_id: u64, observations: I) -> Self
    where
        I: IntoIterator<Item = Observation>,
    {
        if let Some(entry) = self
            .completed
            .iter_mut()
            .chain(self.cancelled.iter_mut())
            .find(|entry| entry.lease_id == lease_id)
        {
            entry.observations.extend(observations);
        }
        self
    }

    pub fn into_parts(self) -> (Vec<LeaseReport>, Vec<LeaseReport>) {
        (self.completed, self.cancelled)
    }

    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.cancelled.is_empty()
    }
}

/// Container struct that stores handlers for each task kind.
#[derive(Clone)]
pub struct WorkerHandlers {
    pub dns: Arc<dyn WorkerHandler>,
    pub http: Arc<dyn WorkerHandler>,
    pub tcp: Arc<dyn WorkerHandler>,
    pub ping: Arc<dyn WorkerHandler>,
    pub trace: Arc<dyn WorkerHandler>,
}

impl WorkerHandlers {
    pub fn new(
        dns: Arc<dyn WorkerHandler>,
        http: Arc<dyn WorkerHandler>,
        tcp: Arc<dyn WorkerHandler>,
        ping: Arc<dyn WorkerHandler>,
        trace: Arc<dyn WorkerHandler>,
    ) -> Self {
        Self {
            dns,
            http,
            tcp,
            ping,
            trace,
        }
    }

    pub fn default_handlers() -> Self {
        Self {
            dns: Arc::new(DnsWorker::default()),
            http: Arc::new(HttpWorker::default()),
            tcp: Arc::new(TcpWorker::default()),
            ping: Arc::new(PingWorker::default()),
            trace: Arc::new(TraceWorker::default()),
        }
    }
}

impl Default for WorkerHandlers {
    fn default() -> Self {
        Self::default_handlers()
    }
}

/// Handle returned to manage the lifecycle of worker pools.
pub struct WorkerPoolsHandle {
    joins: Vec<JoinHandle<()>>,
    trace_guard: Option<TraceRuntimeGuard>,
}

impl WorkerPoolsHandle {
    pub async fn shutdown(self) {
        for join in self.joins {
            if let Err(err) = join.await {
                if err.is_panic() {
                    error!("worker pool task panicked during shutdown");
                }
            }
        }
        if let Some(guard) = self.trace_guard {
            guard.shutdown().await;
        }
    }
}

/// Spawns worker pools for each task kind, wiring them to their corresponding queues.
pub fn spawn_worker_pools<R>(
    config: WorkerPoolsConfig,
    queues: DispatchQueues,
    handlers: WorkerHandlers,
    reporter: R,
    lease_extender: Option<LeaseExtenderClient>,
) -> Result<WorkerPoolsHandle>
where
    R: ReportSink,
{
    let WorkerHandlers {
        dns,
        http,
        tcp,
        ping,
        trace,
    } = handlers;

    let DispatchQueues {
        dns: dns_queue,
        http: http_queue,
        tcp: tcp_queue,
        ping: ping_queue,
        trace: trace_queue,
    } = queues;

    let mut joins = Vec::new();

    let reporter_http = reporter.clone();
    let reporter_tcp = reporter.clone();
    let reporter_ping = reporter.clone();
    let reporter_trace = reporter.clone();

    let controller = ConcurrencyController::new(config.concurrency_limits());

    let lease_extender_dns = lease_extender.clone();
    joins.push(spawn_pool(
        "dns",
        TaskKind::Dns,
        dns_queue,
        dns,
        reporter,
        controller.clone(),
        None,
        lease_extender_dns,
    ));

    let lease_extender_http = lease_extender.clone();
    joins.push(spawn_pool(
        "http",
        TaskKind::Http,
        http_queue,
        http,
        reporter_http,
        controller.clone(),
        None,
        lease_extender_http,
    ));

    let lease_extender_tcp = lease_extender.clone();
    joins.push(spawn_pool(
        "tcp",
        TaskKind::Tcp,
        tcp_queue,
        tcp,
        reporter_tcp,
        controller.clone(),
        None,
        lease_extender_tcp,
    ));

    let lease_extender_ping = lease_extender.clone();
    joins.push(spawn_pool(
        "ping",
        TaskKind::Ping,
        ping_queue,
        ping,
        reporter_ping,
        controller.clone(),
        None,
        lease_extender_ping,
    ));

    let (trace_runtime, trace_guard) = TraceRuntime::new(config.trace_runtime_workers())?;
    let lease_extender_trace = lease_extender;
    joins.push(spawn_pool(
        "trace",
        TaskKind::Trace,
        trace_queue,
        trace,
        reporter_trace,
        controller,
        Some(trace_runtime),
        lease_extender_trace,
    ));

    Ok(WorkerPoolsHandle {
        joins,
        trace_guard: Some(trace_guard),
    })
}

fn spawn_pool<R>(
    label: &'static str,
    kind: TaskKind,
    mut queue: mpsc::Receiver<DispatchedLease>,
    handler: Arc<dyn WorkerHandler>,
    reporter: R,
    controller: ConcurrencyController,
    trace_runtime: Option<TraceRuntime>,
    lease_extender: Option<LeaseExtenderClient>,
) -> JoinHandle<()>
where
    R: ReportSink,
{
    tokio::spawn(async move {
        let mut joinset = JoinSet::new();
        let inflight_gauge = Arc::new(AtomicUsize::new(0));
        while let Some(item) = queue.recv().await {
            let assignment = item.into_assignment();
            let lease_id = assignment.lease().lease_id;
            let target = assignment.lease().task_id;
            let controller_for_task = controller.clone();
            let permit = controller_for_task.acquire(kind, target).await;
            let tracker = Arc::clone(&inflight_gauge);
            let handler = Arc::clone(&handler);
            let reporter = reporter.clone();
            let runtime = trace_runtime.clone();
            let lease_extender_for_task = lease_extender.clone();
            let start = Instant::now();
            tracker.fetch_add(1, Ordering::SeqCst);
            metrics::gauge!("workers.inflight", "kind" => label)
                .set(tracker.load(Ordering::SeqCst) as f64);

            let task = async move {
                let result = AssertUnwindSafe(handler.handle(assignment))
                    .catch_unwind()
                    .await;
                match result {
                    Ok(Ok(report)) => {
                        controller_for_task.record_success(kind, target);
                        let (completed, cancelled) = report.into_parts();
                        if !completed.is_empty() {
                            metrics::counter!("workers.completed", "kind" => label)
                                .increment(completed.len() as u64);
                        }
                        if !cancelled.is_empty() {
                            metrics::counter!("workers.cancelled", "kind" => label)
                                .increment(cancelled.len() as u64);
                        }
                        if !completed.is_empty() || !cancelled.is_empty() {
                            if let Err(err) = reporter.report(completed, cancelled).await {
                                warn!(error = %err, kind = label, "failed to report lease outcome");
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        controller_for_task.record_error(kind, target);
                        metrics::counter!("workers.errors", "kind" => label).increment(1);
                        warn!(error = %err, kind = label, "worker handler returned error");
                    }
                    Err(panic) => {
                        controller_for_task.record_error(kind, target);
                        metrics::counter!("workers.panics", "kind" => label).increment(1);
                        error!(kind = label, "worker panicked: {:?}", panic);
                    }
                }
                metrics::histogram!("workers.latency", "kind" => label)
                    .record(start.elapsed().as_secs_f64());
                drop(permit);
                tracker.fetch_sub(1, Ordering::SeqCst);
                metrics::gauge!("workers.inflight", "kind" => label)
                    .set(tracker.load(Ordering::SeqCst) as f64);
                if let Some(extender) = lease_extender_for_task {
                    if let Err(err) = extender.finish(lease_id).await {
                        warn!(error = %err, kind = label, "failed to finalize lease tracking");
                    }
                }
            };

            joinset.spawn(async move {
                if let Some(runtime) = runtime {
                    if let Err(err) = runtime.spawn(task).await {
                        warn!(error = %err, kind = label, "trace runtime failed to spawn task");
                    }
                } else {
                    let _ = task.await;
                }
            });
        }
        drop(queue);
        while let Some(result) = joinset.join_next().await {
            if let Err(err) = result {
                if err.is_panic() {
                    error!(kind = label, "worker join panicked");
                }
            }
        }
    })
}

/// Default worker implementation for DNS checks.
#[derive(Default)]
pub struct DnsWorker;

#[async_trait]
impl WorkerHandler for DnsWorker {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "DNS lease cancelled before start");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }
        debug!(lease_id = lease.lease_id, kind = ?lease.kind, "processing DNS lease");
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "DNS lease cancelled during processing");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }
        Ok(WorkerReport::completed(lease.lease_id))
    }
}

/// Default worker implementation for HTTP checks.
#[derive(Default)]
pub struct HttpWorker;

#[async_trait]
impl WorkerHandler for HttpWorker {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "HTTP lease cancelled before start");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }
        debug!(lease_id = lease.lease_id, kind = ?lease.kind, "processing HTTP lease");
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "HTTP lease cancelled during processing");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }
        Ok(WorkerReport::completed(lease.lease_id))
    }
}

#[derive(Clone)]
struct TraceRuntime {
    inner: Arc<TraceRuntimeInner>,
}

struct TraceRuntimeInner {
    tx: mpsc::Sender<TraceCommand>,
}

pub struct TraceRuntimeGuard {
    tx: mpsc::Sender<TraceCommand>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

enum TraceCommand {
    Run { task: BoxFuture<'static, ()> },
    Shutdown,
}

impl TraceRuntime {
    fn new(worker_threads: usize) -> Result<(Self, TraceRuntimeGuard)> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .worker_threads(worker_threads)
            .thread_name("trace-worker")
            .build()
            .context("failed to initialize trace runtime")?;
        let (tx, rx) = mpsc::channel::<TraceCommand>(worker_threads.max(1) * 4);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let join = std::thread::spawn(move || {
            runtime.block_on(run_trace_runtime(rx, shutdown_rx));
        });

        let guard = TraceRuntimeGuard {
            tx: tx.clone(),
            shutdown: Some(shutdown_tx),
            join: Some(join),
        };

        let inner = TraceRuntimeInner { tx };
        Ok((
            TraceRuntime {
                inner: Arc::new(inner),
            },
            guard,
        ))
    }

    async fn spawn(
        &self,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), TraceRuntimeError> {
        let (done_tx, done_rx) = oneshot::channel();
        let task = async move {
            task.await;
            let _ = done_tx.send(());
        };
        self.inner
            .tx
            .send(TraceCommand::Run {
                task: Box::pin(task),
            })
            .await
            .map_err(|_| TraceRuntimeError::ChannelClosed)?;
        done_rx
            .await
            .map_err(|_| TraceRuntimeError::ChannelClosed)?;
        Ok(())
    }
}

impl TraceRuntimeGuard {
    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.tx.send(TraceCommand::Shutdown).await;
        if let Some(join) = self.join.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = join.join();
            })
            .await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TraceRuntimeError {
    #[error("trace runtime channel closed")]
    ChannelClosed,
}

async fn run_trace_runtime(
    mut rx: mpsc::Receiver<TraceCommand>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut joinset = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                break;
            }
            Some(cmd) = rx.recv() => {
                match cmd {
                    TraceCommand::Run { task } => {
                        joinset.spawn(task);
                    }
                    TraceCommand::Shutdown => break,
                }
            }
            else => break,
        }
    }
    drop(rx);
    while let Some(result) = joinset.join_next().await {
        if let Err(err) = result {
            if err.is_panic() {
                error!("trace runtime task panicked");
            }
        }
    }
}
