use std::{env, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use codecrafters_dns_server::BytePacketBuf;
use tokio::{
    net::UdpSocket,
    sync::Semaphore,
    time::{timeout, Instant},
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let config = ProxyConfig::from_env()?;
    info!(?config, "starting DNS proxy");

    let listener = Arc::new(
        UdpSocket::bind(config.listen_addr)
            .await
            .with_context(|| format!("failed to bind UDP socket on {}", config.listen_addr))?,
    );

    let concurrency_limit = Arc::new(Semaphore::new(config.max_inflight_requests));

    loop {
        let mut request_buffer = BytePacketBuf::new();
        let (len, peer) = listener
            .recv_from(&mut request_buffer.buf)
            .await
            .context("failed to receive request")?;

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
        upstream_socket.recv(&mut response_buffer.buf),
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
