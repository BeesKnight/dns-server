use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::control_plane::{Lease, TaskKind};

/// Configuration for the dispatcher runtime.
#[derive(Clone, Debug)]
pub struct DispatcherConfig {
    pub dispatch_capacity: usize,
    pub dns_queue_capacity: usize,
    pub http_queue_capacity: usize,
    pub tcp_queue_capacity: usize,
    pub ping_queue_capacity: usize,
    pub trace_queue_capacity: usize,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            dispatch_capacity: 64,
            dns_queue_capacity: 32,
            http_queue_capacity: 32,
            tcp_queue_capacity: 32,
            ping_queue_capacity: 32,
            trace_queue_capacity: 32,
        }
    }
}

#[derive(Debug)]
struct DispatchInner {
    dispatch_tx: mpsc::Sender<DispatchJob>,
    backpressure: watch::Sender<bool>,
}

/// Entry point for submitting leases into the dispatcher.
#[derive(Clone, Debug)]
pub struct DispatchIngress {
    inner: Arc<DispatchInner>,
}

impl DispatchIngress {
    pub async fn dispatch(&self, lease: Lease) -> Result<(), DispatchError> {
        let job = DispatchJob::new(lease);
        self.inner
            .dispatch_tx
            .send(job)
            .await
            .map_err(|_| DispatchError::Closed)
    }

    pub async fn dispatch_batch<I>(&self, leases: I) -> Result<(), DispatchError>
    where
        I: IntoIterator<Item = Lease>,
    {
        for lease in leases.into_iter() {
            self.dispatch(lease).await?;
        }
        Ok(())
    }

    pub fn subscribe_backpressure(&self) -> watch::Receiver<bool> {
        self.inner.backpressure.subscribe()
    }
}

/// Receivers for each per-kind queue.
pub struct DispatchQueues {
    pub dns: mpsc::Receiver<DispatchedLease>,
    pub http: mpsc::Receiver<DispatchedLease>,
    pub tcp: mpsc::Receiver<DispatchedLease>,
    pub ping: mpsc::Receiver<DispatchedLease>,
    pub trace: mpsc::Receiver<DispatchedLease>,
}

/// Handle to the background dispatcher task.
pub struct DispatcherHandle {
    join: JoinHandle<()>,
}

impl DispatcherHandle {
    pub async fn shutdown(self) {
        let _ = self.join.await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatcher has been shut down")]
    Closed,
}

struct DispatchJob {
    lease: Lease,
    enqueued_at: Instant,
}

impl DispatchJob {
    fn new(lease: Lease) -> Self {
        Self {
            lease,
            enqueued_at: Instant::now(),
        }
    }
}

struct QueueEntry {
    sender: mpsc::Sender<DispatchedLease>,
    tracker: Arc<QueueTracker>,
}

impl QueueEntry {
    fn new(sender: mpsc::Sender<DispatchedLease>, kind: TaskKind) -> Self {
        Self {
            sender,
            tracker: Arc::new(QueueTracker::new(kind)),
        }
    }
}

struct QueueTracker {
    kind: TaskKind,
    depth: AtomicUsize,
}

impl QueueTracker {
    fn new(kind: TaskKind) -> Self {
        Self {
            kind,
            depth: AtomicUsize::new(0),
        }
    }

    fn kind(&self) -> TaskKind {
        self.kind
    }

    fn increment(&self) {
        let depth = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
        metrics::gauge!("dispatcher.queue.fill", "kind" => kind_label(self.kind)).set(depth as f64);
    }

    fn decrement(&self) {
        let mut current = self.depth.load(Ordering::SeqCst);
        loop {
            if current == 0 {
                return;
            }
            let next = current - 1;
            match self
                .depth
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    metrics::gauge!("dispatcher.queue.fill", "kind" => kind_label(self.kind))
                        .set(next as f64);
                    break;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

fn kind_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Dns => "dns",
        TaskKind::Http => "http",
        TaskKind::Tcp => "tcp",
        TaskKind::Ping => "ping",
        TaskKind::Trace => "trace",
    }
}

/// Item routed into a per-kind queue.
pub struct DispatchedLease {
    lease: Option<Lease>,
    enqueued_at: Instant,
    tracker: Arc<QueueTracker>,
    counted: bool,
}

impl DispatchedLease {
    fn new(lease: Lease, enqueued_at: Instant, tracker: Arc<QueueTracker>) -> Self {
        Self {
            lease: Some(lease),
            enqueued_at,
            tracker,
            counted: false,
        }
    }

    fn mark_enqueued(mut self) -> Self {
        if !self.counted {
            self.tracker.increment();
            self.counted = true;
        }
        self
    }

    fn undo_enqueue(&mut self) {
        if self.counted {
            self.tracker.decrement();
            self.counted = false;
        }
    }

    pub fn lease(&self) -> &Lease {
        self.lease.as_ref().expect("lease already taken")
    }

    pub fn into_lease(mut self) -> Lease {
        let lease = self.lease.take().expect("lease already taken");
        let elapsed = self.enqueued_at.elapsed().as_secs_f64();
        metrics::histogram!(
            "dispatcher.queue.wait_time",
            "kind" => kind_label(self.tracker.kind())
        )
        .record(elapsed);
        lease
    }
}

impl Drop for DispatchedLease {
    fn drop(&mut self) {
        if self.counted {
            self.tracker.decrement();
            self.counted = false;
        }
    }
}

struct QueueTable {
    dns: QueueEntry,
    http: QueueEntry,
    tcp: QueueEntry,
    ping: QueueEntry,
    trace: QueueEntry,
}

impl QueueTable {
    fn entry(&self, kind: TaskKind) -> &QueueEntry {
        match kind {
            TaskKind::Dns => &self.dns,
            TaskKind::Http => &self.http,
            TaskKind::Tcp => &self.tcp,
            TaskKind::Ping => &self.ping,
            TaskKind::Trace => &self.trace,
        }
    }
}

/// Spawns the dispatcher runtime and returns handles for interacting with it.
pub fn spawn_dispatcher(
    config: DispatcherConfig,
) -> (DispatchIngress, DispatchQueues, DispatcherHandle) {
    let (dispatch_tx, dispatch_rx) = mpsc::channel(config.dispatch_capacity);

    let (dns_tx, dns_rx) = mpsc::channel(config.dns_queue_capacity);
    let (http_tx, http_rx) = mpsc::channel(config.http_queue_capacity);
    let (tcp_tx, tcp_rx) = mpsc::channel(config.tcp_queue_capacity);
    let (ping_tx, ping_rx) = mpsc::channel(config.ping_queue_capacity);
    let (trace_tx, trace_rx) = mpsc::channel(config.trace_queue_capacity);

    let (backpressure_tx, _) = watch::channel(false);

    let inner = Arc::new(DispatchInner {
        dispatch_tx,
        backpressure: backpressure_tx.clone(),
    });

    let ingress = DispatchIngress {
        inner: Arc::clone(&inner),
    };

    let queues = DispatchQueues {
        dns: dns_rx,
        http: http_rx,
        tcp: tcp_rx,
        ping: ping_rx,
        trace: trace_rx,
    };

    let table = QueueTable {
        dns: QueueEntry::new(dns_tx, TaskKind::Dns),
        http: QueueEntry::new(http_tx, TaskKind::Http),
        tcp: QueueEntry::new(tcp_tx, TaskKind::Tcp),
        ping: QueueEntry::new(ping_tx, TaskKind::Ping),
        trace: QueueEntry::new(trace_tx, TaskKind::Trace),
    };

    let handle = tokio::spawn(run_dispatcher(dispatch_rx, table, backpressure_tx));

    (ingress, queues, DispatcherHandle { join: handle })
}

async fn run_dispatcher(
    mut dispatch_rx: mpsc::Receiver<DispatchJob>,
    table: QueueTable,
    backpressure: watch::Sender<bool>,
) {
    let mut backpressure_active = false;
    while let Some(job) = dispatch_rx.recv().await {
        let entry = table.entry(job.lease.kind);
        let sender = entry.sender.clone();
        let tracker = Arc::clone(&entry.tracker);
        let item = DispatchedLease::new(job.lease, job.enqueued_at, tracker);
        enqueue_item(sender, item, &backpressure, &mut backpressure_active).await;
    }

    if backpressure_active {
        let _ = backpressure.send(false);
    }
    drop(backpressure);
}

async fn enqueue_item(
    sender: mpsc::Sender<DispatchedLease>,
    item: DispatchedLease,
    backpressure: &watch::Sender<bool>,
    backpressure_active: &mut bool,
) {
    use tokio::sync::mpsc::error::TrySendError;

    let item = item.mark_enqueued();
    match sender.try_send(item) {
        Ok(()) => {}
        Err(TrySendError::Full(mut item)) => {
            item.undo_enqueue();
            if !*backpressure_active {
                let _ = backpressure.send(true);
                *backpressure_active = true;
            }
            let item = item.mark_enqueued();
            match sender.send(item).await {
                Ok(()) => {
                    if *backpressure_active {
                        let _ = backpressure.send(false);
                        *backpressure_active = false;
                    }
                }
                Err(err) => {
                    let mut item = err.0;
                    item.undo_enqueue();
                    if *backpressure_active {
                        let _ = backpressure.send(false);
                        *backpressure_active = false;
                    }
                }
            }
        }
        Err(TrySendError::Closed(mut item)) => {
            item.undo_enqueue();
        }
    }
}
