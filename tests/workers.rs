mod support;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::Instant;

use codecrafters_dns_server::control_plane::{Lease, TaskKind};
use codecrafters_dns_server::dispatcher::{spawn_dispatcher, DispatcherConfig};
use codecrafters_dns_server::workers::{
    spawn_worker_pools, ReportSink, WorkerHandler, WorkerHandlers, WorkerPoolsConfig,
};

include!(concat!(env!("OUT_DIR"), "/worker_test_macro.rs"));

worker_test!(worker_pool_respects_concurrency_limits, {
    let dispatcher_config = DispatcherConfig {
        dispatch_capacity: 16,
        dns_queue_capacity: 16,
        http_queue_capacity: 4,
        tcp_queue_capacity: 4,
        ping_queue_capacity: 4,
        trace_queue_capacity: 4,
    };

    let (ingress, queues, dispatcher_handle) = spawn_dispatcher(dispatcher_config);
    let reporter = TestReporter::default();

    let mut pool_config = WorkerPoolsConfig::new();
    pool_config.set_concurrency(TaskKind::Dns, 2);

    let handler = Arc::new(LatchingHandler::new(2));
    let mut handlers = WorkerHandlers::default();
    handlers.dns = handler.clone();

    let pools = spawn_worker_pools(pool_config, queues, handlers, reporter.clone())
        .expect("worker pools spawn");

    for id in 1..=6 {
        ingress
            .dispatch(Lease {
                lease_id: id,
                task_id: id,
                kind: TaskKind::Dns,
                lease_until_ms: 0,
            })
            .await
            .expect("dispatch lease");
    }

    handler.wait_for_inflight(2, Duration::from_secs(1)).await;
    assert_eq!(handler.peak(), 2, "peak inflight should match concurrency");

    handler.release_all();
    reporter.wait_for_total(6, Duration::from_secs(2)).await;

    drop(ingress);
    pools.shutdown().await;
    dispatcher_handle.shutdown().await;
});

worker_test!(worker_pool_recovers_from_panics, {
    let dispatcher_config = DispatcherConfig::default();
    let (ingress, queues, dispatcher_handle) = spawn_dispatcher(dispatcher_config);

    let reporter = TestReporter::default();

    let mut pool_config = WorkerPoolsConfig::new();
    pool_config.set_concurrency(TaskKind::Dns, 2);

    let handler = Arc::new(PanicOnceHandler::new());
    let mut handlers = WorkerHandlers::default();
    handlers.dns = handler.clone();

    let pools = spawn_worker_pools(pool_config, queues, handlers, reporter.clone())
        .expect("spawn worker pools");

    for id in 1..=3 {
        ingress
            .dispatch(Lease {
                lease_id: id,
                task_id: id,
                kind: TaskKind::Dns,
                lease_until_ms: 0,
            })
            .await
            .expect("dispatch lease");
    }

    reporter.wait_for_total(2, Duration::from_secs(2)).await;

    drop(ingress);
    pools.shutdown().await;
    dispatcher_handle.shutdown().await;

    let completed = reporter.completed_async().await;
    assert_eq!(
        completed.len(),
        2,
        "two tasks should complete despite panic"
    );
    let mut unique = completed.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        2,
        "completed lease identifiers should be unique"
    );
    for id in &unique {
        assert!((1..=3).contains(id), "unexpected lease id {id} reported");
    }
});

#[derive(Clone, Default)]
struct TestReporter {
    inner: Arc<TestReporterInner>,
}

#[derive(Default)]
struct TestReporterInner {
    completed: Mutex<Vec<u64>>,
    notify: Notify,
}

#[async_trait]
impl ReportSink for TestReporter {
    async fn report(&self, completed: Vec<u64>) -> Result<()> {
        let mut guard = self.inner.completed.lock().await;
        guard.extend(completed);
        drop(guard);
        self.inner.notify.notify_waiters();
        Ok(())
    }
}

impl TestReporter {
    async fn wait_for_total(&self, expected: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let guard = self.inner.completed.lock().await;
                if guard.len() >= expected {
                    return;
                }
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for reports");
            }
            self.inner.notify.notified().await;
        }
    }

    async fn completed_async(&self) -> Vec<u64> {
        self.inner.completed.lock().await.clone()
    }
}

struct LatchingHandler {
    current: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
    started: Notify,
    release: Arc<Semaphore>,
}

impl LatchingHandler {
    fn new(limit: usize) -> Self {
        Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit,
            started: Notify::new(),
            release: Arc::new(Semaphore::new(0)),
        }
    }

    async fn wait_for_inflight(&self, expected: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let current = self.current.load(Ordering::SeqCst);
            if current >= expected {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for inflight workers");
            }
            self.started.notified().await;
        }
    }

    fn release_all(&self) {
        self.release.add_permits(self.limit * 4);
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl WorkerHandler for LatchingHandler {
    async fn handle(&self, lease: Lease) -> Result<codecrafters_dns_server::workers::WorkerReport> {
        let inflight = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        loop {
            let observed = self.peak.load(Ordering::SeqCst);
            if inflight > observed {
                if self
                    .peak
                    .compare_exchange(observed, inflight, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            } else {
                break;
            }
        }
        if inflight == self.limit {
            self.started.notify_waiters();
        }
        let _permit = self.release.acquire().await.expect("semaphore closed");
        self.current.fetch_sub(1, Ordering::SeqCst);
        Ok(codecrafters_dns_server::workers::WorkerReport::completed(
            lease.lease_id,
        ))
    }
}

struct PanicOnceHandler {
    tripped: AtomicBool,
}

impl PanicOnceHandler {
    fn new() -> Self {
        Self {
            tripped: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl WorkerHandler for PanicOnceHandler {
    async fn handle(&self, lease: Lease) -> Result<codecrafters_dns_server::workers::WorkerReport> {
        if !self.tripped.swap(true, Ordering::SeqCst) {
            panic!("intentional panic for testing");
        }
        Ok(codecrafters_dns_server::workers::WorkerReport::completed(
            lease.lease_id,
        ))
    }
}
