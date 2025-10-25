use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::control_plane::{Observation, TaskSpec};
use crate::dispatcher::LeaseAssignment;

use super::{WorkerHandler, WorkerReport};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::IcmpSocket;

#[cfg(not(unix))]
mod unix {
    use anyhow::{anyhow, Result};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    use super::IcmpOutcome;

    #[derive(Clone, Debug, Default)]
    pub struct IcmpSocket;

    impl IcmpSocket {
        pub fn new(_address: &SocketAddr) -> Result<Self> {
            Ok(Self)
        }

        pub async fn send_echo(
            &self,
            _address: &SocketAddr,
            _identifier: u16,
            _sequence: u16,
            _payload: &[u8],
            _timeout: Duration,
            _cancel: CancellationToken,
        ) -> Result<IcmpOutcome> {
            Err(anyhow!("ICMP sockets not supported on this platform"))
        }
    }
}

#[cfg(not(unix))]
use unix::IcmpSocket;

const DEFAULT_PING_COUNT: u32 = 4;
const DEFAULT_TIMEOUT_MS: u64 = 1000;
const DEFAULT_RATE_PER_SECOND: u32 = 10;
const MAX_PING_COUNT: u32 = 20;
const MIN_INTERVAL: Duration = Duration::from_micros(1);

#[derive(Clone)]
pub struct PingWorker {
    engine: Arc<dyn PingEngine>,
    limiter: Arc<PingRateLimiter>,
}

impl PingWorker {
    pub(crate) fn new(engine: Arc<dyn PingEngine>, limiter: Arc<PingRateLimiter>) -> Self {
        Self { engine, limiter }
    }

    pub fn with_engine(engine: Arc<dyn PingEngine>) -> Self {
        Self::new(
            engine,
            Arc::new(PingRateLimiter::new(DEFAULT_RATE_PER_SECOND as usize)),
        )
    }
}

impl Default for PingWorker {
    fn default() -> Self {
        let engine: Arc<dyn PingEngine> = Arc::new(SystemPingEngine::default());
        Self::with_engine(engine)
    }
}

#[async_trait]
impl WorkerHandler for PingWorker {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "Ping lease cancelled before start");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }

        let spec = match lease.spec {
            TaskSpec::Ping(spec) => spec,
            other => {
                return Err(anyhow!("unexpected spec for Ping worker: {other:?}"));
            }
        };

        let count = spec
            .count
            .unwrap_or(DEFAULT_PING_COUNT)
            .clamp(1, MAX_PING_COUNT);
        let timeout = Duration::from_millis(spec.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let rate = spec
            .rate_limit_per_sec
            .unwrap_or(DEFAULT_RATE_PER_SECOND)
            .max(1) as usize;
        let interval = spec
            .interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| {
                let seconds = 1.0f64 / rate as f64;
                let candidate = Duration::from_secs_f64(seconds);
                if candidate.is_zero() {
                    MIN_INTERVAL
                } else {
                    candidate
                }
            });

        let limiter_guard = self.limiter.with_rate(rate);

        let addresses = match resolve_addresses(&spec.host).await {
            Ok(addresses) if !addresses.is_empty() => addresses,
            Ok(_) => {
                let report = WorkerReport::completed(lease.lease_id).with_observation(
                    lease.lease_id,
                    resolution_failure_observation(&spec.host, "no addresses resolved".into()),
                );
                return Ok(report);
            }
            Err(err) => {
                let report = WorkerReport::completed(lease.lease_id).with_observation(
                    lease.lease_id,
                    resolution_failure_observation(&spec.host, err.to_string()),
                );
                return Ok(report);
            }
        };

        let mut outcome = PingOutcome::new(count as usize);

        for index in 0..count {
            if token.is_cancelled() {
                outcome.mark_cancelled();
                break;
            }

            let permit = limiter_guard.acquire().await?;
            let address = addresses[(index as usize) % addresses.len()];
            let sequence = index as u16;

            let request = PingRequest {
                address,
                sequence,
                timeout,
            };

            let result = self.engine.probe(request, token.clone()).await;
            permit.release_with_delay(interval);

            let observation = PingObservation::from_outcome(index, result);
            outcome.record(observation);
        }

        let cancelled = outcome.is_cancelled();
        let observations = outcome.into_observations();

        if cancelled {
            Ok(WorkerReport::cancelled(lease.lease_id)
                .with_observations(lease.lease_id, observations))
        } else {
            Ok(WorkerReport::completed(lease.lease_id)
                .with_observations(lease.lease_id, observations))
        }
    }
}

#[derive(Debug, Clone)]
pub struct PingObservation {
    attempt: u32,
    address: Option<SocketAddr>,
    protocol: PingProtocol,
    outcome: PingProbeOutcome,
}

impl PingObservation {
    fn from_outcome(attempt: u32, outcome: PingProbeOutcome) -> Self {
        let address = outcome.address();
        let protocol = outcome.protocol();
        Self {
            attempt,
            address,
            protocol,
            outcome,
        }
    }

    fn is_success(&self) -> bool {
        matches!(
            self.outcome,
            PingProbeOutcome::Success { .. } | PingProbeOutcome::FallbackSuccess { .. }
        )
    }

    fn elapsed_ms(&self) -> Option<f64> {
        match &self.outcome {
            PingProbeOutcome::Success { rtt, .. } => Some(rtt.as_secs_f64() * 1000.0),
            PingProbeOutcome::FallbackSuccess { rtt, .. } => Some(rtt.as_secs_f64() * 1000.0),
            _ => None,
        }
    }

    fn reason(&self) -> Option<PingFailureReason> {
        match &self.outcome {
            PingProbeOutcome::Timeout { .. } => Some(PingFailureReason::Timeout),
            PingProbeOutcome::DestinationUnreachable { .. } => Some(PingFailureReason::Unreachable),
            PingProbeOutcome::Filtered { .. } => Some(PingFailureReason::Filtered),
            PingProbeOutcome::Error { .. } => Some(PingFailureReason::Error),
            PingProbeOutcome::Cancelled => Some(PingFailureReason::Cancelled),
            PingProbeOutcome::Success { .. } | PingProbeOutcome::FallbackSuccess { .. } => None,
        }
    }

    fn into_observation(self) -> Observation {
        let value = match self.outcome.clone() {
            PingProbeOutcome::Success {
                rtt, bytes, ttl, ..
            } => json!({
                "attempt": self.attempt,
                "success": true,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "rtt_ms": rtt.as_secs_f64() * 1000.0,
                "bytes": bytes,
                "ttl": ttl,
            }),
            PingProbeOutcome::FallbackSuccess { rtt, .. } => json!({
                "attempt": self.attempt,
                "success": true,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "rtt_ms": rtt.as_secs_f64() * 1000.0,
            }),
            PingProbeOutcome::Timeout { .. } => json!({
                "attempt": self.attempt,
                "success": false,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "reason": PingFailureReason::Timeout,
            }),
            PingProbeOutcome::DestinationUnreachable { code, message, .. } => json!({
                "attempt": self.attempt,
                "success": false,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "reason": PingFailureReason::Unreachable,
                "code": code,
                "message": message,
            }),
            PingProbeOutcome::Filtered { code, message, .. } => json!({
                "attempt": self.attempt,
                "success": false,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "reason": PingFailureReason::Filtered,
                "code": code,
                "message": message,
            }),
            PingProbeOutcome::Error { error, .. } => json!({
                "attempt": self.attempt,
                "success": false,
                "protocol": self.protocol,
                "address": self.address.map(|addr| addr.to_string()),
                "reason": PingFailureReason::Error,
                "message": error,
            }),
            PingProbeOutcome::Cancelled => json!({
                "attempt": self.attempt,
                "success": false,
                "protocol": self.protocol,
                "reason": PingFailureReason::Cancelled,
            }),
        };

        Observation {
            name: "ping_attempt".into(),
            value,
            unit: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PingProbeOutcome {
    Success {
        address: SocketAddr,
        rtt: Duration,
        bytes: usize,
        ttl: Option<u8>,
    },
    FallbackSuccess {
        address: SocketAddr,
        rtt: Duration,
    },
    Timeout {
        address: Option<SocketAddr>,
        protocol: PingProtocol,
    },
    DestinationUnreachable {
        address: Option<SocketAddr>,
        code: u8,
        message: String,
        protocol: PingProtocol,
    },
    Filtered {
        address: Option<SocketAddr>,
        code: u8,
        message: String,
        protocol: PingProtocol,
    },
    Error {
        address: Option<SocketAddr>,
        error: String,
        protocol: PingProtocol,
    },
    Cancelled,
}

impl PingProbeOutcome {
    fn address(&self) -> Option<SocketAddr> {
        match self {
            PingProbeOutcome::Success { address, .. } => Some(*address),
            PingProbeOutcome::FallbackSuccess { address, .. } => Some(*address),
            PingProbeOutcome::Timeout { address, .. }
            | PingProbeOutcome::DestinationUnreachable { address, .. }
            | PingProbeOutcome::Filtered { address, .. }
            | PingProbeOutcome::Error { address, .. } => *address,
            PingProbeOutcome::Cancelled => None,
        }
    }

    fn protocol(&self) -> PingProtocol {
        match self {
            PingProbeOutcome::Success { .. } => PingProtocol::Icmp,
            PingProbeOutcome::FallbackSuccess { .. } => PingProtocol::TcpSyn,
            PingProbeOutcome::Timeout { protocol, .. }
            | PingProbeOutcome::DestinationUnreachable { protocol, .. }
            | PingProbeOutcome::Filtered { protocol, .. }
            | PingProbeOutcome::Error { protocol, .. } => *protocol,
            PingProbeOutcome::Cancelled => PingProtocol::Icmp,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PingProtocol {
    Icmp,
    TcpSyn,
}

impl serde::Serialize for PingProtocol {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            PingProtocol::Icmp => "icmp",
            PingProtocol::TcpSyn => "tcp_syn",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub enum PingFailureReason {
    #[serde(rename = "timeout")]
    Timeout,
    #[serde(rename = "unreachable")]
    Unreachable,
    #[serde(rename = "filtered")]
    Filtered,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct PingOutcome {
    attempts: Vec<PingObservation>,
    cancelled: bool,
}

impl PingOutcome {
    fn new(capacity: usize) -> Self {
        Self {
            attempts: Vec::with_capacity(capacity),
            cancelled: false,
        }
    }

    fn record(&mut self, observation: PingObservation) {
        self.attempts.push(observation);
    }

    fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    fn into_observations(mut self) -> Vec<Observation> {
        let summary = self.summary_observation();
        let mut observations: Vec<Observation> = self
            .attempts
            .drain(..)
            .map(PingObservation::into_observation)
            .collect();
        observations.push(summary);
        observations
    }

    fn summary_observation(&self) -> Observation {
        let sent = self.attempts.len() as u32;
        let received = self.attempts.iter().filter(|obs| obs.is_success()).count() as u32;
        let loss = if sent == 0 {
            0.0
        } else {
            ((sent - received) as f64 / sent as f64) * 100.0
        };
        let mut rtts: Vec<f64> = self
            .attempts
            .iter()
            .filter_map(|obs| obs.elapsed_ms())
            .collect();
        let (min_rtt, max_rtt, avg_rtt, stddev_rtt) = if rtts.is_empty() {
            (None, None, None, None)
        } else {
            rtts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let min = Some(*rtts.first().unwrap());
            let max = Some(*rtts.last().unwrap());
            let sum: f64 = rtts.iter().sum();
            let avg = sum / rtts.len() as f64;
            let variance = rtts
                .iter()
                .map(|value| {
                    let diff = *value - avg;
                    diff * diff
                })
                .sum::<f64>()
                / rtts.len() as f64;
            let stddev = variance.sqrt();
            (min, max, Some(avg), Some(stddev))
        };

        let mut reasons: HashMap<PingFailureReason, u32> = HashMap::new();
        for obs in &self.attempts {
            if let Some(reason) = obs.reason() {
                *reasons.entry(reason).or_default() += 1;
            }
        }

        let protocol = self
            .attempts
            .iter()
            .find(|obs| obs.is_success())
            .or_else(|| self.attempts.first())
            .map(|obs| obs.protocol);

        Observation {
            name: "ping_summary".into(),
            value: json!({
                "success": received > 0,
                "sent": sent,
                "received": received,
                "loss_pct": loss,
                "min_rtt_ms": min_rtt,
                "max_rtt_ms": max_rtt,
                "avg_rtt_ms": avg_rtt,
                "stddev_rtt_ms": stddev_rtt,
                "protocol": protocol,
                "reasons": reasons,
                "cancelled": self.cancelled,
            }),
            unit: None,
        }
    }
}

#[derive(Clone)]
pub struct PingRequest {
    pub address: SocketAddr,
    pub sequence: u16,
    pub timeout: Duration,
}

#[async_trait]
pub trait PingEngine: Send + Sync {
    async fn probe(&self, request: PingRequest, cancel: CancellationToken) -> PingProbeOutcome;
}

#[derive(Clone, Default)]
pub struct SystemPingEngine {
    identifier: Arc<AtomicU16>,
}

#[async_trait]
impl PingEngine for SystemPingEngine {
    async fn probe(&self, request: PingRequest, cancel: CancellationToken) -> PingProbeOutcome {
        let identifier = self.identifier.fetch_add(1, Ordering::Relaxed);
        let socket = match IcmpSocket::new(&request.address) {
            Ok(socket) => socket,
            Err(err) => {
                warn!("failed to create ICMP socket: {err}");
                return tcp_syn_probe(request, cancel).await;
            }
        };

        let payload = build_payload();
        match socket
            .send_echo(
                &request.address,
                identifier,
                request.sequence,
                &payload,
                request.timeout,
                cancel.clone(),
            )
            .await
        {
            Ok(outcome) => match outcome {
                IcmpOutcome::Echo { rtt, bytes, ttl } => PingProbeOutcome::Success {
                    address: request.address,
                    rtt,
                    bytes,
                    ttl,
                },
                IcmpOutcome::Timeout => PingProbeOutcome::Timeout {
                    address: Some(request.address),
                    protocol: PingProtocol::Icmp,
                },
                IcmpOutcome::DestinationUnreachable { code, message } => {
                    PingProbeOutcome::DestinationUnreachable {
                        address: Some(request.address),
                        code,
                        message,
                        protocol: PingProtocol::Icmp,
                    }
                }
                IcmpOutcome::Filtered { code, message } => PingProbeOutcome::Filtered {
                    address: Some(request.address),
                    code,
                    message,
                    protocol: PingProtocol::Icmp,
                },
                IcmpOutcome::Cancelled => PingProbeOutcome::Cancelled,
            },
            Err(err) => {
                warn!("ICMP probe failed: {err}");
                tcp_syn_probe(request, cancel).await
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum IcmpOutcome {
    Echo {
        rtt: Duration,
        bytes: usize,
        ttl: Option<u8>,
    },
    Timeout,
    DestinationUnreachable {
        code: u8,
        message: String,
    },
    Filtered {
        code: u8,
        message: String,
    },
    Cancelled,
}

fn build_payload() -> Vec<u8> {
    (0u8..56u8).collect()
}

async fn tcp_syn_probe(request: PingRequest, cancel: CancellationToken) -> PingProbeOutcome {
    let start = Instant::now();
    let target = match request.address {
        SocketAddr::V4(addr) => SocketAddr::new(IpAddr::V4(*addr.ip()), 80),
        SocketAddr::V6(addr) => SocketAddr::new(IpAddr::V6(*addr.ip()), 80),
    };
    let connect = TcpStream::connect(target);
    let timeout = request.timeout;
    tokio::select! {
        _ = cancel.cancelled() => {
            PingProbeOutcome::Cancelled
        }
        result = time::timeout(timeout, connect) => {
            match result {
                Ok(Ok(stream)) => {
                    let elapsed = start.elapsed();
                    drop(stream);
                    PingProbeOutcome::FallbackSuccess {
                        address: target,
                        rtt: elapsed,
                    }
                }
                Ok(Err(err)) => {
                    PingProbeOutcome::Error {
                        address: Some(target),
                        error: err.to_string(),
                        protocol: PingProtocol::TcpSyn,
                    }
                }
                Err(_) => PingProbeOutcome::Timeout {
                    address: Some(target),
                    protocol: PingProtocol::TcpSyn,
                }
            }
        }
    }
}

pub(crate) struct PingRateLimiter {
    semaphore: Arc<Semaphore>,
}

impl PingRateLimiter {
    fn new(max_permits: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_permits.max(1))),
        }
    }

    fn with_rate(self: &Arc<Self>, _rate: usize) -> PingRateLimiterGuard {
        PingRateLimiterGuard {
            limiter: Arc::clone(self),
        }
    }
}

struct PingRateLimiterGuard {
    limiter: Arc<PingRateLimiter>,
}

impl PingRateLimiterGuard {
    async fn acquire(&self) -> Result<PingPermit> {
        let permit = self
            .limiter
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("rate limiter closed"))?;
        Ok(PingPermit {
            permit: Some(permit),
        })
    }
}

struct PingPermit {
    permit: Option<OwnedSemaphorePermit>,
}

impl PingPermit {
    fn release_with_delay(mut self, delay: Duration) {
        if let Some(permit) = self.permit.take() {
            tokio::spawn(async move {
                time::sleep(delay).await;
                drop(permit);
            });
        }
    }
}

impl Drop for PingPermit {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            drop(permit);
        }
    }
}

async fn resolve_addresses(host: &str) -> Result<Vec<SocketAddr>> {
    let entries = tokio::net::lookup_host((host, 0))
        .await
        .with_context(|| format!("failed to resolve host {host}"))?;
    Ok(entries.collect())
}

fn resolution_failure_observation(host: &str, error: String) -> Observation {
    Observation {
        name: "ping_resolution".into(),
        value: json!({
            "host": host,
            "error": error,
        }),
        unit: None,
    }
}
