//! Fixed, request-owned connection slots; no worker or unbounded endpoint map.
use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, SemaphorePermit};

use super::ClientStream;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    pub enabled: bool,
    /// Per TLS endpoint, shared across its resolved addresses and client clones.
    pub max_connections: usize,
    /// Checked on checkout, without an idle reaper task.
    pub idle_timeout_ms: u64,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_connections: 8,
            idle_timeout_ms: 30_000,
        }
    }
}

impl PoolSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.max_connections),
            "upstreams.dot_pool.max_connections must be in 1..=256"
        );
        ensure!(
            (1..=600_000).contains(&self.idle_timeout_ms),
            "upstreams.dot_pool.idle_timeout_ms must be in 1..=600000"
        );
        Ok(())
    }
}

pub(super) struct Idle {
    pub stream: ClientStream,
    pub address: SocketAddr,
    pub returned: tokio::time::Instant,
}

pub(super) struct Pool {
    slots: Mutex<Vec<Slot>>,
    available: Semaphore,
    pub idle_timeout: Duration,
}

struct Slot {
    busy: bool,
    idle: Option<Idle>,
}

pub(super) struct Lease<'a> {
    pool: &'a Pool,
    index: usize,
    idle: Option<Idle>,
    // Released after Drop has published the free slot.
    _permit: SemaphorePermit<'a>,
}

impl Deref for Lease<'_> {
    type Target = Option<Idle>;

    fn deref(&self) -> &Self::Target {
        &self.idle
    }
}

impl DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.idle
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut slots = self.pool.slots.lock().unwrap();
        slots[self.index].idle = self.idle.take();
        slots[self.index].busy = false;
    }
}

impl Pool {
    pub fn new(settings: &PoolSettings) -> Self {
        Self {
            slots: Mutex::new(
                (0..settings.max_connections)
                    .map(|_| Slot {
                        busy: false,
                        idle: None,
                    })
                    .collect(),
            ),
            available: Semaphore::new(settings.max_connections),
            idle_timeout: Duration::from_millis(settings.idle_timeout_ms),
        }
    }

    pub async fn checkout(&self, address: SocketAddr) -> Lease<'_> {
        // Queue for any available slot, not a particular busy connection. The
        // semaphore is never closed; cancellation while waiting consumes nothing.
        let permit = self.available.acquire().await.unwrap();
        let mut slots = self.slots.lock().unwrap();
        let index = slots
            .iter()
            .position(|slot| {
                !slot.busy
                    && slot
                        .idle
                        .as_ref()
                        .is_some_and(|idle| idle.address == address)
            })
            .or_else(|| slots.iter().position(|slot| !slot.busy))
            .expect("an acquired permit owns one available slot");
        slots[index].busy = true;
        Lease {
            pool: self,
            index,
            idle: slots[index].idle.take(),
            _permit: permit,
        }
    }
}
