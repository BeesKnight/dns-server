mod support;

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::Instant;

use dns_agent::control_plane::{
    Lease, LeaseReport, Observation, PingSpec, TaskKind, TaskSpec, TcpSpec, TraceSpec,
};
use dns_agent::dispatcher::{spawn_dispatcher, DispatcherConfig, LeaseAssignment};
use dns_agent::runtime::{SocketConfigurator, TimerService};
use dns_agent::workers::{
    spawn_worker_pools, PingEngine, PingProbeOutcome, PingProtocol, PingRequest, PingWorker,
    RawSocketCapability, ReportSink, TcpWorker, TraceEngine, TraceObservation, TraceProbeOutcome,
    TraceProbeRequest, TraceStatus, TraceWorker, WorkerHandler, WorkerHandlers, WorkerPoolsConfig,
    WorkerReport,
};
use dns_agent::{BytePacketBuf, Dns, DnsRecord, QueryType};
use serde_json::json;
use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;

include!(concat!(env!("OUT_DIR"), "/worker_test_macro.rs"));

fn copy_into_packet(bytes: &[u8]) -> Dns {
    let mut packet = BytePacketBuf::new();
    packet.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
    packet.set_len(bytes.len()).unwrap();
    packet.seek(0).unwrap();
    Dns::parse_req(&mut packet).unwrap()
}

fn make_response(id: u16, qname: &str, record: DnsRecord) -> Vec<u8> {
    let mut buffer = BytePacketBuf::new();
    buffer.write_u16(id).unwrap();
    buffer.write_u16(0x8180).unwrap();
    buffer.write_u16(1).unwrap();
    buffer.write_u16(1).unwrap();
    buffer.write_u16(0).unwrap();
    buffer.write_u16(0).unwrap();
    buffer.write_qname(qname).unwrap();
    let qtype = match record {
        DnsRecord::A { .. } => QueryType::A,
        DnsRecord::AAAA { .. } => QueryType::AAAA,
        DnsRecord::MX { .. } => QueryType::MX,
        DnsRecord::NS { .. } => QueryType::NS,
        DnsRecord::TXT { .. } => QueryType::TXT,
        _ => panic!("unsupported record"),
    };
    buffer.write_u16(u16::from(qtype)).unwrap();
    buffer.write_u16(1).unwrap();
    record.write(&mut buffer).unwrap();
    let len = buffer.pos();
    buffer.set_len(len).unwrap();
    buffer.as_slice().to_vec()
}

fn make_assignment(id: u64, kind: TaskKind) -> (LeaseAssignment, CancellationToken) {
    let lease = Lease {
        lease_id: id,
        task_id: id,
        kind,
        lease_until_ms: 0,
        spec: dummy_spec(kind, id),
    };
    let token = CancellationToken::new();
    let assignment = LeaseAssignment::new(lease, token.clone());
    (assignment, token)
}

fn make_trace_assignment(id: u64, spec: TraceSpec) -> (LeaseAssignment, CancellationToken) {
    let lease = Lease {
        lease_id: id,
        task_id: id,
        kind: TaskKind::Trace,
        lease_until_ms: 0,
        spec: TaskSpec::Trace(spec),
    };
    let token = CancellationToken::new();
    let assignment = LeaseAssignment::new(lease, token.clone());
    (assignment, token)
}

fn dummy_spec(kind: TaskKind, id: u64) -> TaskSpec {
    match kind {
        TaskKind::Dns => TaskSpec::Dns {
            query: format!("example-{id}.com"),
            server: None,
        },
        TaskKind::Http => TaskSpec::Http {
            url: format!("https://example.com/{id}"),
            method: Some("GET".into()),
        },
        TaskKind::Tcp => TaskSpec::Tcp(TcpSpec {
            host: "127.0.0.1".into(),
            port: 53,
        }),
        TaskKind::Ping => TaskSpec::Ping(PingSpec {
            host: "127.0.0.1".into(),
            count: Some(4),
            interval_ms: None,
            timeout_ms: None,
            rate_limit_per_sec: None,
        }),
        TaskKind::Trace => TaskSpec::Trace(TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(8),
        }),
    }
}

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
    let handlers = WorkerHandlers {
        dns: handler.clone(),
        ..WorkerHandlers::default()
    };

    let pools = spawn_worker_pools(pool_config, queues, handlers, reporter.clone(), None)
        .expect("worker pools spawn");

    for id in 1..=6 {
        let (assignment, _) = make_assignment(id, TaskKind::Dns);
        ingress.dispatch(assignment).await.expect("dispatch lease");
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
    let handlers = WorkerHandlers {
        dns: handler.clone(),
        ..WorkerHandlers::default()
    };

    let pools = spawn_worker_pools(pool_config, queues, handlers, reporter.clone(), None)
        .expect("spawn worker pools");

    for id in 1..=3 {
        let (assignment, _) = make_assignment(id, TaskKind::Dns);
        ingress.dispatch(assignment).await.expect("dispatch lease");
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

#[tokio::test]
async fn dns_worker_emits_record_observations() {
    let udp = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind udp socket");
    let addr = udp.local_addr().expect("udp socket address");
    let handled = Arc::new(AtomicUsize::new(0));
    let udp_task = {
        let handled = handled.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let (len, peer) = udp.recv_from(&mut buf).await.expect("receive query");
                let message = copy_into_packet(&buf[..len]);
                let question = message.question.first().expect("dns question");
                let ttl = match question.qtype {
                    QueryType::A => 120,
                    QueryType::AAAA => 240,
                    QueryType::MX => 360,
                    QueryType::NS => 480,
                    QueryType::TXT => 600,
                    _ => 0,
                };
                let response = match question.qtype {
                    QueryType::A => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::A {
                            domain: question.qname.clone(),
                            addr: Ipv4Addr::new(1, 2, 3, 4),
                            ttl,
                        },
                    ),
                    QueryType::AAAA => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::AAAA {
                            domain: question.qname.clone(),
                            addr: Ipv6Addr::LOCALHOST,
                            ttl,
                        },
                    ),
                    QueryType::MX => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::MX {
                            domain: question.qname.clone(),
                            preference: 10,
                            exchange: "mail.example.com".to_string(),
                            ttl,
                        },
                    ),
                    QueryType::NS => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::NS {
                            domain: question.qname.clone(),
                            host: "ns1.example.com".to_string(),
                            ttl,
                        },
                    ),
                    QueryType::TXT => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::TXT {
                            domain: question.qname.clone(),
                            data: vec!["hello".to_string()],
                            ttl,
                        },
                    ),
                    _ => unreachable!(),
                };
                udp.send_to(&response, peer).await.expect("send response");
                if handled.fetch_add(1, Ordering::SeqCst) + 1 == 5 {
                    break;
                }
            }
        })
    };

    let lease = Lease {
        lease_id: 900,
        task_id: 900,
        kind: TaskKind::Dns,
        lease_until_ms: 0,
        spec: TaskSpec::Dns {
            query: "records.example".into(),
            server: Some(addr.to_string()),
        },
    };
    let token = CancellationToken::new();
    let assignment = LeaseAssignment::new(lease, token);

    let worker = dns_agent::workers::DnsWorker;
    let report = worker
        .handle(assignment)
        .await
        .expect("dns worker completed");

    udp_task.await.expect("udp task completes");

    let (completed, cancelled) = report.into_parts();
    assert!(cancelled.is_empty(), "dns lease should not be cancelled");
    assert_eq!(completed.len(), 1, "expected a single completed lease");
    let observations = &completed[0].observations;
    let server_str = addr.to_string();

    let find = |name: &str| -> &Observation {
        observations
            .iter()
            .find(|obs| obs.name == name)
            .unwrap_or_else(|| panic!("missing observation {name}"))
    };

    let a_record = find("dns_a_record");
    assert_eq!(a_record.value["query"].as_str(), Some("records.example"));
    assert_eq!(a_record.value["server"].as_str(), Some(server_str.as_str()));
    assert_eq!(a_record.value["address"].as_str(), Some("1.2.3.4"));
    assert_eq!(a_record.value["ttl"].as_u64(), Some(120));

    let aaaa_record = find("dns_aaaa_record");
    assert_eq!(aaaa_record.value["address"].as_str(), Some("::1"));
    assert_eq!(aaaa_record.value["ttl"].as_u64(), Some(240));

    let mx_record = find("dns_mx_record");
    assert_eq!(
        mx_record.value["exchange"].as_str(),
        Some("mail.example.com")
    );
    assert_eq!(mx_record.value["preference"].as_u64(), Some(10));
    assert_eq!(mx_record.value["ttl"].as_u64(), Some(360));

    let ns_record = find("dns_ns_record");
    assert_eq!(ns_record.value["host"].as_str(), Some("ns1.example.com"));
    assert_eq!(ns_record.value["ttl"].as_u64(), Some(480));

    let txt_record = find("dns_txt_record");
    assert_eq!(
        txt_record.value["data"].as_array().map(|arr| arr.len()),
        Some(1)
    );
    assert_eq!(txt_record.value["data"][0].as_str(), Some("hello"));
    assert_eq!(txt_record.value["ttl"].as_u64(), Some(600));

    assert!(
        observations.iter().all(|obs| obs.name != "dns_error"),
        "successful resolution should not emit errors"
    );
}

worker_test!(worker_reports_cancellation, {
    let dispatcher_config = DispatcherConfig::default();
    let (ingress, queues, dispatcher_handle) = spawn_dispatcher(dispatcher_config);

    let reporter = TestReporter::default();

    let mut pool_config = WorkerPoolsConfig::new();
    pool_config.set_concurrency(TaskKind::Dns, 1);

    let handler = Arc::new(CancellableHandler::new());
    let handlers = WorkerHandlers {
        dns: handler.clone(),
        ..WorkerHandlers::default()
    };

    let pools = spawn_worker_pools(pool_config, queues, handlers, reporter.clone(), None)
        .expect("spawn worker pools");

    let (assignment, token) = make_assignment(1, TaskKind::Dns);
    ingress.dispatch(assignment).await.expect("dispatch lease");

    handler.wait_started(Duration::from_secs(1)).await;
    token.cancel();

    reporter.wait_for_cancelled(1, Duration::from_secs(2)).await;

    drop(ingress);
    pools.shutdown().await;
    dispatcher_handle.shutdown().await;

    let cancelled = reporter.cancelled_async().await;
    assert_eq!(cancelled, vec![1]);
    let reports = reporter.cancelled_reports_async().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].observations[0].name, "cancelled");
    let completed = reporter.completed_async().await;
    assert!(completed.is_empty());
});

worker_test!(tcp_worker_happy_eyeballs_fallback, {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind tcp listener");
    let port = listener.local_addr().expect("listener address").port();
    let accept = tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(socket);
        }
    });

    let lease = Lease {
        lease_id: 700,
        task_id: 700,
        kind: TaskKind::Tcp,
        lease_until_ms: 0,
        spec: TaskSpec::Tcp(TcpSpec {
            host: "localhost".into(),
            port,
        }),
    };
    let token = CancellationToken::new();
    let assignment = LeaseAssignment::new(lease, token);

    let worker = TcpWorker::default();
    let report = worker
        .handle(assignment)
        .await
        .expect("tcp worker completed");

    let (completed, cancelled) = report.into_parts();
    assert!(cancelled.is_empty(), "tcp lease should not be cancelled");
    assert_eq!(completed.len(), 1, "expected a single completed lease");
    let observations = &completed[0].observations;
    assert!(
        observations
            .iter()
            .any(|obs| obs.name == "tcp_attempt" && obs.value["success"].as_bool() == Some(true)),
        "successful attempt should be recorded",
    );
    assert!(
        observations
            .iter()
            .any(|obs| obs.name == "tcp_attempt" && obs.value["success"].as_bool() == Some(false)),
        "fallback attempt should record a failure",
    );
    let summary = observations
        .iter()
        .find(|obs| obs.name == "tcp_summary")
        .expect("summary observation present");
    assert_eq!(summary.value["success"].as_bool(), Some(true));
    let expected_address = format!("127.0.0.1:{port}");
    assert_eq!(
        summary.value["address"].as_str(),
        Some(expected_address.as_str()),
        "expected IPv4 address in summary",
    );
    accept.await.expect("listener task completes");
});

worker_test!(trace_worker_collects_observation, {
    let engine = Arc::new(MockTraceEngine::with_responses(vec![
        TraceProbeOutcome::Hop {
            address: Some(SocketAddr::from(([10, 0, 0, 1], 33434))),
            rtt: Some(Duration::from_millis(20)),
            reached: false,
        },
        TraceProbeOutcome::Timeout,
        TraceProbeOutcome::Hop {
            address: Some(SocketAddr::from(([10, 0, 0, 5], 33434))),
            rtt: Some(Duration::from_millis(35)),
            reached: true,
        },
    ]));
    let capability: Arc<dyn RawSocketCapability> = Arc::new(MockCapability::ok(true));
    let worker =
        TraceWorker::with_engine_and_config(engine, 64, capability, SocketConfigurator::default());

    let (assignment, _) = make_trace_assignment(
        42,
        TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(4),
        },
    );

    let report = worker.handle(assignment).await.expect("trace report");
    let (completed, cancelled) = report.into_parts();
    assert!(cancelled.is_empty());
    assert_eq!(completed.len(), 1);
    let observation = completed[0]
        .observations
        .iter()
        .find(|obs| obs.name == "trace")
        .expect("trace observation");
    let parsed: TraceObservation = serde_json::from_value(observation.value.clone()).unwrap();
    assert_eq!(parsed.status, TraceStatus::Ok);
    assert_eq!(parsed.hops.len(), 2);
    assert!(parsed.hops[0].success);
    assert!(parsed.hops[1].reached_destination);
    assert!(!parsed.nat_detected);
    assert!(!parsed.loop_detected);
    assert!(!parsed.permission_denied);
});

worker_test!(trace_worker_respects_rate_limits, {
    let engine = Arc::new(MockTraceEngine::with_responses(vec![
        TraceProbeOutcome::Timeout,
        TraceProbeOutcome::Timeout,
        TraceProbeOutcome::Hop {
            address: Some(SocketAddr::from(([192, 0, 2, 1], 33434))),
            rtt: Some(Duration::from_millis(10)),
            reached: true,
        },
    ]));
    let capability: Arc<dyn RawSocketCapability> = Arc::new(MockCapability::ok(true));
    let worker =
        TraceWorker::with_engine_and_config(engine, 2, capability, SocketConfigurator::default());

    let (assignment, _) = make_trace_assignment(
        99,
        TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(1),
        },
    );

    let start = Instant::now();
    let report = worker.handle(assignment).await.expect("trace report");
    let elapsed = start.elapsed();
    let (completed, _) = report.into_parts();
    assert_eq!(completed.len(), 1);
    assert!(
        elapsed >= Duration::from_millis(450),
        "elapsed {:?} shorter than rate limit",
        elapsed
    );
});

worker_test!(trace_worker_marks_permission_denied, {
    let engine = Arc::new(MockTraceEngine::with_responses(
        vec![TraceProbeOutcome::Timeout; 3],
    ));
    let capability: Arc<dyn RawSocketCapability> = Arc::new(MockCapability::ok(false));
    let worker =
        TraceWorker::with_engine_and_config(engine, 10, capability, SocketConfigurator::default());

    let (assignment, _) = make_trace_assignment(
        55,
        TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(1),
        },
    );

    let report = worker.handle(assignment).await.expect("trace report");
    let (completed, _) = report.into_parts();
    let observation = completed[0]
        .observations
        .iter()
        .find(|obs| obs.name == "trace")
        .expect("trace observation");
    let parsed: TraceObservation = serde_json::from_value(observation.value.clone()).unwrap();
    assert_eq!(parsed.status, TraceStatus::PermissionDenied);
    assert!(parsed.permission_denied);
});

#[derive(Debug)]
struct MockTraceEngine {
    responses: Arc<Mutex<VecDeque<TraceProbeOutcome>>>,
    calls: Arc<Mutex<Vec<TraceProbeRequest>>>,
}

impl MockTraceEngine {
    fn with_responses(responses: Vec<TraceProbeOutcome>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    async fn calls(&self) -> Vec<TraceProbeRequest> {
        self.calls.lock().await.clone()
    }
}

impl Clone for MockTraceEngine {
    fn clone(&self) -> Self {
        Self {
            responses: Arc::clone(&self.responses),
            calls: Arc::clone(&self.calls),
        }
    }
}

#[async_trait]
impl TraceEngine for MockTraceEngine {
    async fn probe(
        &self,
        request: TraceProbeRequest,
        cancel: CancellationToken,
        _configurator: SocketConfigurator,
    ) -> TraceProbeOutcome {
        if cancel.is_cancelled() {
            return TraceProbeOutcome::Error {
                error: "cancelled".into(),
            };
        }
        self.calls.lock().await.push(request);
        self.responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(TraceProbeOutcome::Timeout)
    }
}

#[derive(Debug, Clone)]
struct MockCapability {
    result: Result<bool, io::ErrorKind>,
}

impl MockCapability {
    fn ok(value: bool) -> Self {
        Self { result: Ok(value) }
    }

    #[allow(dead_code)]
    fn err(kind: io::ErrorKind) -> Self {
        Self { result: Err(kind) }
    }
}

impl RawSocketCapability for MockCapability {
    fn can_use_raw(&self) -> std::io::Result<bool> {
        match self.result {
            Ok(value) => Ok(value),
            Err(kind) => Err(std::io::Error::from(kind)),
        }
    }
}

#[tokio::test]
async fn timer_service_respects_virtual_time() {
    tokio::time::pause();
    let timer = TimerService::new();
    let flag = Arc::new(AtomicBool::new(false));
    let worker_flag = flag.clone();

    timer
        .schedule(Duration::from_millis(50), async move {
            worker_flag.store(true, Ordering::SeqCst);
        })
        .expect("timer scheduled");

    tokio::time::advance(Duration::from_millis(40)).await;
    assert!(
        !flag.load(Ordering::SeqCst),
        "timer fired before virtual time advanced sufficiently"
    );

    tokio::time::advance(Duration::from_millis(20)).await;
    tokio::task::yield_now().await;

    assert!(
        flag.load(Ordering::SeqCst),
        "timer did not fire after advancing virtual time"
    );
}

worker_test!(ping_worker_produces_observations, {
    let engine: Arc<dyn PingEngine> = Arc::new(MockPingEngine::new(vec![
        PingProbeOutcome::Success {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            rtt: Duration::from_millis(8),
            bytes: 64,
            ttl: Some(52),
        },
        PingProbeOutcome::Timeout {
            address: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            protocol: PingProtocol::Icmp,
        },
        PingProbeOutcome::FallbackSuccess {
            address: SocketAddr::from(([127, 0, 0, 1], 80)),
            rtt: Duration::from_millis(30),
        },
    ]));

    let worker = PingWorker::with_engine(engine);
    let lease = Lease {
        lease_id: 42,
        task_id: 100,
        kind: TaskKind::Ping,
        lease_until_ms: 0,
        spec: TaskSpec::Ping(PingSpec {
            host: "127.0.0.1".into(),
            count: Some(3),
            interval_ms: Some(10),
            timeout_ms: Some(200),
            rate_limit_per_sec: Some(10),
        }),
    };
    let assignment = LeaseAssignment::new(lease, CancellationToken::new());

    let report = worker
        .handle(assignment)
        .await
        .expect("ping worker should succeed");
    let (completed, cancelled) = report.into_parts();
    assert!(cancelled.is_empty());
    assert_eq!(completed.len(), 1);

    let observations = &completed[0].observations;
    assert_eq!(observations.len(), 4);
    for attempt in &observations[..3] {
        assert_eq!(attempt.name, "ping_attempt");
    }
    let success_value = observations[0].value.as_object().expect("attempt json");
    assert_eq!(
        success_value
            .get("protocol")
            .and_then(|value| value.as_str()),
        Some("icmp")
    );
    assert_eq!(
        success_value
            .get("success")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let fallback_value = observations[2].value.as_object().expect("fallback json");
    assert_eq!(
        fallback_value
            .get("protocol")
            .and_then(|value| value.as_str()),
        Some("tcp_syn")
    );
    assert_eq!(
        fallback_value
            .get("success")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let summary = observations.last().expect("summary observation");
    assert_eq!(summary.name, "ping_summary");
    let summary_value = summary.value.as_object().expect("summary json");
    assert_eq!(summary_value.get("sent").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(
        summary_value.get("received").and_then(|v| v.as_u64()),
        Some(2)
    );
    let reasons = summary_value
        .get("reasons")
        .and_then(|value| value.as_object())
        .expect("reasons map");
    assert_eq!(reasons.get("timeout").and_then(|v| v.as_u64()), Some(1));
});

struct MockPingEngine {
    outcomes: Mutex<VecDeque<PingProbeOutcome>>,
}

impl MockPingEngine {
    fn new(outcomes: Vec<PingProbeOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(VecDeque::from(outcomes)),
        }
    }
}

#[async_trait]
impl PingEngine for MockPingEngine {
    async fn probe(
        &self,
        _request: PingRequest,
        _cancel: CancellationToken,
        _configurator: SocketConfigurator,
    ) -> PingProbeOutcome {
        let mut guard = self.outcomes.lock().await;
        guard.pop_front().unwrap_or(PingProbeOutcome::Timeout {
            address: None,
            protocol: PingProtocol::Icmp,
        })
    }
}

worker_test!(tcp_worker_reports_cancellation_summary, {
    let worker = TcpWorker::default();
    let lease = Lease {
        lease_id: 701,
        task_id: 701,
        kind: TaskKind::Tcp,
        lease_until_ms: 0,
        spec: TaskSpec::Tcp(TcpSpec {
            host: "127.0.0.1".into(),
            port: 9,
        }),
    };
    let token = CancellationToken::new();
    token.cancel();
    let assignment = LeaseAssignment::new(lease, token);

    let report = worker
        .handle(assignment)
        .await
        .expect("tcp worker cancellation");
    let (completed, cancelled) = report.into_parts();
    assert!(
        completed.is_empty(),
        "cancelled lease should not be completed"
    );
    assert_eq!(cancelled.len(), 1, "expected cancelled lease entry");
    let observations = &cancelled[0].observations;
    let summary = observations
        .iter()
        .find(|obs| obs.name == "tcp_summary")
        .expect("summary observation");
    assert_eq!(summary.value["cancelled"].as_bool(), Some(true));
});

#[derive(Clone, Default)]
struct TestReporter {
    inner: Arc<TestReporterInner>,
}

#[derive(Default)]
struct TestReporterInner {
    completed: Mutex<Vec<LeaseReport>>,
    cancelled: Mutex<Vec<LeaseReport>>,
    notify: Notify,
}

#[async_trait]
impl ReportSink for TestReporter {
    async fn report(&self, completed: Vec<LeaseReport>, cancelled: Vec<LeaseReport>) -> Result<()> {
        {
            let mut guard = self.inner.completed.lock().await;
            guard.extend(completed);
        }
        {
            let mut guard = self.inner.cancelled.lock().await;
            guard.extend(cancelled);
        }
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
        self.inner
            .completed
            .lock()
            .await
            .iter()
            .map(|entry| entry.lease_id)
            .collect()
    }

    async fn wait_for_cancelled(&self, expected: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let guard = self.inner.cancelled.lock().await;
                if guard.len() >= expected {
                    return;
                }
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for cancellations");
            }
            self.inner.notify.notified().await;
        }
    }

    async fn cancelled_async(&self) -> Vec<u64> {
        self.inner
            .cancelled
            .lock()
            .await
            .iter()
            .map(|entry| entry.lease_id)
            .collect()
    }

    async fn cancelled_reports_async(&self) -> Vec<LeaseReport> {
        self.inner.cancelled.lock().await.clone()
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

struct CancellableHandler {
    started: Notify,
    running: AtomicBool,
}

impl CancellableHandler {
    fn new() -> Self {
        Self {
            started: Notify::new(),
            running: AtomicBool::new(false),
        }
    }

    async fn wait_started(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.running.load(Ordering::SeqCst) {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for handler to start");
            }
            self.started.notified().await;
        }
    }
}

#[async_trait]
impl WorkerHandler for CancellableHandler {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let (lease, token) = assignment.into_parts();
        self.running.store(true, Ordering::SeqCst);
        self.started.notify_waiters();
        token.cancelled().await;
        self.running.store(false, Ordering::SeqCst);
        Ok(WorkerReport::cancelled(lease.lease_id).with_observation(
            lease.lease_id,
            Observation {
                name: "cancelled".into(),
                value: json!(true),
                unit: None,
            },
        ))
    }
}

#[async_trait]
impl WorkerHandler for LatchingHandler {
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let lease = assignment.lease();
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
        Ok(WorkerReport::completed(lease.lease_id))
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
    async fn handle(&self, assignment: LeaseAssignment) -> Result<WorkerReport> {
        let lease = assignment.lease();
        if !self.tripped.swap(true, Ordering::SeqCst) {
            panic!("intentional panic for testing");
        }
        Ok(WorkerReport::completed(lease.lease_id))
    }
}
