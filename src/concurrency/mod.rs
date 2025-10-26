use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::control_plane::TaskKind;

mod persistence;

pub use persistence::{ConcurrencyPersistence, ConcurrencyStateStore};

type TargetId = u64;

/// Configuration describing the static ceilings for concurrency control.
#[derive(Clone, Debug)]
pub struct ConcurrencyLimits {
    pub per_kind: HashMap<TaskKind, usize>,
    pub global: usize,
}

impl ConcurrencyLimits {
    pub fn new(per_kind: HashMap<TaskKind, usize>, global: usize) -> Self {
        Self { per_kind, global }
    }

    pub fn per_kind(&self, kind: TaskKind) -> usize {
        self.per_kind.get(&kind).copied().unwrap_or(1).max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Timeout,
    Error,
}

/// Controller implementing an AIMD (additive increase, multiplicative decrease)
/// policy for per-kind and per-target concurrency windows.
#[derive(Clone)]
pub struct ConcurrencyController {
    inner: Arc<Mutex<ControllerInner>>,
    notify: Arc<Notify>,
    additive_step: f64,
    decrease_factor: f64,
    persistence: Option<ConcurrencyPersistence>,
}

impl ConcurrencyController {
    pub fn new(limits: ConcurrencyLimits) -> Self {
        Self::with_optional_persistence(limits, None)
    }

    pub fn with_persistence(
        limits: ConcurrencyLimits,
        persistence: ConcurrencyPersistence,
    ) -> Self {
        Self::with_optional_persistence(limits, Some(persistence))
    }

    fn with_optional_persistence(
        limits: ConcurrencyLimits,
        persistence: Option<ConcurrencyPersistence>,
    ) -> Self {
        let inner = ControllerInner::new(limits);
        Self {
            inner: Arc::new(Mutex::new(inner)),
            notify: Arc::new(Notify::new()),
            additive_step: 1.0,
            decrease_factor: 0.5,
            persistence,
        }
    }

    pub async fn acquire(&self, kind: TaskKind, target: TargetId) -> ConcurrencyPermit {
        loop {
            let acquired = {
                let mut inner = self.inner.lock().expect("controller mutex poisoned");
                inner.try_acquire(kind, target)
            };
            if let Some(snapshot) = acquired {
                if let Some(persistence) = &self.persistence {
                    persistence.record_global(snapshot.global.limit, snapshot.global.inflight);
                    persistence.record_kind(kind, snapshot.kind.limit, snapshot.kind.inflight);
                    persistence.record_target(
                        kind,
                        target,
                        snapshot.target.limit,
                        snapshot.target.inflight,
                    );
                }
                return ConcurrencyPermit {
                    controller: self.clone(),
                    kind,
                    target,
                    released: false,
                };
            }
            self.notify.notified().await;
        }
    }

    pub fn available(&self, kind: TaskKind) -> usize {
        let inner = self.inner.lock().expect("controller mutex poisoned");
        inner.available(kind)
    }

    pub fn record_success(&self, kind: TaskKind, target: TargetId) {
        self.record(kind, target, Outcome::Success);
    }

    pub fn record_timeout(&self, kind: TaskKind, target: TargetId) {
        self.record(kind, target, Outcome::Timeout);
    }

    pub fn record_error(&self, kind: TaskKind, target: TargetId) {
        self.record(kind, target, Outcome::Error);
    }

    fn record(&self, kind: TaskKind, target: TargetId, outcome: Outcome) {
        let mut inner = self.inner.lock().expect("controller mutex poisoned");
        let adjustments = inner.apply_outcome(
            kind,
            target,
            outcome,
            self.additive_step,
            self.decrease_factor,
        );
        drop(inner);
        if adjustments.notified {
            self.notify.notify_waiters();
        }
        if let Some(persistence) = &self.persistence {
            if let Some(global) = adjustments.global {
                persistence.record_global(global.limit, global.inflight);
            }
            if let Some(kind_snapshot) = adjustments.kind {
                persistence.record_kind(kind, kind_snapshot.limit, kind_snapshot.inflight);
            }
            if let Some(target_snapshot) = adjustments.target {
                persistence.record_target(
                    kind,
                    target,
                    target_snapshot.limit,
                    target_snapshot.inflight,
                );
            }
        }
    }

    fn release(&self, kind: TaskKind, target: TargetId) {
        let mut inner = self.inner.lock().expect("controller mutex poisoned");
        let result = inner.release(kind, target);
        if result.notified {
            self.notify.notify_one();
        }
        drop(inner);
        if let Some(persistence) = &self.persistence {
            if let Some(global) = result.global {
                persistence.record_global(global.limit, global.inflight);
            }
            if let Some(kind_snapshot) = result.kind {
                persistence.record_kind(kind, kind_snapshot.limit, kind_snapshot.inflight);
            }
            if let Some(target_snapshot) = result.target {
                persistence.record_target(
                    kind,
                    target,
                    target_snapshot.limit,
                    target_snapshot.inflight,
                );
            }
        }
    }
}

pub struct ConcurrencyPermit {
    controller: ConcurrencyController,
    kind: TaskKind,
    target: TargetId,
    released: bool,
}

impl ConcurrencyPermit {
    pub fn kind(&self) -> TaskKind {
        self.kind
    }

    pub fn target(&self) -> TargetId {
        self.target
    }
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        if !self.released {
            self.controller.release(self.kind, self.target);
            self.released = true;
        }
    }
}

struct ControllerInner {
    limits: ConcurrencyLimits,
    global: WindowState,
    per_kind: HashMap<TaskKind, KindState>,
    per_target: HashMap<(TaskKind, TargetId), TargetState>,
}

impl ControllerInner {
    fn new(limits: ConcurrencyLimits) -> Self {
        let global = WindowState::new(limits.global.max(1));
        Self {
            limits,
            global,
            per_kind: HashMap::new(),
            per_target: HashMap::new(),
        }
    }

    fn try_acquire(&mut self, kind: TaskKind, target: TargetId) -> Option<AcquireSnapshot> {
        let global_limit = self.global.limit();
        if self.global.inflight >= global_limit {
            metrics::gauge!("concurrency.backpressure", "scope" => "global").set(1.0);
            return None;
        }

        let kind_state = self
            .per_kind
            .entry(kind)
            .or_insert_with(|| KindState::new(self.limits.per_kind(kind)));
        if kind_state.inflight >= kind_state.window.limit() {
            metrics::gauge!("concurrency.backpressure", "scope" => "kind", "kind" => kind_label(kind))
                .set(1.0);
            return None;
        }

        let key = (kind, target);
        let target_state = self
            .per_target
            .entry(key)
            .or_insert_with(|| TargetState::new(self.limits.per_kind(kind)));
        if target_state.inflight >= target_state.window.limit() {
            metrics::gauge!("concurrency.backpressure", "scope" => "target", "kind" => kind_label(kind))
                .set(1.0);
            return None;
        }

        self.global.inflight += 1;
        kind_state.inflight += 1;
        target_state.inflight += 1;

        metrics::gauge!("concurrency.inflight", "scope" => "global")
            .set(self.global.inflight as f64);
        metrics::gauge!("concurrency.inflight", "scope" => "kind", "kind" => kind_label(kind))
            .set(kind_state.inflight as f64);
        metrics::gauge!("concurrency.inflight", "scope" => "target", "kind" => kind_label(kind))
            .set(target_state.inflight as f64);

        Some(AcquireSnapshot {
            global: WindowSnapshot {
                limit: self.global.limit(),
                inflight: self.global.inflight,
            },
            kind: WindowSnapshot {
                limit: kind_state.window.limit(),
                inflight: kind_state.inflight,
            },
            target: WindowSnapshot {
                limit: target_state.window.limit(),
                inflight: target_state.inflight,
            },
        })
    }

    fn available(&self, kind: TaskKind) -> usize {
        let limit = self
            .per_kind
            .get(&kind)
            .map(|state| state.window.limit())
            .unwrap_or_else(|| self.limits.per_kind(kind));
        let inflight = self
            .per_kind
            .get(&kind)
            .map(|state| state.inflight)
            .unwrap_or(0);
        limit.saturating_sub(inflight)
    }

    fn release(&mut self, kind: TaskKind, target: TargetId) -> ReleaseResult {
        let key = (kind, target);
        let mut target_snapshot = None;
        let mut kind_snapshot = None;
        if let Some(target_state) = self.per_target.get_mut(&key) {
            if target_state.inflight > 0 {
                target_state.inflight -= 1;
            }
            if target_state.inflight == 0 {
                metrics::gauge!("concurrency.backpressure", "scope" => "target", "kind" => kind_label(kind))
                    .set(0.0);
            }
            target_snapshot = Some(WindowSnapshot {
                limit: target_state.window.limit(),
                inflight: target_state.inflight,
            });
        }

        if let Some(kind_state) = self.per_kind.get_mut(&kind) {
            if kind_state.inflight > 0 {
                kind_state.inflight -= 1;
            }
            if kind_state.inflight < kind_state.window.limit() {
                metrics::gauge!("concurrency.backpressure", "scope" => "kind", "kind" => kind_label(kind))
                    .set(0.0);
            }
            kind_snapshot = Some(WindowSnapshot {
                limit: kind_state.window.limit(),
                inflight: kind_state.inflight,
            });
        }

        if self.global.inflight > 0 {
            self.global.inflight -= 1;
        }
        if self.global.inflight < self.global.limit() {
            metrics::gauge!("concurrency.backpressure", "scope" => "global").set(0.0);
        }
        metrics::gauge!("concurrency.inflight", "scope" => "global")
            .set(self.global.inflight as f64);

        ReleaseResult {
            notified: true,
            global: Some(WindowSnapshot {
                limit: self.global.limit(),
                inflight: self.global.inflight,
            }),
            kind: kind_snapshot,
            target: target_snapshot,
        }
    }

    fn apply_outcome(
        &mut self,
        kind: TaskKind,
        target: TargetId,
        outcome: Outcome,
        additive_step: f64,
        decrease_factor: f64,
    ) -> AdjustmentResult {
        let kind_state = self
            .per_kind
            .entry(kind)
            .or_insert_with(|| KindState::new(self.limits.per_kind(kind)));
        let key = (kind, target);
        let target_state = self
            .per_target
            .entry(key)
            .or_insert_with(|| TargetState::new(self.limits.per_kind(kind)));

        let mut notified = false;

        match outcome {
            Outcome::Success => {
                let delta_kind = kind_state.window.increase(additive_step);
                let delta_target = target_state.window.increase(additive_step);
                kind_state.stats.successes += 1;
                target_state.stats.successes += 1;
                metrics::gauge!("concurrency.window", "scope" => "kind", "kind" => kind_label(kind))
                    .set(kind_state.window.limit() as f64);
                metrics::gauge!("concurrency.window", "scope" => "target", "kind" => kind_label(kind))
                    .set(target_state.window.limit() as f64);
                metrics::gauge!("concurrency.adjustment", "scope" => "kind", "kind" => kind_label(kind))
                    .set(delta_kind);
                metrics::gauge!("concurrency.adjustment", "scope" => "target", "kind" => kind_label(kind))
                    .set(delta_target);
                if delta_kind > 0.0 || delta_target > 0.0 {
                    notified = true;
                }
            }
            Outcome::Timeout => {
                let delta_kind = kind_state.window.decrease(decrease_factor);
                let delta_target = target_state.window.decrease(decrease_factor);
                kind_state.stats.timeouts += 1;
                target_state.stats.timeouts += 1;
                metrics::gauge!("concurrency.window", "scope" => "kind", "kind" => kind_label(kind))
                    .set(kind_state.window.limit() as f64);
                metrics::gauge!("concurrency.window", "scope" => "target", "kind" => kind_label(kind))
                    .set(target_state.window.limit() as f64);
                metrics::gauge!("concurrency.adjustment", "scope" => "kind", "kind" => kind_label(kind))
                    .set(-delta_kind.abs());
                metrics::gauge!("concurrency.adjustment", "scope" => "target", "kind" => kind_label(kind))
                    .set(-delta_target.abs());
                notified = true;
            }
            Outcome::Error => {
                let delta_kind = kind_state.window.decrease(decrease_factor);
                let delta_target = target_state.window.decrease(decrease_factor);
                kind_state.stats.errors += 1;
                target_state.stats.errors += 1;
                metrics::gauge!("concurrency.window", "scope" => "kind", "kind" => kind_label(kind))
                    .set(kind_state.window.limit() as f64);
                metrics::gauge!("concurrency.window", "scope" => "target", "kind" => kind_label(kind))
                    .set(target_state.window.limit() as f64);
                metrics::gauge!("concurrency.adjustment", "scope" => "kind", "kind" => kind_label(kind))
                    .set(-delta_kind.abs());
                metrics::gauge!("concurrency.adjustment", "scope" => "target", "kind" => kind_label(kind))
                    .set(-delta_target.abs());
                notified = true;
            }
        }

        let global_snapshot = WindowSnapshot {
            limit: self.global.limit(),
            inflight: self.global.inflight,
        };
        let kind_snapshot = WindowSnapshot {
            limit: kind_state.window.limit(),
            inflight: kind_state.inflight,
        };
        let target_snapshot = WindowSnapshot {
            limit: target_state.window.limit(),
            inflight: target_state.inflight,
        };

        AdjustmentResult {
            notified,
            global: Some(global_snapshot),
            kind: Some(kind_snapshot),
            target: Some(target_snapshot),
        }
    }
}

struct AdjustmentResult {
    notified: bool,
    global: Option<WindowSnapshot>,
    kind: Option<WindowSnapshot>,
    target: Option<WindowSnapshot>,
}

#[derive(Clone, Copy)]
struct WindowSnapshot {
    limit: usize,
    inflight: usize,
}

struct AcquireSnapshot {
    global: WindowSnapshot,
    kind: WindowSnapshot,
    target: WindowSnapshot,
}

struct ReleaseResult {
    notified: bool,
    global: Option<WindowSnapshot>,
    kind: Option<WindowSnapshot>,
    target: Option<WindowSnapshot>,
}

struct WindowState {
    current: f64,
    max: f64,
    inflight: usize,
}

impl WindowState {
    fn new(max: usize) -> Self {
        let max = max.max(1) as f64;
        Self {
            current: max,
            max,
            inflight: 0,
        }
    }

    fn limit(&self) -> usize {
        self.current.floor().max(1.0) as usize
    }

    fn increase(&mut self, step: f64) -> f64 {
        let before = self.limit() as f64;
        self.current = (self.current + step).min(self.max);
        self.current - before
    }

    fn decrease(&mut self, factor: f64) -> f64 {
        let before = self.limit() as f64;
        self.current = (self.current * factor).max(1.0);
        self.current - before
    }
}

struct KindState {
    window: WindowState,
    inflight: usize,
    stats: OutcomeStats,
}

impl KindState {
    fn new(max: usize) -> Self {
        Self {
            window: WindowState::new(max),
            inflight: 0,
            stats: OutcomeStats::default(),
        }
    }
}

struct TargetState {
    window: WindowState,
    inflight: usize,
    stats: OutcomeStats,
}

impl TargetState {
    fn new(max: usize) -> Self {
        Self {
            window: WindowState::new(max),
            inflight: 0,
            stats: OutcomeStats::default(),
        }
    }
}

#[derive(Default)]
struct OutcomeStats {
    successes: u64,
    timeouts: u64,
    errors: u64,
}

pub(super) fn kind_label(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Dns => "dns",
        TaskKind::Http => "http",
        TaskKind::Tcp => "tcp",
        TaskKind::Ping => "ping",
        TaskKind::Trace => "trace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::time::{timeout, Duration};

    fn controller_for(kind_limit: usize, global: usize) -> ConcurrencyController {
        let mut map = HashMap::new();
        map.insert(TaskKind::Dns, kind_limit);
        let limits = ConcurrencyLimits::new(map, global);
        ConcurrencyController::new(limits)
    }

    #[tokio::test]
    async fn acquires_respect_limits() {
        let controller = controller_for(2, 2);
        let first = controller.acquire(TaskKind::Dns, 1).await;
        let second = controller.acquire(TaskKind::Dns, 2).await;

        let blocked = timeout(
            Duration::from_millis(50),
            controller.acquire(TaskKind::Dns, 3),
        )
        .await;
        assert!(blocked.is_err(), "third acquisition should block");

        drop(first);
        let third = timeout(
            Duration::from_millis(200),
            controller.acquire(TaskKind::Dns, 3),
        )
        .await
        .expect("third permit available");
        drop(second);
        drop(third);
    }

    #[tokio::test]
    async fn successes_increase_window_until_ceiling() {
        let controller = controller_for(4, 4);
        controller.record_success(TaskKind::Dns, 1);
        controller.record_success(TaskKind::Dns, 1);
        controller.record_success(TaskKind::Dns, 1);
        let available = controller.available(TaskKind::Dns);
        assert!(available >= 4, "window should grow towards ceiling");
    }

    #[tokio::test]
    async fn failures_reduce_window() {
        let controller = controller_for(4, 4);
        controller.record_error(TaskKind::Dns, 1);
        controller.record_error(TaskKind::Dns, 1);
        let available = controller.available(TaskKind::Dns);
        assert!(available <= 3, "window should shrink after errors");
    }
}
