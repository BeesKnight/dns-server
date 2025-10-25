mod support;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use dns_agent::control_plane::{
    ClaimRequest, ClaimResponse, ControlPlaneClient, ControlPlaneError, ControlPlaneTransport,
    ExtendOutcome, ExtendRequest, HeartbeatRequest, HeartbeatResponse, LeaseReport,
    RegisterRequest, RegisterResponse, ReportRequest, ReportResponse, TaskKind, TaskSpec,
    TransportError,
};
use support::{build_tasks, MockBackend, TaskType};
use tokio::time::Instant as TokioInstant;

#[derive(Clone)]
struct MockBackendTransport {
    backend: MockBackend,
}

impl MockBackendTransport {
    fn new(backend: MockBackend) -> Self {
        Self { backend }
    }
}

fn duration_to_millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

fn deadline_to_millis(deadline: TokioInstant) -> u64 {
    deadline
        .checked_duration_since(TokioInstant::now())
        .unwrap_or_default()
        .as_millis() as u64
}

#[async_trait]
impl ControlPlaneTransport for MockBackendTransport {
    async fn register(
        &self,
        request: &RegisterRequest,
    ) -> Result<RegisterResponse, TransportError> {
        let _ = request.hostname.clone();
        let registration = self
            .backend
            .register()
            .await
            .map_err(|err| TransportError::Backend(err.to_string()))?;
        Ok(RegisterResponse {
            agent_id: registration.agent_id.0,
            lease_duration_ms: duration_to_millis(registration.lease_duration),
            heartbeat_timeout_ms: duration_to_millis(registration.heartbeat_timeout),
            auth_token: registration.auth_token.clone(),
        })
    }

    async fn heartbeat(
        &self,
        request: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, TransportError> {
        let ack = self
            .backend
            .heartbeat(support::AgentId(request.agent_id))
            .await
            .map_err(|err| TransportError::Backend(err.to_string()))?;
        Ok(HeartbeatResponse {
            agent_id: ack.agent_id.0,
            next_deadline_ms: deadline_to_millis(ack.next_deadline),
        })
    }

    async fn claim(&self, request: &ClaimRequest) -> Result<ClaimResponse, TransportError> {
        let capacities = request
            .capacities
            .iter()
            .map(|(kind, capacity)| (into_task_type(*kind), *capacity))
            .collect();
        let claim = self
            .backend
            .claim(support::ClaimRequest {
                agent_id: support::AgentId(request.agent_id),
                capacities,
            })
            .await
            .map_err(|err| TransportError::Backend(err.to_string()))?;
        let leases = claim
            .leases
            .into_iter()
            .map(|lease| dns_agent::control_plane::Lease {
                lease_id: lease.lease_id,
                task_id: lease.task.id,
                kind: from_task_type(lease.task.kind),
                lease_until_ms: deadline_to_millis(lease.lease_until),
                spec: lease.task.spec.clone(),
            })
            .collect();
        Ok(ClaimResponse { leases })
    }

    async fn extend(&self, request: &ExtendRequest) -> Result<Vec<ExtendOutcome>, TransportError> {
        let outcomes = self
            .backend
            .extend(support::ExtendRequest {
                agent_id: support::AgentId(request.agent_id),
                lease_ids: request.lease_ids.clone(),
                extend_by: Duration::from_millis(request.extend_by_ms),
            })
            .await
            .map_err(|err| TransportError::Backend(err.to_string()))?;
        Ok(outcomes
            .into_iter()
            .map(|outcome| ExtendOutcome {
                lease_id: outcome.lease_id,
                new_deadline_ms: deadline_to_millis(outcome.new_deadline),
            })
            .collect())
    }

    async fn report(&self, request: &ReportRequest) -> Result<ReportResponse, TransportError> {
        self.backend
            .report(support::ReportRequest {
                agent_id: support::AgentId(request.agent_id),
                completed: request.completed.clone(),
                cancelled: request.cancelled.clone(),
            })
            .await
            .map_err(|err| TransportError::Backend(err.to_string()))?;
        Ok(ReportResponse {
            acknowledged: request.completed.len() + request.cancelled.len(),
        })
    }
}

fn into_task_type(kind: TaskKind) -> TaskType {
    match kind {
        TaskKind::Dns => TaskType::Dns,
        TaskKind::Http => TaskType::Http,
        TaskKind::Tcp => TaskType::Tcp,
        TaskKind::Ping => TaskType::Ping,
        TaskKind::Trace => TaskType::Trace,
    }
}

fn from_task_type(kind: TaskType) -> TaskKind {
    match kind {
        TaskType::Dns => TaskKind::Dns,
        TaskType::Http => TaskKind::Http,
        TaskType::Tcp => TaskKind::Tcp,
        TaskType::Ping => TaskKind::Ping,
        TaskType::Trace => TaskKind::Trace,
    }
}

#[tokio::test]
async fn client_handles_lease_lifecycle() -> Result<(), ControlPlaneError> {
    let (backend, control, driver) = MockBackend::new();
    let transport = MockBackendTransport::new(backend.clone());
    let client = ControlPlaneClient::new(transport);

    let mut queue = HashMap::new();
    queue.insert(TaskType::Dns, 4);
    control.enqueue_many(build_tasks(&queue)).await;

    let registration = client
        .register(RegisterRequest {
            hostname: Some("test-agent".into()),
        })
        .await?;
    assert_eq!(registration.agent_id, 1);

    let mut capacities = HashMap::new();
    capacities.insert(TaskKind::Dns, 2);
    let batch = client.claim(&capacities).await?;
    assert_eq!(batch.leases.len(), 2);
    assert!(matches!(batch.leases[0].spec, TaskSpec::Dns { .. }));

    let snapshot = client.snapshot().await;
    assert_eq!(snapshot.leases.len(), 2);

    let lease_ids: Vec<u64> = batch.leases.iter().map(|lease| lease.lease_id).collect();
    let outcomes = client
        .extend(lease_ids.clone(), Duration::from_millis(50))
        .await?;
    assert_eq!(outcomes.len(), lease_ids.len());

    let completed: Vec<LeaseReport> = lease_ids
        .iter()
        .copied()
        .map(|lease_id| LeaseReport {
            lease_id,
            observations: Vec::new(),
        })
        .collect();
    let report = client.report(completed, Vec::new()).await?;
    assert_eq!(report.acknowledged, lease_ids.len());

    let heartbeat = client.heartbeat().await?;
    assert_eq!(heartbeat.agent_id, registration.agent_id);

    control.shutdown().await;
    let _ = driver.await;
    Ok(())
}
