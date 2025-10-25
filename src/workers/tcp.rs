use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde_json::json;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpSocket;
use tokio::task;
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::control_plane::{Observation, TaskSpec, TcpSpec};
use crate::dispatcher::LeaseAssignment;
use crate::runtime::SocketConfigurator;

use super::{WorkerHandler, WorkerReport};

const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);

/// Worker implementation responsible for executing TCP checks.
#[derive(Clone)]
pub struct TcpWorker {
    configurator: SocketConfigurator,
}

impl TcpWorker {
    pub fn new(configurator: SocketConfigurator) -> Self {
        Self { configurator }
    }
}

impl Default for TcpWorker {
    fn default() -> Self {
        Self::new(SocketConfigurator::default())
    }
}

#[async_trait]
impl WorkerHandler for TcpWorker {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        if token.is_cancelled() {
            debug!(lease_id = lease.lease_id, kind = ?lease.kind, "TCP lease cancelled before start");
            return Ok(WorkerReport::cancelled(lease.lease_id)
                .with_observation(lease.lease_id, TcpObservation::cancellation_summary(None)));
        }

        let spec = match &lease.spec {
            TaskSpec::Tcp(spec) => spec.clone(),
            other => return Err(anyhow!("received non TCP spec for TCP worker: {:?}", other)),
        };

        debug!(lease_id = lease.lease_id, host = %spec.host, port = spec.port, "resolving TCP target");
        let addresses = match resolve_targets(&spec).await {
            Ok(addresses) => addresses,
            Err(err) => {
                warn!(
                    lease_id = lease.lease_id,
                    host = %spec.host,
                    port = spec.port,
                    error = %err,
                    "failed to resolve TCP target"
                );
                let observation = tcp_resolution_failure_observation(&spec, err.to_string());
                return Ok(WorkerReport::completed(lease.lease_id)
                    .with_observation(lease.lease_id, observation));
            }
        };

        if addresses.is_empty() {
            warn!(
                lease_id = lease.lease_id,
                host = %spec.host,
                port = spec.port,
                "resolver returned no addresses"
            );
            let observation = tcp_resolution_failure_observation(
                &spec,
                "no socket addresses resolved".to_string(),
            );
            return Ok(WorkerReport::completed(lease.lease_id)
                .with_observation(lease.lease_id, observation));
        }

        let outcome =
            connect_with_happy_eyeballs(addresses, token.clone(), self.configurator.clone()).await;
        let TcpOutcome {
            observations,
            success,
            last_failure,
            last_attempt,
            mut error,
            cancelled: outcome_cancelled,
        } = outcome;

        if token.is_cancelled() {
            // Ensure errors reflect cancellation when the caller requested it.
            if error.is_none() {
                error = Some(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
        }

        if let Some(ref success_obs) = success {
            debug!(
                lease_id = lease.lease_id,
                address = %success_obs.address,
                rtt_ms = success_obs.elapsed_ms(),
                "TCP connect succeeded"
            );
        } else if let Some(ref err) = error {
            warn!(
                lease_id = lease.lease_id,
                host = %spec.host,
                port = spec.port,
                error = %err,
                "TCP connect failed"
            );
        }

        let mut report = if outcome_cancelled || token.is_cancelled() {
            WorkerReport::cancelled(lease.lease_id)
        } else {
            WorkerReport::completed(lease.lease_id)
        };

        report = report.with_observations(
            lease.lease_id,
            observations
                .into_iter()
                .map(TcpObservation::into_observation),
        );

        if let Some(success_obs) = success {
            report = report.with_observation(lease.lease_id, success_obs.into_summary());
        } else if outcome_cancelled || token.is_cancelled() {
            report = report.with_observation(
                lease.lease_id,
                TcpObservation::cancellation_summary(last_attempt),
            );
        } else if let Some(err) = error {
            let context = last_failure.or_else(|| last_attempt.clone());
            report = report.with_observation(
                lease.lease_id,
                TcpObservation::failure_summary(err, context),
            );
        }

        Ok(report)
    }
}

async fn resolve_targets(spec: &TcpSpec) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = spec.host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, spec.port)]);
    }

    let host_port = format!("{}:{}", spec.host, spec.port);
    let async_lookup = tokio::net::lookup_host(host_port.clone());
    let blocking_lookup = task::spawn_blocking(move || -> io::Result<Vec<SocketAddr>> {
        let iter = host_port.to_socket_addrs()?;
        Ok(iter.collect())
    });

    let (async_result, blocking_result) = tokio::join!(async_lookup, blocking_lookup);

    let mut addresses = Vec::new();
    let mut errors = Vec::new();

    match async_result {
        Ok(iter) => addresses.extend(iter),
        Err(err) => errors.push(anyhow!("async resolver failed: {err}")),
    }

    match blocking_result {
        Ok(Ok(mut list)) => addresses.append(&mut list),
        Ok(Err(err)) => errors.push(anyhow!("std resolver failed: {err}")),
        Err(err) => errors.push(anyhow!("blocking resolver join error: {err}")),
    }

    if addresses.is_empty() {
        if let Some(error) = errors.into_iter().next() {
            return Err(error);
        }
        return Err(anyhow!(
            "no socket addresses resolved for host {}",
            spec.host
        ));
    }

    let mut seen = HashSet::new();
    addresses.retain(|addr| seen.insert(*addr));

    Ok(order_addresses(addresses))
}

fn order_addresses(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut v6 = VecDeque::new();
    let mut v4 = VecDeque::new();
    for addr in addresses {
        if addr.is_ipv6() {
            v6.push_back(addr);
        } else {
            v4.push_back(addr);
        }
    }

    let mut ordered = Vec::new();
    while !v6.is_empty() || !v4.is_empty() {
        if let Some(addr) = v6.pop_front() {
            ordered.push(addr);
        }
        if let Some(addr) = v4.pop_front() {
            ordered.push(addr);
        }
    }
    ordered
}

async fn connect_with_happy_eyeballs(
    addresses: Vec<SocketAddr>,
    token: CancellationToken,
    configurator: SocketConfigurator,
) -> TcpOutcome {
    let race_token = CancellationToken::new();
    let mut tasks = FuturesUnordered::new();
    for (index, address) in addresses.into_iter().enumerate() {
        tasks.push(run_attempt(
            index,
            address,
            token.clone(),
            race_token.clone(),
            configurator.clone(),
        ));
    }

    let mut observations = Vec::new();
    let mut success = None;
    let mut last_failure = None;
    let mut last_attempt = None;
    let mut error = None;
    let mut cancelled = false;

    while let Some(result) = tasks.next().await {
        let obs = result.observation;
        if result.success && success.is_none() {
            success = Some(obs.clone());
            race_token.cancel();
        }
        if !result.success && !result.cancelled {
            last_failure = Some(obs.clone());
        }
        if result.cancelled {
            cancelled = true;
        }
        last_attempt = Some(obs.clone());
        if let Some(err) = result.error {
            if result.cancelled {
                if error.is_none() {
                    error = Some(err);
                }
            } else {
                error = Some(err);
            }
        }
        observations.push(obs);
    }

    TcpOutcome {
        observations,
        success,
        last_failure,
        last_attempt,
        error,
        cancelled,
    }
}

async fn run_attempt(
    index: usize,
    address: SocketAddr,
    global: CancellationToken,
    race: CancellationToken,
    configurator: SocketConfigurator,
) -> AttemptResult {
    let attempt = TcpAttempt::new(index, address);
    if index > 0 {
        let delay = delay_for(index);
        let sleeper = sleep(delay);
        tokio::pin!(sleeper);
        tokio::select! {
            _ = sleeper.as_mut() => {}
            _ = race.cancelled() => {
                return attempt.superseded(Duration::default());
            }
            _ = global.cancelled() => {
                return attempt.cancelled(Duration::default());
            }
        }
    }

    let start = Instant::now();
    let mut connect = Box::pin(connect_once(address, configurator));
    tokio::select! {
        res = &mut connect => {
            let elapsed = start.elapsed();
            match res {
                Ok(()) => attempt.success(elapsed),
                Err(err) => attempt.failure(elapsed, err),
            }
        }
        _ = race.cancelled() => {
            let elapsed = start.elapsed();
            attempt.superseded(elapsed)
        }
        _ = global.cancelled() => {
            let elapsed = start.elapsed();
            attempt.cancelled(elapsed)
        }
    }
}

fn delay_for(index: usize) -> Duration {
    HAPPY_EYEBALLS_DELAY
        .checked_mul(index as u32)
        .unwrap_or_else(|| HAPPY_EYEBALLS_DELAY)
}

async fn connect_once(address: SocketAddr, configurator: SocketConfigurator) -> io::Result<()> {
    let socket = configure_socket(&address, &configurator)?;
    let stream = socket.connect(address).await?;
    stream.set_nodelay(true)?;
    Ok(())
}

fn configure_socket(
    address: &SocketAddr,
    configurator: &SocketConfigurator,
) -> io::Result<TcpSocket> {
    let socket = match address {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    configurator.configure_tcp_client(&socket);
    Ok(socket)
}

fn tcp_resolution_failure_observation(spec: &TcpSpec, error: String) -> Observation {
    Observation {
        name: "tcp_resolution".into(),
        value: json!({
            "host": spec.host,
            "port": spec.port,
            "error": error,
        }),
        unit: None,
    }
}

#[derive(Debug)]
struct TcpOutcome {
    observations: Vec<TcpObservation>,
    success: Option<TcpObservation>,
    last_failure: Option<TcpObservation>,
    last_attempt: Option<TcpObservation>,
    error: Option<io::Error>,
    cancelled: bool,
}

#[derive(Clone, Debug)]
pub struct TcpObservation {
    attempt: usize,
    address: SocketAddr,
    success: bool,
    elapsed: Duration,
    error: Option<String>,
    errno: Option<i32>,
    cancelled: bool,
    reason: Option<String>,
}

impl TcpObservation {
    fn success(attempt: usize, address: SocketAddr, elapsed: Duration) -> Self {
        Self {
            attempt,
            address,
            success: true,
            elapsed,
            error: None,
            errno: None,
            cancelled: false,
            reason: None,
        }
    }

    fn failure(
        attempt: usize,
        address: SocketAddr,
        elapsed: Duration,
        message: String,
        errno: Option<i32>,
    ) -> Self {
        Self {
            attempt,
            address,
            success: false,
            elapsed,
            error: Some(message),
            errno,
            cancelled: false,
            reason: None,
        }
    }

    fn cancelled(
        attempt: usize,
        address: SocketAddr,
        elapsed: Duration,
        reason: &'static str,
    ) -> Self {
        Self {
            attempt,
            address,
            success: false,
            elapsed,
            error: None,
            errno: None,
            cancelled: true,
            reason: Some(reason.into()),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1000.0
    }

    fn into_observation(self) -> Observation {
        Observation {
            name: "tcp_attempt".into(),
            value: json!({
                "attempt": self.attempt,
                "success": self.success,
                "address": self.address.to_string(),
                "rtt_ms": self.elapsed_ms(),
                "error": self.error,
                "errno": self.errno,
                "cancelled": self.cancelled,
                "reason": self.reason,
            }),
            unit: None,
        }
    }

    fn into_summary(self) -> Observation {
        Observation {
            name: "tcp_summary".into(),
            value: json!({
                "success": true,
                "address": self.address.to_string(),
                "rtt_ms": self.elapsed_ms(),
                "errno": self.errno,
            }),
            unit: None,
        }
    }

    fn failure_summary(err: io::Error, context: Option<TcpObservation>) -> Observation {
        let errno = err.raw_os_error();
        let message = err.to_string();
        let (address, rtt_ms, context_errno) = context
            .map(|ctx| {
                (
                    Some(ctx.address.to_string()),
                    Some(ctx.elapsed_ms()),
                    ctx.errno,
                )
            })
            .unwrap_or((None, None, None));
        Observation {
            name: "tcp_summary".into(),
            value: json!({
                "success": false,
                "address": address,
                "rtt_ms": rtt_ms,
                "error": message,
                "errno": context_errno.or(errno),
            }),
            unit: None,
        }
    }

    fn cancellation_summary(context: Option<TcpObservation>) -> Observation {
        let (address, rtt_ms, errno, reason) = context
            .map(|ctx| {
                (
                    Some(ctx.address.to_string()),
                    Some(ctx.elapsed_ms()),
                    ctx.errno,
                    ctx.reason,
                )
            })
            .unwrap_or((None, None, None, None));
        Observation {
            name: "tcp_summary".into(),
            value: json!({
                "success": false,
                "cancelled": true,
                "address": address,
                "rtt_ms": rtt_ms,
                "errno": errno,
                "reason": reason,
            }),
            unit: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TcpAttempt {
    index: usize,
    address: SocketAddr,
}

impl TcpAttempt {
    fn new(index: usize, address: SocketAddr) -> Self {
        Self { index, address }
    }

    fn success(self, elapsed: Duration) -> AttemptResult {
        AttemptResult {
            observation: TcpObservation::success(self.index, self.address, elapsed),
            success: true,
            cancelled: false,
            error: None,
        }
    }

    fn failure(self, elapsed: Duration, error: io::Error) -> AttemptResult {
        let errno = error.raw_os_error();
        let message = error.to_string();
        AttemptResult {
            observation: TcpObservation::failure(self.index, self.address, elapsed, message, errno),
            success: false,
            cancelled: false,
            error: Some(error),
        }
    }

    fn superseded(self, elapsed: Duration) -> AttemptResult {
        AttemptResult {
            observation: TcpObservation::cancelled(self.index, self.address, elapsed, "superseded"),
            success: false,
            cancelled: false,
            error: None,
        }
    }

    fn cancelled(self, elapsed: Duration) -> AttemptResult {
        AttemptResult {
            observation: TcpObservation::cancelled(self.index, self.address, elapsed, "cancelled"),
            success: false,
            cancelled: true,
            error: Some(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
        }
    }
}

struct AttemptResult {
    observation: TcpObservation,
    success: bool,
    cancelled: bool,
    error: Option<io::Error>,
}
