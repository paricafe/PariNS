//! Managed DNS lifecycle. Configuration transactions survive HTTP disconnects.
use super::store::{Store, Stored};
use crate::{config::Config, metrics::Metrics, server::Server};
use anyhow::{Result, anyhow, ensure};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
    grace: Duration,
    metrics: Arc<Metrics>,
    listen: SocketAddr,
    started: Instant,
}

impl Running {
    fn start(server: Server, grace: u64, listen: SocketAddr) -> Self {
        let metrics = server.metrics().clone();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            grace: Duration::from_millis(grace + 2000),
            metrics,
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
}

impl Manager {
    pub async fn open(store: Store) -> Result<Self> {
        let saved = store.read()?;
        let mut this = Self {
            store: Arc::new(store),
            saved,
            running: None,
            last_error: None,
            generation: 0,
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
        })
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
        ensure!(toml.len() <= 256 * 1024, "configuration exceeds 256 KiB");
        let dir = self.store.dir.clone();
        tokio::task::spawn_blocking(move || {
            let config = Config::parse_in(&toml, &dir)?;
            config.check_files()?;
            Ok(config)
        })
        .await?
    }

    async fn restore(&mut self, toml: &str) {
        let result = async {
            let config = self.validate(toml.to_owned()).await?;
            let grace = config.shutdown_grace_ms;
            let server = Server::bind(config).await?;
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

    pub async fn apply(&mut self, next: Stored) -> Result<()> {
        let config = self.validate(next.toml.clone()).await?;
        let grace = config.shutdown_grace_ms;
        let previous = self.saved.clone();
        self.stop().await;
        let result = async {
            let server = Server::bind(config).await?;
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
                self.generation = self.generation.saturating_add(1);
                self.running = Some(running);
                self.saved = Some(next);
                self.last_error = None;
                Ok(())
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.restore(&previous.toml).await;
                }
                if self.running.is_none() {
                    self.last_error = Some(format!("Apply failed; DNS is stopped: {error}"));
                }
                Err(anyhow!("configuration not applied: {error}"))
            }
        }
    }

    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }
}
