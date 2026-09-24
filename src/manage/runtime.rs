//! Managed DNS lifecycle. Configuration transactions survive HTTP disconnects.
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
use super::{
    Active,
    store::{Store, Stored},
    transport::Snapshot,
};
use crate::runtime_health::State as HealthState;
use crate::{
    cache::persistence::SnapshotReport,
    cache_persistence::{CachePersistence, ConsumedSnapshot},
    config::Config,
    runtime_services::RuntimeServices,
    server::Server,
    storage::RuntimeSettings,
};
use anyhow::{Result, anyhow, ensure};
use futures_util::FutureExt;
use serde_json::Value;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

struct RuntimeCompletion {
    services: Arc<RuntimeServices>,
    resolver: Arc<crate::resolver::Resolver>,
    generation: u64,
    complete: bool,
}
impl Drop for RuntimeCompletion {
    fn drop(&mut self) {
        if !self.complete {
            self.resolver.force_shutdown();
            self.services.set_dns_health(
                HealthState::Failed,
                self.generation,
                Some("task_aborted"),
            );
        }
    }
}

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
    grace: Duration,
    config: Config,
    resolver: Arc<crate::resolver::Resolver>,
    listen: SocketAddr,
    doh_listen: Option<SocketAddr>,
    started: Instant,
}

impl Running {
    fn start(
        server: Server,
        config: Config,
        listen: SocketAddr,
        services: Arc<RuntimeServices>,
        generation: u64,
    ) -> Self {
        let grace = config.shutdown_grace_ms;
        let resolver = server.resolver().clone();
        let doh_listen = server.encrypted_addrs().ok().and_then(|addresses| {
            addresses
                .into_iter()
                .find(|(name, _)| *name == "doh")
                .map(|(_, address)| address)
        });
        let (stop, stopped) = oneshot::channel();
        services.set_dns_health(HealthState::Running, generation, None);
        let completion = RuntimeCompletion {
            services,
            resolver: resolver.clone(),
            generation,
            complete: false,
        };
        let task = tokio::spawn(async move {
            let mut completion = completion;
            let result = std::panic::AssertUnwindSafe(server.run(async {
                let _ = stopped.await;
            }))
            .catch_unwind()
            .await;
            let (result, code) = match result {
                Ok(Ok(())) if completion.resolver.is_quiescent() => (Ok(()), None),
                Ok(Ok(())) => (Ok(()), Some("drain_deadline")),
                Ok(Err(error)) => (Err(error), Some("listener_failed")),
                Err(_) => (Err(anyhow!("DNS task panicked")), Some("task_panic")),
            };
            if code.is_some() {
                completion.resolver.force_shutdown();
            }
            completion.services.set_dns_health(
                if code.is_some() {
                    HealthState::Failed
                } else {
                    HealthState::Stopped
                },
                generation,
                code,
            );
            completion.complete = true;
            result
        });
        Self {
            stop,
            task,
            grace: Duration::from_millis(grace + 2000),
            config,
            resolver,
            listen,
            doh_listen,
            started: Instant::now(),
        }
    }

    async fn stop(mut self) -> (Arc<crate::resolver::Resolver>, Config, bool) {
        let _ = self.stop.send(());
        let result = timeout(self.grace, &mut self.task).await;
        let clean = matches!(result, Ok(Ok(Ok(()))));
        if result.is_err() {
            self.resolver.force_shutdown();
            self.task.abort();
            let _ = self.task.await;
        }
        let quiescent = clean && self.resolver.is_quiescent();
        (self.resolver, self.config, quiescent)
    }
}

pub(super) struct Manager {
    pub services: Arc<RuntimeServices>,
    persistence: Arc<CachePersistence>,
    cache_report: SnapshotReport,
    cache_ready: bool,
    pub store: Arc<Store>,
    pub saved: Option<Stored>,
    running: Option<Running>,
    pub last_error: Option<String>,
    generation: u64,
    address: SocketAddr,
    active: Arc<Mutex<Active>>,
    last_reload: Option<Value>,
    reload_attempt: u64,
}

impl Manager {
    pub async fn open(
        store: Store,
        address: SocketAddr,
        active: Arc<Mutex<Active>>,
    ) -> Result<Self> {
        let saved = store.read()?;
        let config = saved
            .as_ref()
            .map(|saved| Config::parse_in(&saved.toml, &store.dir))
            .transpose()?;
        let settings = config
            .as_ref()
            .map(RuntimeSettings::from_config)
            .unwrap_or_default();
        let cache_settings = config
            .as_ref()
            .map(|c| c.cache.persistence.clone())
            .unwrap_or_default();
        let revision = saved.as_ref().map_or(0, |s| s.revision);
        let directory = store.dir.join("runtime");
        let (services, persistence) = tokio::task::spawn_blocking(move || {
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "managed runtime must be an owned directory, not a link"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let services = RuntimeServices::open(&directory, settings, revision)?;
            let persistence = Arc::new(CachePersistence::new(&directory));
            Ok::<_, anyhow::Error>((services, persistence))
        })
        .await??;
        let consumed = crate::runtime_lifecycle::consume(persistence.clone(), cache_settings).await;
        let cache_ready = consumed.is_ok();
        let cache_report = match &consumed {
            Ok(snapshot) => snapshot.report().clone(),
            Err(error) => SnapshotReport {
                reason: Some(format!("clean snapshot consumption failed: {error:#}")),
                ..Default::default()
            },
        };
        if let Some(config) = config {
            let snapshot = tokio::task::spawn_blocking(move || Snapshot::prepare(&config, address))
                .await?
                .map_err(|error| anyhow!("management TLS stage failed: {error:#}"))?;
            active.lock().unwrap().snapshot = Arc::new(snapshot);
        }
        let mut this = Self {
            services,
            persistence,
            cache_report,
            cache_ready,
            store: Arc::new(store),
            saved,
            running: None,
            last_error: None,
            generation: 0,
            address,
            active,
            last_reload: None,
            reload_attempt: 0,
        };
        if let Some(saved) = this.saved.clone() {
            this.restore(&saved.toml, consumed.ok()).await;
        }
        Ok(this)
    }

    pub fn status(&self) -> serde_json::Value {
        let health = self.services.health.snapshot();
        let running = self
            .running
            .as_ref()
            .filter(|r| health.ready && !r.task.is_finished());
        serde_json::json!({
            "running": running.is_some(),
            "last_error": if self.running.is_some() && running.is_none() {
                Some("DNS task exited; reapply configuration to restart".to_string())
            } else { self.last_error.clone() },
            "revision": self.saved.as_ref().map_or(0, |s| s.revision),
            "metrics": self.services.metrics.snapshot(),
            "metrics_scope": "process",
            "dns_health": health,
            "diagnostics": {"cache":running.map(|r| r.resolver.cache().diagnostics_snapshot()),"quic":self.services.metrics.quic.snapshot(),"upstreams":running.map(|r| r.resolver.upstream_diagnostics())},
            "certificates": self.certificate_status(),
            "storage": self.services.status(),
            "cache_persistence": self.cache_report,
            "listen": running.map(|r| r.listen.to_string()),
            "doh_listen": running.and_then(|r| r.doh_listen).map(|a| a.to_string()),
            "doh_http3": running.is_some_and(|r| r.config.doh.as_ref().is_some_and(|d| d.http3)),
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

    fn certificate_status(&self) -> Value {
        let certificates = self.active.lock().unwrap().snapshot.certificates.clone();
        let mut value = certificates
            .map(|set| serde_json::json!(set.summary()))
            .unwrap_or_else(|| serde_json::json!({"certificate_generation":0,"roles":[]}));
        value["last_reload"] = serde_json::json!(self.last_reload);
        value
    }

    /// Serialized by the existing mutation permit and Manager lock, including
    /// while DNS is stopped. The management listener owns the same active set.
    pub async fn reload_certificates(&mut self, source: &'static str) -> Result<Value> {
        let revision = self.saved.as_ref().map_or(0, |saved| saved.revision);
        self.reload_attempt = self.reload_attempt.saturating_add(1);
        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64
        };
        let started = now();
        let certificates = self.active.lock().unwrap().snapshot.certificates.clone();
        let result = if let Some(certificates) = certificates {
            let prepare = certificates.clone();
            match tokio::task::spawn_blocking(move || prepare.prepare_reload()).await {
                Ok(Ok(prepared)) => certificates.publish(prepared),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(anyhow!("certificate preparation worker failed")),
            }
        } else {
            Ok(false)
        };
        let outcome = match &result {
            Ok(true) => "applied",
            Ok(false) => "unchanged",
            Err(_) => "failed",
        };
        let error_code = result.as_ref().err().map(|error| {
            if error.is::<crate::tls::ManagementNameMismatch>() {
                "CERTIFICATE_NAME_MISMATCH"
            } else {
                "CERTIFICATE_INVALID"
            }
        });
        self.last_reload = Some(
            serde_json::json!({"attempt_id":self.reload_attempt,"source":source,"started_at_ms":started,"completed_at_ms":now(),"outcome":outcome,"error_code":error_code}),
        );
        result.map_err(|error| {
            if error.is::<crate::tls::ManagementNameMismatch>() {
                error
            } else {
                super::transport::CertificateInvalid(error).into()
            }
        })?;
        let status = self.certificate_status();
        Ok(
            serde_json::json!({"outcome":outcome,"revision":revision,"certificate_generation":status["certificate_generation"],"certificates":status}),
        )
    }

    pub fn resolver(&self) -> Option<&Arc<crate::resolver::Resolver>> {
        self.running
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| &r.resolver)
    }

    /// Compare source documents, not the serialized compiled filter (which is
    /// deliberately omitted by Config). Any non-cache change needs a restart.
    pub fn hot_update(&self, next: &str) -> Option<bool> {
        let previous = self.saved.as_ref().filter(|_| self.resolver().is_some())?;
        let split_cache = |text: &str| -> Option<(toml::Value, serde_json::Value, toml::Value)> {
            let mut value: toml::Value = toml::from_str(text).ok()?;
            let original = value.clone();
            for field in ["storage", "query_log", "statistics"] {
                value.as_table_mut()?.remove(field);
            }
            value.as_table_mut()?.remove("cache");
            let mut cache = serde_json::to_value(Config::parse(text).ok()?.cache).ok()?;
            cache.as_object_mut()?.remove("persistence");
            Some((value, cache, original))
        };
        match (split_cache(&previous.toml), split_cache(next)) {
            (Some((a, old, original)), Some((b, new, candidate)))
                if a == b && original != candidate =>
            {
                Some(old != new)
            }
            _ => None,
        }
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
            config.check_non_identity_files()?;
            Ok((config, snapshot))
        })
        .await?
    }

    async fn restore(&mut self, toml: &str, snapshot: Option<ConsumedSnapshot>) {
        let result = async {
            ensure!(
                self.cache_ready,
                "cannot start DNS before clean snapshot is durably consumed"
            );
            let config = Config::parse_in(toml, &self.store.dir)?;
            let certificates = self.active.lock().unwrap().snapshot.certificates.clone();
            let server =
                Server::bind_with_services(config.clone(), certificates, self.services.clone())
                    .await?;
            if let Some(snapshot) = snapshot {
                let cache = server.resolver().cache();
                let fingerprint = crate::cache::persistence::semantic_fingerprint(
                    &config,
                    &server.resolver().policy_digest(),
                )?;
                self.cache_report =
                    crate::runtime_lifecycle::restore(snapshot, cache, fingerprint).await?;
            }
            let listen = server.local_addr()?;
            self.generation = self.generation.saturating_add(1);
            Ok::<_, anyhow::Error>(Running::start(
                server,
                config,
                listen,
                self.services.clone(),
                self.generation,
            ))
        }
        .await;
        match result {
            Ok(running) => {
                self.running = Some(running);
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("DNS unavailable: {error}"));
                self.services.set_dns_health(
                    HealthState::Failed,
                    self.generation,
                    Some("startup_failed"),
                );
            }
        }
    }

    /// Returns whether listener restart was necessary.
    pub async fn apply(&mut self, next: Stored) -> Result<bool> {
        ensure!(
            self.cache_ready,
            "cannot start DNS before clean snapshot is durably consumed; restart after resolving the storage error"
        );
        let (config, candidate) = self.prepare(next.toml.clone()).await?;
        if let Some(replace_cache) = self.hot_update(&next.toml) {
            // Manager's mutex serializes this transaction. There is no fallible
            // IO after persist and no await between live swap and revision update.
            let store = self.store.clone();
            let saved = next.clone();
            let resolver = self.resolver().expect("running cache-only target").clone();
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            if replace_cache {
                resolver.replace_cache(config.cache.clone());
            }
            self.services
                .publish_settings(RuntimeSettings::from_config(&config), next.revision);
            self.running
                .as_mut()
                .expect("running hot-update target")
                .config = config;
            self.saved = Some(next);
            self.last_error = None;
            return Ok(false);
        }
        let previous = self.saved.clone();
        self.stop().await;
        let result = async {
            let server = Server::bind_with_services(
                config.clone(),
                candidate.certificates.clone(),
                self.services.clone(),
            )
            .await?;
            let listen = server.local_addr()?;
            let store = self.store.clone();
            let saved = next.clone();
            // Persist only after all candidate sockets and material are prepared.
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            self.services
                .publish_settings(RuntimeSettings::from_config(&config), next.revision);
            self.generation = self.generation.saturating_add(1);
            Ok::<_, anyhow::Error>(Running::start(
                server,
                config,
                listen,
                self.services.clone(),
                self.generation,
            ))
        }
        .await;
        match result {
            Ok(running) => {
                self.publish(candidate);
                self.running = Some(running);
                self.saved = Some(next);
                self.last_error = None;
                Ok(true)
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.restore(&previous.toml, None).await;
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
            let _ = running.stop().await;
        }
        self.services.set_dns_state(false, self.generation);
    }

    /// Called once after management mutations are closed and drained.
    pub async fn terminal_shutdown(&mut self) {
        let stopped = match self.running.take() {
            Some(running) => Some(running.stop().await),
            None => None,
        };
        self.services.set_dns_state(false, self.generation);
        self.cache_report = crate::runtime_lifecycle::terminal(
            self.services.clone(),
            self.persistence.clone(),
            stopped,
        )
        .await;
    }
}
