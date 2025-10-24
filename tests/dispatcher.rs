use std::time::Duration;

use codecrafters_dns_server::control_plane::{
    Lease, PingSpec, TaskKind, TaskSpec, TcpSpec, TraceSpec,
};
use codecrafters_dns_server::dispatcher::{
    spawn_dispatcher, DispatchQueues, DispatcherConfig, LeaseAssignment,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn test_assignment(id: u64, kind: TaskKind) -> LeaseAssignment {
    let lease = Lease {
        lease_id: id,
        task_id: id,
        kind,
        lease_until_ms: 0,
        spec: dummy_spec(kind, id),
    };
    LeaseAssignment::new(lease, CancellationToken::new())
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
            port: 80,
        }),
        TaskKind::Ping => TaskSpec::Ping(PingSpec {
            host: "127.0.0.1".into(),
            count: Some(3),
        }),
        TaskKind::Trace => TaskSpec::Trace(TraceSpec {
            host: "127.0.0.1".into(),
            max_hops: Some(4),
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_backpressure_recovers_when_capacity_frees() {
    let config = DispatcherConfig {
        dispatch_capacity: 4,
        dns_queue_capacity: 1,
        http_queue_capacity: 1,
        tcp_queue_capacity: 1,
        ping_queue_capacity: 1,
        trace_queue_capacity: 1,
    };

    let (dispatcher, queues, handle) = spawn_dispatcher(config);
    let mut backpressure = dispatcher.subscribe_backpressure();

    // initial state is not backpressured
    assert!(!*backpressure.borrow());

    dispatcher
        .dispatch(test_assignment(1, TaskKind::Dns))
        .await
        .expect("first lease accepted");

    let dispatcher_clone = dispatcher.clone();
    let send_task = tokio::spawn(async move {
        dispatcher_clone
            .dispatch(test_assignment(2, TaskKind::Dns))
            .await
            .expect("second lease accepted");
    });

    // wait for the dispatcher to apply backpressure once the queue is full
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if *backpressure.borrow() {
            break;
        }
        if backpressure.changed().await.is_err() {
            panic!("backpressure channel closed unexpectedly");
        }
        if Instant::now() > deadline {
            panic!("dispatcher did not signal backpressure in time");
        }
    }

    let DispatchQueues {
        mut dns,
        http,
        tcp,
        ping,
        trace,
    } = queues;
    // drain the first lease which should allow the second dispatch to proceed
    let first = dns.recv().await.expect("dns queue closed unexpectedly");
    assert_eq!(first.assignment().lease().lease_id, 1);
    let _ = first.into_assignment();

    send_task.await.expect("dispatch task joined");

    // second lease should now be available in the queue
    let second = dns
        .recv()
        .await
        .expect("dns queue closed before second lease");
    assert_eq!(second.assignment().lease().lease_id, 2);
    let _ = second.into_assignment();

    // Wait for backpressure to clear once there is capacity again.
    let deadline = Instant::now() + Duration::from_secs(1);
    while *backpressure.borrow() {
        if backpressure.changed().await.is_err() {
            break;
        }
        if Instant::now() > deadline {
            panic!("backpressure did not clear");
        }
    }
    assert!(!*backpressure.borrow());

    drop(dispatcher);
    drop(dns);
    drop(http);
    drop(tcp);
    drop(ping);
    drop(trace);

    handle.shutdown().await;
}
