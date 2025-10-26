use std::{collections::HashMap, env, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use dns_agent::{
    control_plane::{
        AgentBootstrap, ControlPlaneClient, ControlPlaneTransport, HttpControlPlaneTransport,
        RegisterRequest, TaskKind,
    },
    dispatcher::{
        spawn_dispatcher, DispatchIngress, DispatcherConfig, DispatcherHandle, LeaseAssignment,
    },
    lease_extender::{
        spawn_lease_extender, LeaseExtenderClient, LeaseExtenderConfig, LeaseExtenderError,
        LeaseExtenderHandle,
    },
    runtime::TimerService,
    BytePacketBuf,
};
use tokio::signal;
use tokio::{
    net::UdpSocket,
    sync::Semaphore,
    task::JoinHandle,
    time::{interval, timeout, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use dns_agent::workers::{
    spawn_worker_pools, WorkerHandlers, WorkerPoolsConfig, WorkerPoolsHandle,
};

#[derive(Clone, Debug)]
struct ProxyConfig {
    listen_addr: SocketAddr,
    upstream_addr: SocketAddr,
    upstream_timeout: Duration,
    max_inflight_requests: usize,
}

impl ProxyConfig {
    fn from_env() -> Result<Self> {
        let listen_addr: SocketAddr = env::var("AGENT_DNS_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:2053".to_string())
            .parse()
            .context("invalid listen address")?;

        let upstream_addr: SocketAddr = env::var("AGENT_DNS_UPSTREAM")
            .unwrap_or_else(|_| "127.0.0.1:5354".to_string())
            .parse()
            .context("invalid upstream address")?;

        let upstream_timeout = env::var("AGENT_DNS_UPSTREAM_TIMEOUT_MS")
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(2500));

        let max_inflight_requests = env::var("AGENT_MAX_INFLIGHT")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(2048);

        Ok(Self {
            listen_addr,
            upstream_addr,
            upstream_timeout,
            max_inflight_requests,
        })
    }
}

fn bootstrap_from_env() -> Result<Option<AgentBootstrap>> {
    let agent_id = match env::var("AGENT_ID") {
        Ok(value) => Some(value.parse::<u64>().context("invalid AGENT_ID")?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(anyhow!("AGENT_ID must be valid unicode"));
        }
    };

    let auth_token = match env::var("AGENT_AUTH_TOKEN") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(anyhow!("AGENT_AUTH_TOKEN must be valid unicode"));
        }
    };

    match (agent_id, auth_token) {
        (Some(agent_id), Some(auth_token)) => Ok(Some(AgentBootstrap {
            agent_id,
            auth_token,
        })),
        (None, None) => Ok(None),
        _ => Err(anyhow!(
            "AGENT_ID and AGENT_AUTH_TOKEN must both be set when bootstrapping"
        )),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let config = ProxyConfig::from_env()?;
    let timer = TimerService::new();
    info!(?config, "starting DNS proxy");

    let listener = Arc::new(
        UdpSocket::bind(config.listen_addr)
            .await
            .with_context(|| format!("failed to bind UDP socket on {}", config.listen_addr))?,
    );

    let concurrency_limit = Arc::new(Semaphore::new(config.max_inflight_requests));
    let shutdown = CancellationToken::new();
    let shutdown_for_signal = shutdown.clone();
    let ctrl_c_task = tokio::spawn(async move {
        let shutdown_cancel = shutdown_for_signal.clone();
        tokio::select! {
            result = signal::ctrl_c() => {
                match result {
                    Ok(()) => {
                        shutdown_cancel.cancel();
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to install ctrl+c handler");
                        shutdown_cancel.cancel();
                    }
                }
            }
            _ = shutdown_for_signal.cancelled() => {}
        }
    });

    let mut dispatcher_handle: Option<DispatcherHandle> = None;
    let mut worker_handle: Option<WorkerPoolsHandle> = None;
    let mut lease_extender_handle: Option<LeaseExtenderHandle> = None;
    let mut control_plane_task: Option<JoinHandle<Result<()>>> = None;

    if let Ok(base_url) = env::var("AGENT_CONTROL_PLANE") {
        let bootstrap = bootstrap_from_env()?;
        match HttpControlPlaneTransport::new(&base_url) {
            Ok(transport) => {
                if let Some(ref credentials) = bootstrap {
                    transport.set_auth_headers(
                        credentials.agent_id.to_string(),
                        credentials.auth_token.clone(),
                    );
                }
                let client = ControlPlaneClient::with_bootstrap(transport, bootstrap.clone());

                let dispatcher_config = DispatcherConfig::default();
                let (dispatch_ingress, dispatch_queues, handle) =
                    spawn_dispatcher(dispatcher_config);
                dispatcher_handle = Some(handle);

                let lease_extender_config = LeaseExtenderConfig {
                    extend_every: Duration::from_secs(2),
                    extend_by: Duration::from_millis(500),
                };
                let (lease_handle, lease_client) =
                    spawn_lease_extender(client.clone(), lease_extender_config);
                lease_extender_handle = Some(lease_handle);

                let worker_config = WorkerPoolsConfig::from_env();
                let claim_capacities = worker_capacities(&worker_config);
                let worker_handlers = WorkerHandlers::default();
                let worker_pools = spawn_worker_pools(
                    worker_config,
                    dispatch_queues,
                    worker_handlers,
                    client.clone(),
                    Some(lease_client.clone()),
                )?;
                worker_handle = Some(worker_pools);

                let timer_clone = timer.clone();
                let shutdown_clone = shutdown.clone();
                let capacities = claim_capacities.clone();
                control_plane_task = Some(tokio::spawn(async move {
                    run_control_plane(
                        client,
                        dispatch_ingress,
                        lease_client,
                        timer_clone,
                        capacities,
                        shutdown_clone,
                    )
                    .await
                }));
            }
            Err(err) => {
                warn!(error = %err, "failed to initialize control plane client");
            }
        }
    }

    loop {
        let mut request_buffer = BytePacketBuf::new();
        let recv_result = tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            result = listener.recv_from(&mut request_buffer.buf[..]) => result,
        };
        let (len, peer) = recv_result.context("failed to receive request")?;

        request_buffer
            .set_len(len)
            .context("failed to set request length")?;
        request_buffer.seek(0)?;

        let permit = concurrency_limit
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let listener = Arc::clone(&listener);
        let proxy_config = config.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) =
                handle_request(listener, proxy_config, request_buffer, len, peer).await
            {
                warn!(?peer, error = %err, "failed to proxy DNS request");
            }
        });
    }

    if !shutdown.is_cancelled() {
        shutdown.cancel();
    }

    if let Some(task) = control_plane_task {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "control plane task exited with error");
            }
            Err(err) => {
                warn!(error = %err, "control plane task join error");
            }
        }
    }

    if let Some(handle) = dispatcher_handle {
        handle.shutdown().await;
    }

    if let Some(handle) = worker_handle {
        handle.shutdown().await;
    }

    if let Some(handle) = lease_extender_handle {
        handle.shutdown().await;
    }

    let _ = ctrl_c_task.await;

    Ok(())
}

async fn handle_request(
    listener: Arc<UdpSocket>,
    config: ProxyConfig,
    request_buffer: BytePacketBuf,
    request_len: usize,
    peer: SocketAddr,
) -> Result<()> {
    let start = Instant::now();
    let request_id = u16::from_be_bytes([request_buffer.buf[0], request_buffer.buf[1]]);

    let upstream_socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to allocate ephemeral UDP socket")?;

    let bytes_sent = upstream_socket
        .send_to(&request_buffer.buf[..request_len], config.upstream_addr)
        .await
        .context("failed to forward request to upstream")?;

    if bytes_sent != request_len {
        warn!(
            expected = request_len,
            sent = bytes_sent,
            "truncated DNS request"
        );
    }

    let mut response_buffer = BytePacketBuf::new();
    let upstream_result = timeout(
        config.upstream_timeout,
        upstream_socket.recv(&mut response_buffer.buf[..]),
    )
    .await;

    let response_len = match upstream_result {
        Ok(Ok(len)) => len,
        Ok(Err(err)) => return Err(err).context("error receiving response from upstream"),
        Err(_) => {
            debug!("upstream timeout");
            return Ok(());
        }
    };

    response_buffer
        .set_len(response_len)
        .context("failed to set response length")?;
    response_buffer.seek(0)?;

    response_buffer.buf[0] = (request_id >> 8) as u8;
    response_buffer.buf[1] = (request_id & 0xFF) as u8;

    listener
        .send_to(&response_buffer.buf[..response_len], peer)
        .await
        .context("failed to send response")?;

    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    debug!(
        ?peer,
        latency_ms,
        bytes = response_len,
        "served DNS response"
    );

    Ok(())
}

fn worker_capacities(config: &WorkerPoolsConfig) -> HashMap<TaskKind, usize> {
    let mut capacities = HashMap::new();
    capacities.insert(TaskKind::Dns, config.concurrency(TaskKind::Dns));
    capacities.insert(TaskKind::Http, config.concurrency(TaskKind::Http));
    capacities.insert(TaskKind::Tcp, config.concurrency(TaskKind::Tcp));
    capacities.insert(TaskKind::Ping, config.concurrency(TaskKind::Ping));
    capacities.insert(TaskKind::Trace, config.concurrency(TaskKind::Trace));
    capacities
}

async fn run_control_plane<T>(
    client: ControlPlaneClient<T>,
    dispatcher: DispatchIngress,
    lease_extender: LeaseExtenderClient,
    timer: TimerService,
    capacities: HashMap<TaskKind, usize>,
    shutdown: CancellationToken,
) -> Result<()>
where
    T: dns_agent::control_plane::ControlPlaneTransport + 'static,
{
    let snapshot = client.snapshot().await;
    if let Some(agent_id) = snapshot.agent_id {
        info!(
            agent_id,
            "restored control plane session from bootstrap state"
        );
    } else if let Err(err) = client
        .register(RegisterRequest {
            hostname: hostname::get()
                .ok()
                .and_then(|value| value.into_string().ok()),
        })
        .await
    {
        warn!(error = %err, "control plane registration failed");
        return Ok(());
    }

    let heartbeat_client = client.clone();
    let heartbeat_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = heartbeat_shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(err) = heartbeat_client.heartbeat().await {
                        warn!(error = %err, "heartbeat failed");
                    }
                }
            }
        }
    });

    let mut backpressure = dispatcher.subscribe_backpressure();

    loop {
        if shutdown.is_cancelled() {
            break;
        }

        while *backpressure.borrow() {
            let changed = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                changed = backpressure.changed() => changed,
            };
            if changed.is_err() {
                return Ok(());
            }
        }

        let claim = client.claim(&capacities);
        let batch = tokio::select! {
            _ = shutdown.cancelled() => break,
            result = claim => result,
        };

        match batch {
            Ok(batch) => {
                if batch.leases.is_empty() {
                    let sleep = timer.sleep(Duration::from_millis(250));
                    let should_continue = tokio::select! {
                        _ = shutdown.cancelled() => false,
                        result = sleep => result.is_ok(),
                    };
                    if !should_continue {
                        break;
                    }
                    continue;
                }

                info!(leases = batch.leases.len(), "dispatcher received batch");

                for lease in batch.leases {
                    let lease_id = lease.lease_id;
                    let token = CancellationToken::new();
                    if let Err(err) = lease_extender.track(&lease, &token).await {
                        match err {
                            LeaseExtenderError::Closed => return Ok(()),
                        }
                    }

                    let assignment = LeaseAssignment::new(lease, token);
                    if let Err(err) = dispatcher.dispatch(assignment).await {
                        warn!(error = %err, lease_id, "failed to dispatch lease");
                        if let Err(err) = lease_extender.finish(lease_id).await {
                            warn!(error = %err, lease_id, "failed to finalize lease tracking after dispatch error");
                        }
                        return Ok(());
                    }
                }
            }
            Err(err) => {
                warn!(error = %err, "claim loop failed");
                let sleep = timer.sleep(Duration::from_secs(1));
                let should_continue = tokio::select! {
                    _ = shutdown.cancelled() => false,
                    result = sleep => result.is_ok(),
                };
                if !should_continue {
                    break;
                }
            }
        }
    }

    Ok(())
}
