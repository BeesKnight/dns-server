use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::control_plane::{Observation, TaskSpec};
use crate::dispatcher::LeaseAssignment;

use super::{WorkerHandler, WorkerReport};

static FLOW_SEED: Lazy<AtomicU16> = Lazy::new(|| {
    let seed = thread_rng().gen::<u16>();
    AtomicU16::new(seed)
});

const DEFAULT_MAX_HOPS: u8 = 30;
const DEFAULT_TIMEOUT_MS: u64 = 1500;
const DEFAULT_RATE_PER_SECOND: usize = 10;
const PROBES_PER_HOP: u8 = 3;
const MIN_INTERVAL: Duration = Duration::from_micros(1);

#[derive(Clone)]
pub struct TraceWorker {
    engine: Arc<dyn TraceEngine>,
    limiter: Arc<TraceRateLimiter>,
    capability: Arc<dyn RawSocketCapability>,
}

impl TraceWorker {
    pub(crate) fn new(
        engine: Arc<dyn TraceEngine>,
        limiter: Arc<TraceRateLimiter>,
        capability: Arc<dyn RawSocketCapability>,
    ) -> Self {
        Self {
            engine,
            limiter,
            capability,
        }
    }

    pub fn with_engine(engine: Arc<dyn TraceEngine>) -> Self {
        Self::new(
            engine,
            Arc::new(TraceRateLimiter::new(DEFAULT_RATE_PER_SECOND)),
            Arc::new(SystemRawSocketCapability::default()),
        )
    }

    pub fn with_engine_and_config(
        engine: Arc<dyn TraceEngine>,
        rate_per_second: usize,
        capability: Arc<dyn RawSocketCapability>,
    ) -> Self {
        Self::new(
            engine,
            Arc::new(TraceRateLimiter::new(rate_per_second)),
            capability,
        )
    }
}

impl Default for TraceWorker {
    fn default() -> Self {
        let engine: Arc<dyn TraceEngine> = Arc::new(SystemTraceEngine::default());
        Self::with_engine(engine)
    }
}

#[async_trait]
impl WorkerHandler for TraceWorker {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        if token.is_cancelled() {
            info!(lease_id = lease.lease_id, kind = ?lease.kind, "Trace lease cancelled before start");
            return Ok(WorkerReport::cancelled(lease.lease_id));
        }

        let spec = match lease.spec {
            TaskSpec::Trace(spec) => spec,
            other => return Err(anyhow!("unexpected spec for Trace worker: {other:?}")),
        };

        let max_hops = spec.max_hops.unwrap_or(DEFAULT_MAX_HOPS).max(1);
        let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS);

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

        let target = addresses[0];
        let paris_flow = ParisFlowId::next();

        let capability = self.capability.can_use_raw();

        let (protocols, permission_denied) = match capability {
            Ok(true) => (
                vec![TraceProtocol::Icmp, TraceProtocol::Udp, TraceProtocol::Tcp],
                false,
            ),
            Ok(false) => {
                warn!(
                    lease_id = lease.lease_id,
                    "raw socket permissions denied, falling back to TCP"
                );
                (vec![TraceProtocol::Tcp], true)
            }
            Err(err) => {
                warn!(error = %err, lease_id = lease.lease_id, "failed to determine raw socket capability");
                (vec![TraceProtocol::Tcp], false)
            }
        };

        let guard = self.limiter.guard();
        let mut hops = Vec::new();
        let mut protocols_used = Vec::new();
        let mut sequence = 0u16;
        let mut status = TraceStatus::Ok;
        let mut reached_destination = false;
        let start = Instant::now();

        for ttl in 1..=max_hops {
            if token.is_cancelled() {
                debug!(
                    lease_id = lease.lease_id,
                    hop = ttl,
                    "Trace cancelled mid-flight"
                );
                status = TraceStatus::Cancelled;
                break;
            }

            let mut hop_record = TraceHop::new(ttl);
            let mut hop_observed = false;

            for attempt in 0..PROBES_PER_HOP {
                let protocol = protocols[(attempt as usize) % protocols.len()];
                let permit = guard.acquire().await?;
                let request = TraceProbeRequest {
                    target,
                    ttl,
                    protocol,
                    sequence,
                    timeout,
                    paris_flow,
                };
                sequence = sequence.wrapping_add(1);
                protocols_used.push(protocol);
                let outcome = self.engine.probe(request, token.clone()).await;
                permit.release_after(guard.interval());

                match outcome {
                    TraceProbeOutcome::Hop {
                        address,
                        rtt,
                        reached,
                    } => {
                        hop_record.address = address.map(|addr| addr.to_string());
                        hop_record.rtt_ms = rtt.map(|dur| dur.as_secs_f64() * 1000.0);
                        hop_record.protocol = protocol;
                        hop_record.success = true;
                        hop_record.reached_destination = reached;
                        hop_observed = true;
                        if reached {
                            reached_destination = true;
                        }
                        break;
                    }
                    TraceProbeOutcome::Timeout => {
                        hop_record.protocol = protocol;
                        hop_record.success = false;
                        hop_observed = true;
                    }
                    TraceProbeOutcome::Error { error } => {
                        hop_record.protocol = protocol;
                        hop_record.success = false;
                        hop_record.error = Some(error);
                        hop_observed = true;
                        break;
                    }
                }
            }

            if hop_observed {
                hops.push(hop_record);
            }

            if reached_destination {
                break;
            }
        }

        let nat_detected = detect_nat(&hops);
        let loop_detected = detect_loop(&hops);
        let total_duration = start.elapsed();

        if permission_denied && status == TraceStatus::Ok {
            status = TraceStatus::PermissionDenied;
        }

        let observation = TraceObservation {
            host: spec.host.clone(),
            status,
            paris_flow,
            total_duration_ms: total_duration.as_secs_f64() * 1000.0,
            hops,
            nat_detected,
            loop_detected,
            permission_denied,
            protocols: protocols_used,
        };

        let report = match status {
            TraceStatus::Cancelled => WorkerReport::cancelled(lease.lease_id).with_observation(
                lease.lease_id,
                Observation {
                    name: "trace".into(),
                    value: json!(observation),
                    unit: None,
                },
            ),
            _ => WorkerReport::completed(lease.lease_id).with_observation(
                lease.lease_id,
                Observation {
                    name: "trace".into(),
                    value: json!(observation),
                    unit: None,
                },
            ),
        };

        Ok(report)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceObservation {
    pub host: String,
    pub status: TraceStatus,
    pub paris_flow: ParisFlowId,
    pub total_duration_ms: f64,
    pub hops: Vec<TraceHop>,
    pub nat_detected: bool,
    pub loop_detected: bool,
    pub permission_denied: bool,
    pub protocols: Vec<TraceProtocol>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceStatus {
    Ok,
    Cancelled,
    PermissionDenied,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParisFlowId(u16);

impl ParisFlowId {
    pub fn next() -> Self {
        let id = FLOW_SEED.fetch_add(1, Ordering::Relaxed);
        ParisFlowId(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceHop {
    pub ttl: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    pub protocol: TraceProtocol,
    pub success: bool,
    pub reached_destination: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TraceHop {
    fn new(ttl: u8) -> Self {
        Self {
            ttl,
            address: None,
            rtt_ms: None,
            protocol: TraceProtocol::Udp,
            success: false,
            reached_destination: false,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TraceProtocol {
    Tcp,
    Udp,
    Icmp,
}

#[derive(Debug, Clone)]
pub struct TraceProbeRequest {
    pub target: SocketAddr,
    pub ttl: u8,
    pub protocol: TraceProtocol,
    pub sequence: u16,
    pub timeout: Duration,
    pub paris_flow: ParisFlowId,
}

#[derive(Debug, Clone)]
pub enum TraceProbeOutcome {
    Hop {
        address: Option<SocketAddr>,
        rtt: Option<Duration>,
        reached: bool,
    },
    Timeout,
    Error {
        error: String,
    },
}

#[async_trait]
pub trait TraceEngine: Send + Sync {
    async fn probe(
        &self,
        request: TraceProbeRequest,
        cancel: CancellationToken,
    ) -> TraceProbeOutcome;
}

#[derive(Default, Debug)]
struct SystemTraceEngine;

#[async_trait]
impl TraceEngine for SystemTraceEngine {
    async fn probe(
        &self,
        request: TraceProbeRequest,
        cancel: CancellationToken,
    ) -> TraceProbeOutcome {
        if cancel.is_cancelled() {
            return TraceProbeOutcome::Error {
                error: "cancelled".into(),
            };
        }
        debug!(
            target = %request.target,
            ttl = request.ttl,
            protocol = ?request.protocol,
            "system trace engine probe invoked"
        );
        TraceProbeOutcome::Timeout
    }
}

pub trait RawSocketCapability: Send + Sync {
    fn can_use_raw(&self) -> std::io::Result<bool>;
}

#[derive(Default)]
struct SystemRawSocketCapability;

impl RawSocketCapability for SystemRawSocketCapability {
    fn can_use_raw(&self) -> std::io::Result<bool> {
        check_raw_socket_capability()
    }
}

fn detect_nat(hops: &[TraceHop]) -> bool {
    let mut seen = HashSet::new();
    for hop in hops {
        if let Some(address) = &hop.address {
            if !seen.insert(address.clone()) {
                return true;
            }
        }
    }
    false
}

fn detect_loop(hops: &[TraceHop]) -> bool {
    for window in hops.windows(2) {
        if let [left, right] = window {
            if left.address.is_some()
                && left.address == right.address
                && left.success
                && right.success
            {
                return true;
            }
        }
    }
    false
}

#[derive(Clone)]
pub struct TraceRateLimiter {
    semaphore: Arc<Semaphore>,
    interval: Duration,
}

impl TraceRateLimiter {
    fn new(rate_per_second: usize) -> Self {
        let rate = rate_per_second.max(1);
        let interval = if rate >= 1 {
            let seconds = 1.0f64 / rate as f64;
            let candidate = Duration::from_secs_f64(seconds);
            if candidate.is_zero() {
                MIN_INTERVAL
            } else {
                candidate
            }
        } else {
            MIN_INTERVAL
        };
        Self {
            semaphore: Arc::new(Semaphore::new(rate)),
            interval,
        }
    }

    fn guard(self: &Arc<Self>) -> TraceRateGuard {
        TraceRateGuard {
            limiter: Arc::clone(self),
        }
    }
}

pub struct TraceRateGuard {
    limiter: Arc<TraceRateLimiter>,
}

impl TraceRateGuard {
    async fn acquire(&self) -> Result<TracePermit> {
        let permit = self
            .limiter
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("trace rate limiter closed"))?;
        Ok(TracePermit {
            permit: Some(permit),
        })
    }

    fn interval(&self) -> Duration {
        self.limiter.interval
    }
}

pub struct TracePermit {
    permit: Option<OwnedSemaphorePermit>,
}

impl TracePermit {
    fn release_after(mut self, delay: Duration) {
        if let Some(permit) = self.permit.take() {
            tokio::spawn(async move {
                time::sleep(delay).await;
                drop(permit);
            });
        }
    }
}

impl Drop for TracePermit {
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
        name: "trace_resolution".into(),
        value: json!({
            "host": host,
            "error": error,
        }),
        unit: None,
    }
}

fn check_raw_socket_capability() -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use socket2::{Domain, Protocol, Socket, Type};
        match Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4)) {
            Ok(_) => Ok(true),
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

#[allow(dead_code)]
pub fn build_icmp_probe(flow: ParisFlowId, sequence: u16) -> Vec<u8> {
    use pnet_packet::icmp::{echo_request::MutableEchoRequestPacket, IcmpPacket, IcmpTypes};
    use pnet_packet::Packet;

    let mut buffer = vec![0u8; 8];
    {
        let mut packet = MutableEchoRequestPacket::new(&mut buffer).expect("packet buffer");
        packet.set_icmp_type(IcmpTypes::EchoRequest);
        packet.set_identifier(flow.0);
        packet.set_sequence_number(sequence);
        packet.set_checksum(0);
        let checksum = {
            let icmp = IcmpPacket::new(packet.packet()).expect("icmp packet");
            pnet_packet::icmp::checksum(&icmp)
        };
        packet.set_checksum(checksum);
    }
    buffer
}

#[allow(dead_code)]
pub fn build_udp_probe(flow: ParisFlowId) -> (Vec<u8>, u16) {
    use pnet_packet::udp::MutableUdpPacket;

    let mut buffer = vec![0u8; 8];
    let source = 33434 + (flow.0 % 1000) as u16;
    let destination = 33434;
    {
        let mut packet = MutableUdpPacket::new(&mut buffer).expect("packet buffer");
        packet.set_source(source);
        packet.set_destination(destination);
        packet.set_length(8);
    }
    (buffer, source)
}

#[allow(dead_code)]
pub fn build_tcp_probe(flow: ParisFlowId) -> (Vec<u8>, u16) {
    use pnet_packet::tcp::{MutableTcpPacket, TcpFlags};

    let mut buffer = vec![0u8; 20];
    let source = 33434 + (flow.0 % 1000) as u16;
    let destination = 80;
    {
        let mut packet = MutableTcpPacket::new(&mut buffer).expect("packet buffer");
        packet.set_source(source);
        packet.set_destination(destination);
        packet.set_flags(TcpFlags::SYN);
        packet.set_window(64240);
    }
    (buffer, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paris_flow_increments() {
        let first = ParisFlowId::next();
        let second = ParisFlowId::next();
        assert_ne!(first.0, second.0);
    }

    #[test]
    fn icmp_probe_has_identifier_and_sequence() {
        let flow = ParisFlowId(42);
        let packet = build_icmp_probe(flow, 7);
        let parsed =
            pnet_packet::icmp::echo_request::EchoRequestPacket::new(&packet).expect("packet");
        assert_eq!(parsed.get_identifier(), 42);
        assert_eq!(parsed.get_sequence_number(), 7);
    }

    #[test]
    fn udp_probe_sets_ports() {
        let flow = ParisFlowId(100);
        let (packet, source) = build_udp_probe(flow);
        let parsed = pnet_packet::udp::UdpPacket::new(&packet).expect("packet");
        assert_eq!(parsed.get_source(), source);
        assert_eq!(parsed.get_destination(), 33434);
        assert_eq!(parsed.get_length(), 8);
    }

    #[test]
    fn tcp_probe_sets_syn_flag() {
        let flow = ParisFlowId(5);
        let (packet, source) = build_tcp_probe(flow);
        let parsed = pnet_packet::tcp::TcpPacket::new(&packet).expect("packet");
        assert_eq!(parsed.get_source(), source);
        assert_eq!(parsed.get_destination(), 80);
        assert!(parsed.get_flags() & pnet_packet::tcp::TcpFlags::SYN != 0);
    }
}
