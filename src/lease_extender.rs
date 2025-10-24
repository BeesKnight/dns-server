use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::control_plane::{ControlPlaneClient, ControlPlaneTransport, ExtendOutcome, Lease};

#[derive(Clone, Debug)]
pub struct LeaseExtenderConfig {
    pub extend_every: Duration,
    pub extend_by: Duration,
}

#[derive(Clone)]
pub struct LeaseExtenderClient {
    tx: mpsc::Sender<Command>,
}

pub struct LeaseExtenderHandle {
    client: LeaseExtenderClient,
    join: JoinHandle<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseExtenderError {
    #[error("lease extender has shut down")]
    Closed,
}

struct LeaseEntry {
    task_id: u64,
    deadline: Instant,
    token: CancellationToken,
}

enum Command {
    Track {
        lease_id: u64,
        task_id: u64,
        deadline: Instant,
        token: CancellationToken,
    },
    Finish {
        lease_id: u64,
    },
    Revoke {
        lease_id: u64,
    },
    Shutdown,
}

pub struct LeaseExtendUpdate {
    pub outcomes: Vec<ExtendOutcome>,
    pub revoked: Vec<u64>,
}

#[async_trait]
pub trait LeaseExtendClient: Send + Sync + Clone + 'static {
    async fn extend_leases(
        &self,
        lease_ids: Vec<u64>,
        extend_by: Duration,
    ) -> Result<LeaseExtendUpdate>;
}

#[async_trait]
impl<T> LeaseExtendClient for ControlPlaneClient<T>
where
    T: ControlPlaneTransport + Send + Sync + 'static,
{
    async fn extend_leases(
        &self,
        lease_ids: Vec<u64>,
        extend_by: Duration,
    ) -> Result<LeaseExtendUpdate> {
        let outcomes = ControlPlaneClient::extend(self, lease_ids, extend_by)
            .await
            .map_err(|err| anyhow!(err))?;
        Ok(LeaseExtendUpdate {
            outcomes,
            revoked: Vec::new(),
        })
    }
}

impl LeaseExtenderHandle {
    pub fn client(&self) -> LeaseExtenderClient {
        self.client.clone()
    }

    pub async fn shutdown(self) {
        let _ = self.client.tx.send(Command::Shutdown).await;
        let _ = self.join.await;
    }
}

impl LeaseExtenderClient {
    pub async fn track(
        &self,
        lease: &Lease,
        token: &CancellationToken,
    ) -> Result<(), LeaseExtenderError> {
        let deadline = Instant::now() + Duration::from_millis(lease.lease_until_ms);
        self.send(Command::Track {
            lease_id: lease.lease_id,
            task_id: lease.task_id,
            deadline,
            token: token.clone(),
        })
        .await
    }

    pub async fn finish(&self, lease_id: u64) -> Result<(), LeaseExtenderError> {
        self.send(Command::Finish { lease_id }).await
    }

    pub async fn revoke(&self, lease_id: u64) -> Result<(), LeaseExtenderError> {
        self.send(Command::Revoke { lease_id }).await
    }

    async fn send(&self, command: Command) -> Result<(), LeaseExtenderError> {
        self.tx
            .send(command)
            .await
            .map_err(|_| LeaseExtenderError::Closed)
    }
}

pub fn spawn_lease_extender<C>(
    client: C,
    config: LeaseExtenderConfig,
) -> (LeaseExtenderHandle, LeaseExtenderClient)
where
    C: LeaseExtendClient,
{
    let (tx, rx) = mpsc::channel(128);
    let client_handle = LeaseExtenderClient { tx: tx.clone() };
    let runtime_client = client.clone();
    let join = tokio::spawn(run(runtime_client, config, rx));
    let handle = LeaseExtenderHandle {
        client: LeaseExtenderClient { tx },
        join,
    };
    (handle, client_handle)
}

async fn run<C>(client: C, config: LeaseExtenderConfig, mut rx: mpsc::Receiver<Command>)
where
    C: LeaseExtendClient,
{
    let mut entries: HashMap<u64, LeaseEntry> = HashMap::new();
    let mut task_index: HashMap<u64, u64> = HashMap::new();
    let mut ticker = interval(config.extend_every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(command) = rx.recv() => {
                match command {
                    Command::Track { lease_id, task_id, deadline, token } => {
                        let entry = LeaseEntry { task_id, deadline, token };
                        task_index.insert(task_id, lease_id);
                        entries.insert(lease_id, entry);
                    }
                    Command::Finish { lease_id } => {
                        if let Some(entry) = entries.remove(&lease_id) {
                            task_index.remove(&entry.task_id);
                        }
                    }
                    Command::Revoke { lease_id } => {
                        if let Some(entry) = entries.remove(&lease_id) {
                            entry.token.cancel();
                            task_index.remove(&entry.task_id);
                        }
                    }
                    Command::Shutdown => {
                        for (_, entry) in entries.drain() {
                            entry.token.cancel();
                        }
                        task_index.clear();
                        break;
                    }
                }
            }
            _ = ticker.tick(), if !entries.is_empty() => {
                if let Err(err) = extend_active(&client, &config, &mut entries, &mut task_index).await {
                    warn!(error = %err, "failed to extend leases");
                }
            }
            else => break,
        }

        cancel_expired(&mut entries, &mut task_index);
    }
}

async fn extend_active<C>(
    client: &C,
    config: &LeaseExtenderConfig,
    entries: &mut HashMap<u64, LeaseEntry>,
    task_index: &mut HashMap<u64, u64>,
) -> Result<()>
where
    C: LeaseExtendClient,
{
    let lease_ids: Vec<u64> = entries.keys().copied().collect();
    if lease_ids.is_empty() {
        return Ok(());
    }
    let update = client.extend_leases(lease_ids, config.extend_by).await?;
    let now = Instant::now();
    for outcome in update.outcomes {
        if let Some(entry) = entries.get_mut(&outcome.lease_id) {
            entry.deadline = now + Duration::from_millis(outcome.new_deadline_ms);
        }
    }
    for task_id in update.revoked {
        if let Some(lease_id) = task_index.remove(&task_id) {
            if let Some(entry) = entries.remove(&lease_id) {
                entry.token.cancel();
            }
        }
    }
    Ok(())
}

fn cancel_expired(entries: &mut HashMap<u64, LeaseEntry>, task_index: &mut HashMap<u64, u64>) {
    let now = Instant::now();
    let mut expired = Vec::new();
    for (&lease_id, entry) in entries.iter() {
        if entry.deadline <= now {
            expired.push((lease_id, entry.task_id, entry.token.clone()));
        }
    }
    for (lease_id, task_id, token) in expired {
        token.cancel();
        entries.remove(&lease_id);
        task_index.remove(&task_id);
    }
}
