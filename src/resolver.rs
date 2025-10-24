use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use moka::future::Cache;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{Mutex, Notify},
    time::{sleep, timeout},
};

use crate::{BytePacketBuf, Dns, DnsRecord, QueryType, ResultCode, MAX_PACKET_SIZE};

static NEXT_MESSAGE_ID: AtomicU16 = AtomicU16::new(0);

const DEFAULT_TTL_SECS: u32 = 60;

#[derive(Debug, Clone)]
pub struct ARecord {
    pub addr: Ipv4Addr,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub struct AAAARecord {
    pub addr: Ipv6Addr,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub struct MxRecord {
    pub preference: u16,
    pub exchange: String,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub struct NsRecord {
    pub host: String,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub struct TxtRecord {
    pub data: Vec<String>,
    pub ttl: u32,
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub qname: String,
    pub a_records: Vec<ARecord>,
    pub aaaa_records: Vec<AAAARecord>,
    pub mx_records: Vec<MxRecord>,
    pub ns_records: Vec<NsRecord>,
    pub txt_records: Vec<TxtRecord>,
    pub ttl: Duration,
}

#[derive(Clone)]
pub struct DnsResolver {
    upstream: SocketAddr,
    timeout: Duration,
    cache: Cache<String, Arc<Resolution>>,
    singleflight: Arc<Mutex<HashMap<String, SingleflightEntry>>>,
}

struct SingleflightEntry {
    notify: Arc<Notify>,
    expires_at: Instant,
}

impl DnsResolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self {
            upstream,
            timeout,
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(DEFAULT_TTL_SECS as u64))
                .build(),
            singleflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn resolve(&self, qname: &str) -> Result<Arc<Resolution>> {
        let key = qname.to_ascii_lowercase();
        if let Some(value) = self.cache.get(&key).await {
            return Ok(value);
        }

        loop {
            let mut guard = self.singleflight.lock().await;
            if let Some(entry) = guard.get(&key) {
                if entry.expires_at > Instant::now() {
                    let notify = Arc::clone(&entry.notify);
                    drop(guard);
                    notify.notified().await;
                    if let Some(value) = self.cache.get(&key).await {
                        return Ok(value);
                    }
                    continue;
                } else {
                    guard.remove(&key);
                }
            }

            let notify = Arc::new(Notify::new());
            guard.insert(
                key.clone(),
                SingleflightEntry {
                    notify: Arc::clone(&notify),
                    expires_at: Instant::now() + Duration::from_millis(50),
                },
            );
            drop(guard);

            let result = self.resolve_uncached(&key).await;
            if let Ok(resolution) = &result {
                if resolution.ttl > Duration::from_millis(0) {
                    self.cache.insert(key.clone(), Arc::clone(resolution)).await;
                    let cache = self.cache.clone();
                    let key_for_task = key.clone();
                    let ttl = resolution.ttl;
                    tokio::spawn(async move {
                        sleep(ttl).await;
                        cache.invalidate(&key_for_task).await;
                    });
                }
            }

            let mut guard = self.singleflight.lock().await;
            if let Some(entry) = guard.remove(&key) {
                entry.notify.notify_waiters();
            }
            drop(guard);

            if let Some(value) = self.cache.get(&key).await {
                return Ok(value);
            }

            match result {
                Ok(value) => return Ok(value),
                Err(err) => return Err(err),
            }
        }
    }

    async fn resolve_uncached(&self, qname: &str) -> Result<Arc<Resolution>> {
        let mut futures = FuturesUnordered::new();
        for qtype in [
            QueryType::A,
            QueryType::AAAA,
            QueryType::MX,
            QueryType::NS,
            QueryType::TXT,
        ] {
            futures.push(self.query_records(qname.to_string(), qtype));
        }

        let mut a_records = Vec::new();
        let mut aaaa_records = Vec::new();
        let mut mx_records = Vec::new();
        let mut ns_records = Vec::new();
        let mut txt_records = Vec::new();
        let mut min_ttl: Option<u32> = None;
        let mut had_success = false;
        let mut primary_success = false;
        let mut primary_error: Option<anyhow::Error> = None;

        while let Some((qtype, result)) = futures.next().await {
            match result {
                Ok(records) => {
                    if matches!(qtype, QueryType::A | QueryType::AAAA) {
                        primary_success = true;
                    }
                    if !records.is_empty() {
                        had_success = true;
                    }
                    for record in records {
                        match record {
                            DnsRecord::A { addr, ttl, .. } => {
                                min_ttl = update_min_ttl(min_ttl, ttl);
                                a_records.push(ARecord { addr, ttl });
                            }
                            DnsRecord::AAAA { addr, ttl, .. } => {
                                min_ttl = update_min_ttl(min_ttl, ttl);
                                aaaa_records.push(AAAARecord { addr, ttl });
                            }
                            DnsRecord::MX {
                                preference,
                                exchange,
                                ttl,
                                ..
                            } => {
                                min_ttl = update_min_ttl(min_ttl, ttl);
                                mx_records.push(MxRecord {
                                    preference,
                                    exchange,
                                    ttl,
                                });
                            }
                            DnsRecord::NS { host, ttl, .. } => {
                                min_ttl = update_min_ttl(min_ttl, ttl);
                                ns_records.push(NsRecord { host, ttl });
                            }
                            DnsRecord::TXT { data, ttl, .. } => {
                                min_ttl = update_min_ttl(min_ttl, ttl);
                                txt_records.push(TxtRecord { data, ttl });
                            }
                            DnsRecord::OPT { .. } | DnsRecord::UNKNOWN { .. } => {}
                        }
                    }
                }
                Err(err) => {
                    if matches!(qtype, QueryType::A | QueryType::AAAA) {
                        primary_error = Some(err);
                    }
                }
            }
        }

        if !primary_success {
            if let Some(err) = primary_error.take() {
                return Err(err);
            }
        }

        if !had_success {
            if let Some(err) = primary_error.take() {
                return Err(err);
            }
        }

        let ttl_secs = min_ttl.unwrap_or(DEFAULT_TTL_SECS).max(1);
        let ttl = Duration::from_secs(ttl_secs.into());
        Ok(Arc::new(Resolution {
            qname: qname.to_string(),
            a_records,
            aaaa_records,
            mx_records,
            ns_records,
            txt_records,
            ttl,
        }))
    }

    async fn query_records(
        &self,
        qname: String,
        qtype: QueryType,
    ) -> (QueryType, Result<Vec<DnsRecord>>) {
        match self.perform_query(&qname, qtype.clone()).await {
            Ok(message) => {
                if message.header.flags.rescode != ResultCode::NOERROR {
                    return (
                        qtype,
                        Err(anyhow!("dns error: {:?}", message.header.flags.rescode)),
                    );
                }
                let mut filtered = Vec::new();
                for record in &message.answer {
                    if record_matches(record, &qtype, &qname) {
                        filtered.push(record.clone());
                    }
                }
                (qtype, Ok(filtered))
            }
            Err(err) => (qtype, Err(err)),
        }
    }

    async fn perform_query(&self, qname: &str, qtype: QueryType) -> Result<Dns> {
        let id = NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
        let (request, len) = build_query(id, qname, qtype.clone())?;
        match self.exchange_udp(&request, len, id).await {
            Ok((message, truncated)) => {
                if truncated {
                    self.exchange_tcp(&request, len, id).await
                } else {
                    Ok(message)
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn exchange_udp(
        &self,
        request: &BytePacketBuf,
        len: usize,
        id: u16,
    ) -> Result<(Dns, bool)> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket
            .send_to(&request.as_slice()[..len], self.upstream)
            .await
            .context("failed to send UDP request")?;

        let mut response = BytePacketBuf::new();
        let (received, _) = timeout(
            self.timeout,
            socket.recv_from(&mut response.as_mut_slice()[..]),
        )
        .await
        .context("UDP receive timeout")??;
        response.set_len(received)?;
        response.seek(0)?;

        let mut parsed = response;
        let message = Dns::parse_req(&mut parsed)?;
        if message.header.id != id {
            return Err(anyhow!("mismatched DNS transaction ID"));
        }
        let truncated = message.header.flags.truncated_message;
        Ok((message, truncated))
    }

    async fn exchange_tcp(&self, request: &BytePacketBuf, len: usize, id: u16) -> Result<Dns> {
        let mut stream = TcpStream::connect(self.upstream)
            .await
            .context("failed to connect via TCP")?;

        let length_prefix = (len as u16).to_be_bytes();
        stream
            .write_all(&length_prefix)
            .await
            .context("failed to write TCP length prefix")?;
        stream
            .write_all(&request.as_slice()[..len])
            .await
            .context("failed to write TCP payload")?;

        let mut len_buf = [0u8; 2];
        timeout(self.timeout, stream.read_exact(&mut len_buf))
            .await
            .context("TCP read timeout (length)")??;
        let response_len = u16::from_be_bytes(len_buf) as usize;

        let mut response = BytePacketBuf::new();
        timeout(
            self.timeout,
            stream.read_exact(&mut response.as_mut_slice()[..response_len]),
        )
        .await
        .context("TCP read timeout (payload)")??;
        response.set_len(response_len)?;
        response.seek(0)?;

        let mut parsed = response;
        let message = Dns::parse_req(&mut parsed)?;
        if message.header.id != id {
            return Err(anyhow!("mismatched DNS transaction ID"));
        }
        Ok(message)
    }
}

fn record_matches(record: &DnsRecord, qtype: &QueryType, qname: &str) -> bool {
    match (record, qtype) {
        (DnsRecord::A { domain, .. }, QueryType::A) => domain == qname,
        (DnsRecord::AAAA { domain, .. }, QueryType::AAAA) => domain == qname,
        (DnsRecord::MX { domain, .. }, QueryType::MX) => domain == qname,
        (DnsRecord::NS { domain, .. }, QueryType::NS) => domain == qname,
        (DnsRecord::TXT { domain, .. }, QueryType::TXT) => domain == qname,
        _ => false,
    }
}

fn update_min_ttl(current: Option<u32>, ttl: u32) -> Option<u32> {
    Some(match current {
        Some(existing) => existing.min(ttl),
        None => ttl,
    })
}

fn build_query(id: u16, qname: &str, qtype: QueryType) -> Result<(BytePacketBuf, usize)> {
    let mut buffer = BytePacketBuf::new();
    buffer.write_u16(id)?;
    buffer.write_u16(0x0100)?; // recursion desired
    buffer.write_u16(1)?;
    buffer.write_u16(0)?;
    buffer.write_u16(0)?;
    buffer.write_u16(1)?; // EDNS0 OPT record

    buffer.write_qname(qname)?;
    buffer.write_u16(u16::from(qtype))?;
    buffer.write_u16(1)?; // class IN

    buffer.write_u8(0)?; // root label for OPT
    buffer.write_u16(u16::from(QueryType::OPT))?;
    buffer.write_u16(MAX_PACKET_SIZE as u16)?;
    buffer.write_u32(0)?; // extended RCODE + flags
    buffer.write_u16(0)?; // no options

    let len = buffer.pos();
    buffer.set_len(len)?;
    buffer.seek(0)?;
    Ok((buffer, len))
}
