use std::{collections::HashMap, env, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use dns_agent::{
    control_plane::{
        AgentBootstrap, ControlPlaneClient, ControlPlaneTransport, HttpControlPlaneTransport,
        RegisterRequest, TaskKind,
    },
    runtime::TimerService,
    BytePacketBuf,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Semaphore},
    time::{interval, timeout, Instant, MissedTickBehavior},
};
use tracing::{debug, info, warn};

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
                let (batch_tx, mut batch_rx) = mpsc::channel::<Vec<u64>>(32);
                tokio::spawn(async move {
                    while let Some(batch) = batch_rx.recv().await {
                        info!(leases = batch.len(), "dispatcher received batch");
                    }
                });
                tokio::spawn(run_control_plane(client, batch_tx, timer.clone()));
            }
            Err(err) => {
                warn!(error = %err, "failed to initialize control plane client");
            }
        }
    }

    loop {
        let mut request_buffer = BytePacketBuf::new();
        let (len, peer) = listener
            .recv_from(&mut request_buffer.buf[..])
            .await
            .context("failed to receive request")?;

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

async fn run_control_plane<T>(
    client: ControlPlaneClient<T>,
    batch_tx: mpsc::Sender<Vec<u64>>,
    timer: TimerService,
) where
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
        return;
    }

    let heartbeat_client = client.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = heartbeat_client.heartbeat().await {
                warn!(error = %err, "heartbeat failed");
            }
        }
    });

    let extend_client = client.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let snapshot = extend_client.snapshot().await;
            if snapshot.leases.is_empty() {
                continue;
            }
            let lease_ids: Vec<u64> = snapshot.leases.keys().copied().collect();
            if let Err(err) = extend_client
                .extend(lease_ids, Duration::from_millis(500))
                .await
            {
                warn!(error = %err, "lease extension failed");
            }
        }
    });

    let mut capacities = HashMap::new();
    capacities.insert(TaskKind::Dns, 4);
    loop {
        match client.claim(&capacities).await {
            Ok(batch) => {
                if batch.leases.is_empty() {
                    if timer.sleep(Duration::from_millis(250)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let lease_ids: Vec<u64> = batch.leases.iter().map(|lease| lease.lease_id).collect();
                if batch_tx.send(lease_ids).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                warn!(error = %err, "claim loop failed");
                if timer.sleep(Duration::from_secs(1)).await.is_err() {
                    break;
                }
            }
        }
    }
}
