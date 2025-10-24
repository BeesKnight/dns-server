mod support;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use support::{build_tasks, AgentPipeline, AgentPipelineConfig, MockBackend, TaskType};

include!(concat!(env!("OUT_DIR"), "/worker_test_macro.rs"));

worker_test!(batch_lease_protocol_is_respected, {
    let (backend, control, driver) = MockBackend::new();
    control.update_behavior(|behavior| {
        behavior.lease_duration = Duration::from_millis(80);
        behavior.heartbeat_timeout = Duration::from_millis(200);
    });

    let mut config = AgentPipelineConfig::new(8);
    config.extend_every = Duration::from_millis(30);
    config.extend_by = Duration::from_millis(120);
    config.heartbeat_interval = Duration::from_millis(40);
    config
        .processing_latency
        .insert(TaskType::Dns, Duration::from_millis(150));
    config
        .processing_latency
        .insert(TaskType::Http, Duration::from_millis(20));
    config
        .processing_latency
        .insert(TaskType::Tcp, Duration::from_millis(20));
    config
        .processing_latency
        .insert(TaskType::Ping, Duration::from_millis(20));
    config
        .processing_latency
        .insert(TaskType::Trace, Duration::from_millis(20));

    let mut counts = HashMap::new();
    for kind in TaskType::ALL {
        counts.insert(kind, 10);
    }
    let tasks = build_tasks(&counts);
    control.enqueue_many(tasks.clone()).await;

    let pipeline = AgentPipeline::spawn(backend.clone(), config.clone());
    pipeline
        .wait_for_total(tasks.len() as u64, Duration::from_secs(5))
        .await;
    let snapshot = control.snapshot().await;
    let stats = pipeline.shutdown().await;
    control.shutdown().await;
    let _ = driver.await;

    assert_eq!(stats.total_completed(), tasks.len() as u64);
    assert!(
        stats.total_extensions() > 0,
        "lease extensions must happen for slow DNS tasks"
    );
    assert!(
        snapshot.active_leases.is_empty(),
        "all leases should be reported"
    );
});

worker_test!(queue_distribution_and_backpressure, {
    let (backend, control, driver) = MockBackend::new();

    let mut config = AgentPipelineConfig::new(12);
    config.per_type_concurrency.insert(TaskType::Dns, 3);
    config.per_type_concurrency.insert(TaskType::Http, 2);
    config.per_type_concurrency.insert(TaskType::Tcp, 2);
    config.per_type_concurrency.insert(TaskType::Ping, 1);
    config.per_type_concurrency.insert(TaskType::Trace, 1);
    config
        .processing_latency
        .insert(TaskType::Dns, Duration::from_millis(80));
    config
        .processing_latency
        .insert(TaskType::Http, Duration::from_millis(40));
    config
        .processing_latency
        .insert(TaskType::Tcp, Duration::from_millis(40));
    config
        .processing_latency
        .insert(TaskType::Ping, Duration::from_millis(30));
    config
        .processing_latency
        .insert(TaskType::Trace, Duration::from_millis(50));

    let mut counts = HashMap::new();
    counts.insert(TaskType::Dns, 24);
    counts.insert(TaskType::Http, 16);
    counts.insert(TaskType::Tcp, 16);
    counts.insert(TaskType::Ping, 8);
    counts.insert(TaskType::Trace, 8);
    let tasks = build_tasks(&counts);
    control.enqueue_many(tasks.clone()).await;

    let pipeline = AgentPipeline::spawn(backend.clone(), config.clone());
    pipeline
        .wait_for_total(tasks.len() as u64, Duration::from_secs(5))
        .await;
    let stats = pipeline.shutdown().await;
    control.shutdown().await;
    let _ = driver.await;

    for (kind, expected) in &counts {
        assert_eq!(stats.per_type_completed(*kind), *expected as u64);
        let concurrency = match kind {
            TaskType::Dns => 3,
            TaskType::Http => 2,
            TaskType::Tcp => 2,
            TaskType::Ping => 1,
            TaskType::Trace => 1,
        };
        assert!(
            stats.peak_inflight(*kind) <= concurrency,
            "type {:?} exceeded concurrency budget: peak {} > {}",
            kind,
            stats.peak_inflight(*kind),
            concurrency
        );
    }
});

worker_test!(load_metrics_with_thousands_of_tasks, {
    let (backend, control, driver) = MockBackend::new();
    control.update_behavior(|behavior| {
        behavior.lease_duration = Duration::from_millis(120);
    });

    let mut config = AgentPipelineConfig::new(64);
    for kind in TaskType::ALL {
        config.per_type_concurrency.insert(kind, 8);
        config
            .processing_latency
            .insert(kind, Duration::from_millis(20));
    }
    config.extend_every = Duration::from_millis(40);
    config.extend_by = Duration::from_millis(80);

    let mut counts = HashMap::new();
    for kind in TaskType::ALL {
        counts.insert(kind, 2_000);
    }
    let tasks = build_tasks(&counts);
    control.enqueue_many(tasks.clone()).await;

    let pipeline = AgentPipeline::spawn(backend.clone(), config.clone());
    let stats_handle = pipeline.stats();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if stats_handle.total_completed() >= tasks.len() as u64 {
            let snapshot = control.snapshot().await;
            if snapshot.pending.values().all(|queue| queue.is_empty())
                && snapshot.active_leases.is_empty()
            {
                break;
            }
        }
        if Instant::now() > deadline {
            panic!("load test did not finish within deadline");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stats = pipeline.shutdown().await;
    let snapshot = control.snapshot().await;
    control.shutdown().await;
    let _ = driver.await;

    assert_eq!(stats.total_completed(), tasks.len() as u64);
    for kind in TaskType::ALL {
        let configured = config.per_type_concurrency.get(&kind).copied().unwrap_or(0);
        assert!(
            stats.peak_inflight(kind) <= configured,
            "kind {:?} exceeded stabilized window: peak {} > {}",
            kind,
            stats.peak_inflight(kind),
            configured
        );
        assert_eq!(
            stats.per_type_completed(kind),
            *counts.get(&kind).unwrap_or(&0) as u64,
            "kind {:?} did not respect throughput SLA",
            kind,
        );
    }
    assert!(snapshot.pending.values().all(|queue| queue.is_empty()));
    let histogram = stats.histogram();
    let p50 = Duration::from_micros(histogram.value_at_quantile(0.50));
    let p99 = Duration::from_micros(histogram.value_at_quantile(0.99));
    assert!(p50 < Duration::from_millis(50));
    assert!(p99 < Duration::from_millis(120));
    assert!(
        stats.max_leases() <= 64 + (TaskType::ALL.len() as u64 * 8),
        "unexpected growth in active leases"
    );
});

worker_test!(pipeline_handles_backend_cancellation, {
    let (backend, control, driver) = MockBackend::new();

    let mut config = AgentPipelineConfig::new(4);
    config.extend_every = Duration::from_millis(20);
    config.extend_by = Duration::from_millis(60);
    config
        .processing_latency
        .insert(TaskType::Dns, Duration::from_millis(100));
    config.per_type_concurrency.insert(TaskType::Dns, 1);

    let mut counts = HashMap::new();
    counts.insert(TaskType::Dns, 1);
    let tasks = build_tasks(&counts);
    control.enqueue_many(tasks.clone()).await;

    let pipeline = AgentPipeline::spawn(backend.clone(), config.clone());
    let stats_handle = pipeline.stats();

    tokio::time::sleep(Duration::from_millis(50)).await;
    control.update_behavior(|behavior| {
        behavior.revoked_tasks.insert(1);
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if stats_handle.total_cancelled() >= 1 {
            break;
        }
        if Instant::now() > deadline {
            panic!("cancellation was not observed");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stats = pipeline.shutdown().await;
    let snapshot = control.snapshot().await;
    control.shutdown().await;
    let _ = driver.await;

    assert_eq!(stats.total_cancelled(), 1, "expected one cancelled task");
    assert_eq!(stats.total_completed(), 0, "no task should complete");
    assert!(snapshot.active_leases.is_empty());
});
