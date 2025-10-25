use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::pending;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tracing::warn;

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const QUEUE_WARN_DEPTH: usize = 1024;
const LAG_WARN_THRESHOLD: Duration = Duration::from_millis(200);

/// Error returned when scheduling work on a stopped timer service.
#[derive(Debug, Error)]
pub enum TimerError {
    #[error("timer service has been shut down")]
    Closed,
}

enum Command {
    Schedule(ScheduledTimer),
    Shutdown,
}

/// Handle used to interact with the timer background worker.
#[derive(Clone)]
pub struct TimerService {
    inner: Arc<TimerInner>,
}

#[derive(Debug)]
struct TimerInner {
    tx: mpsc::UnboundedSender<Command>,
}

static NEXT_TIMER_ID: AtomicU64 = AtomicU64::new(1);

/// Representation of a scheduled timer.
pub struct ScheduledTimer {
    when: Instant,
    id: u64,
    task: BoxFuture,
}

impl ScheduledTimer {
    fn new(when: Instant, task: BoxFuture) -> Self {
        let id = NEXT_TIMER_ID.fetch_add(1, AtomicOrdering::Relaxed);
        Self { when, id, task }
    }

    fn spawn(self) {
        tokio::spawn(self.task);
    }
}

impl Eq for ScheduledTimer {}

impl PartialEq for ScheduledTimer {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Ord for ScheduledTimer {
    fn cmp(&self, other: &Self) -> Ordering {
        match other.when.cmp(&self.when) {
            Ordering::Equal => other.id.cmp(&self.id),
            order => order,
        }
    }
}

impl PartialOrd for ScheduledTimer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl TimerService {
    /// Creates a new timer service and starts its background worker.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(TimerInner { tx });
        run_timer_worker(rx);
        Self { inner }
    }

    /// Schedules an asynchronous task to run at the specified instant.
    pub fn schedule_at<F>(&self, when: Instant, task: F) -> Result<(), TimerError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let timer = ScheduledTimer::new(when, Box::pin(task));
        self.inner
            .tx
            .send(Command::Schedule(timer))
            .map_err(|_| TimerError::Closed)
    }

    /// Schedules an asynchronous task to run after the provided delay.
    pub fn schedule<F>(&self, delay: Duration, task: F) -> Result<(), TimerError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.schedule_at(Instant::now() + delay, task)
    }

    /// Returns a future that resolves after the specified delay elapses.
    pub async fn sleep(&self, delay: Duration) -> Result<(), TimerError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.schedule(delay, async move {
            let _ = tx.send(());
        })?;
        rx.await.map_err(|_| TimerError::Closed)
    }
}

impl Drop for TimerInner {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

fn run_timer_worker(mut rx: mpsc::UnboundedReceiver<Command>) {
    tokio::spawn(async move {
        let mut queue: BinaryHeap<ScheduledTimer> = BinaryHeap::new();
        loop {
            let next_deadline = queue.peek().map(|timer| timer.when);
            let mut sleep = next_deadline.map(|deadline| Box::pin(time::sleep_until(deadline)));

            tokio::select! {
                _ = async {
                    if let Some(sleep) = sleep.as_mut() {
                        sleep.await;
                    } else {
                        pending::<()>().await;
                    }
                } => {
                    if sleep.is_some() {
                        fire_due(&mut queue).await;
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Command::Schedule(timer)) => {
                            queue.push(timer);
                            update_depth_metric(queue.len());
                            maybe_warn_depth(queue.len());
                        }
                        Some(Command::Shutdown) | None => break,
                    }
                }
            }
        }

        // Drain any remaining timers to avoid leaving dangling tasks and reset metrics.
        while let Some(timer) = queue.pop() {
            timer.spawn();
        }
        update_depth_metric(0);
    });
}

async fn fire_due(queue: &mut BinaryHeap<ScheduledTimer>) {
    let now = Instant::now();
    while let Some(timer) = queue.peek() {
        if timer.when > now {
            break;
        }
        if let Some(timer) = queue.pop() {
            record_fire_metrics(now, timer.when);
            timer.spawn();
        }
    }
    update_depth_metric(queue.len());
}

fn record_fire_metrics(now: Instant, scheduled: Instant) {
    let lag = now.saturating_duration_since(scheduled);
    metrics::histogram!("timer.fire_lag_seconds").record(lag.as_secs_f64());
    if lag > LAG_WARN_THRESHOLD {
        warn!(
            lag_secs = lag.as_secs_f64(),
            "timer execution lag exceeds threshold"
        );
    }
}

fn maybe_warn_depth(depth: usize) {
    if depth > QUEUE_WARN_DEPTH {
        warn!(depth, "timer queue depth exceeded warning threshold");
    }
}

fn update_depth_metric(depth: usize) {
    metrics::gauge!("timer.queue_depth").set(depth as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    #[tokio::test]
    async fn timer_fires_with_virtual_time() {
        tokio::time::pause();
        let timer = TimerService::new();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone2 = fired.clone();
        tokio::spawn(async move {
            timer
                .sleep(Duration::from_millis(40))
                .await
                .expect("timer should complete under virtual time");
            fired_clone2.store(true, AtomicOrdering::SeqCst);
        });
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_millis(30)).await;
        tokio::task::yield_now().await;
        assert!(!fired.load(AtomicOrdering::SeqCst));

        tokio::time::advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert!(fired.load(AtomicOrdering::SeqCst));
    }
}
