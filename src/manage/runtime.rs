//! Managed DNS lifecycle. Configuration transactions survive HTTP disconnects.
use super::{
    Active,
    store::{Store, Stored},
    transport::Snapshot,
};
use crate::{config::Config, metrics::Metrics, server::Server};
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
    grace: Duration,
    metrics: Arc<Metrics>,
    resolver: Arc<crate::resolver::Resolver>,
    listen: SocketAddr,
    started: Instant,
}

impl Running {
    fn start(server: Server, grace: u64, listen: SocketAddr) -> Self {
        let metrics = server.metrics().clone();
        let resolver = server.resolver().clone();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            grace: Duration::from_millis(grace + 2000),
            metrics,
            resolver,
            listen,
            started: Instant::now(),
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.send(());
        if timeout(self.grace, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

pub(super) struct Manager {
    pub store: Arc<Store>,
    pub saved: Option<Stored>,
    running: Option<Running>,
    pub last_error: Option<String>,
    generation: u64,
    address: SocketAddr,
    active: Arc<Mutex<Active>>,
}

impl Manager {
    pub async fn open(
        store: Store,
        address: SocketAddr,
        active: Arc<Mutex<Active>>,
    ) -> Result<Self> {
        let saved = store.read()?;
        if let Some(saved) = &saved {
            let config = Config::parse_in(&saved.toml, &store.dir)?;
            let snapshot = Snapshot::prepare(&config, address)
                .map_err(|error| anyhow!("management TLS stage failed: {error:#}"))?;
            active.lock().unwrap().snapshot = Arc::new(snapshot);
        }
        let mut this = Self {
            store: Arc::new(store),
            saved,
            running: None,
            last_error: None,
            generation: 0,
            address,
            active,
        };
        if let Some(saved) = this.saved.clone() {
            this.restore(&saved.toml).await;
        }
        Ok(this)
    }

    pub fn status(&self) -> serde_json::Value {
        let running = self.running.as_ref().filter(|r| !r.task.is_finished());
        serde_json::json!({
            "running": running.is_some(),
            "last_error": if self.running.is_some() && running.is_none() {
                Some("DNS task exited; reapply configuration to restart".to_string())
            } else { self.last_error.clone() },
            "revision": self.saved.as_ref().map_or(0, |s| s.revision),
            "metrics": running.map(|r| r.metrics.snapshot()),
            "listen": running.map(|r| r.listen.to_string()),
            "uptime_seconds": running.map(|r| r.started.elapsed().as_secs()),
            "generation": self.generation,
            "cache": running.map(|r| r.resolver.cache().snapshot()),
            "refresh": running.map(|r| r.resolver.refresh_snapshot()),
        })
    }

    pub fn transport_change(&self, toml: &str, request_host: &str) -> Result<Value> {
        let config = Config::parse_in(toml, &self.store.dir)?;
        let candidate = Snapshot::describe(&config, self.address)?;
        let current = self.active.lock().unwrap().snapshot.clone();
        Ok(current.change(&candidate, request_host, self.address))
    }

    pub fn transport(&self) -> Value {
        self.active.lock().unwrap().snapshot.view()
    }

    pub fn resolver(&self) -> Option<&Arc<crate::resolver::Resolver>> {
        self.running
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| &r.resolver)
    }

    /// Compare source documents, not the serialized compiled filter (which is
    /// deliberately omitted by Config). Any non-cache change needs a restart.
    pub fn cache_only(&self, next: &str) -> bool {
        let Some(previous) = self.saved.as_ref().filter(|_| self.resolver().is_some()) else {
            return false;
        };
        let split_cache = |text: &str| -> Option<(toml::Value, Option<toml::Value>)> {
            let mut value: toml::Value = toml::from_str(text).ok()?;
            let cache = value.as_table_mut()?.remove("cache");
            Some((value, cache))
        };
        matches!((split_cache(&previous.toml), split_cache(next)), (Some((a, old)), Some((b, new))) if a == b && old != new)
    }

    pub fn statistics_snapshot(&self) -> (u64, Option<crate::metrics::Snapshot>) {
        (
            self.generation,
            self.running
                .as_ref()
                .filter(|r| !r.task.is_finished())
                .map(|r| r.metrics.snapshot()),
        )
    }

    pub async fn validate(&self, toml: String) -> Result<Config> {
        Ok(self.prepare(toml).await?.0)
    }

    pub async fn prepare(&self, toml: String) -> Result<(Config, Snapshot)> {
        ensure!(toml.len() <= 256 * 1024, "configuration exceeds 256 KiB");
        let dir = self.store.dir.clone();
        let address = self.address;
        tokio::task::spawn_blocking(move || {
            let config = Config::parse_in(&toml, &dir)?;
            let snapshot = Snapshot::prepare(&config, address)?;
            config.check_files()?;
            Ok((config, snapshot))
        })
        .await?
    }

    async fn restore(&mut self, toml: &str) {
        let result = async {
            let config = self.validate(toml.to_owned()).await?;
            let grace = config.shutdown_grace_ms;
            let selected = self.active.lock().unwrap().snapshot.selected.clone();
            let server = Server::bind_with_identity(config, selected).await?;
            let listen = server.local_addr()?;
            Ok::<_, anyhow::Error>(Running::start(server, grace, listen))
        }
        .await;
        match result {
            Ok(running) => {
                self.generation = self.generation.saturating_add(1);
                self.running = Some(running);
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("DNS unavailable: {error}"));
            }
        }
    }

    /// Returns whether listener restart was necessary.
    pub async fn apply(&mut self, next: Stored) -> Result<bool> {
        let (config, candidate) = self.prepare(next.toml.clone()).await?;
        if self.cache_only(&next.toml) {
            // Manager's mutex serializes this transaction. There is no fallible
            // IO after persist and no await between live swap and revision update.
            let store = self.store.clone();
            let saved = next.clone();
            let resolver = self.resolver().expect("running cache-only target").clone();
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            resolver.replace_cache(config.cache);
            self.saved = Some(next);
            self.last_error = None;
            return Ok(false);
        }
        let grace = config.shutdown_grace_ms;
        let previous = self.saved.clone();
        self.stop().await;
        let result = async {
            let server = Server::bind_with_identity(config, candidate.selected.clone()).await?;
            let listen = server.local_addr()?;
            let store = self.store.clone();
            let saved = next.clone();
            // Persist only after all candidate sockets and material are prepared.
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            Ok::<_, anyhow::Error>(Running::start(server, grace, listen))
        }
        .await;
        match result {
            Ok(running) => {
                self.publish(candidate);
                self.generation = self.generation.saturating_add(1);
                self.running = Some(running);
                self.saved = Some(next);
                self.last_error = None;
                Ok(true)
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.restore(&previous.toml).await;
                }
                if self.running.is_none() {
                    self.last_error = Some(format!("Apply failed; DNS is stopped: {error}"));
                }
                Err(error.context("configuration not applied"))
            }
        }
    }

    fn publish(&self, mut candidate: Snapshot) {
        let mut active = self.active.lock().unwrap();
        if active.snapshot.changes_realm(&candidate) {
            active.sessions.clear();
            candidate.realm = active.snapshot.realm.saturating_add(1);
        } else {
            candidate.realm = active.snapshot.realm;
        }
        active.snapshot = Arc::new(candidate);
    }

    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }
}
