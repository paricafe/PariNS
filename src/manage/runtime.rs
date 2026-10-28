//! Managed DNS lifecycle. Configuration transactions survive HTTP disconnects.
//! HTTP management survives unavailable DNS identities; saved DNS startup owns
//! their retry and publishes its CertificateSet after binding. HTTPS management
//! must prepare its identity before serving.
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
use super::{
    Active,
    store::{MAX_CONFIG, Store, Stored},
    transport::Snapshot,
};
use crate::resolver::ForcedShutdown;
use crate::runtime_health::State as HealthState;
use crate::{
    cache::persistence::SnapshotReport,
    cache_persistence::{CachePersistence, ConsumedSnapshot},
    config::{CacheConfig, Config},
    runtime_services::RuntimeServices,
    server::Server,
    storage::RuntimeSettings,
};
use anyhow::{Context, Result, anyhow, ensure};
use futures_util::FutureExt;
use serde_json::Value;
use std::{
    net::SocketAddr,
    path::PathBuf,
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
            self.resolver.force_shutdown(ForcedShutdown::TaskAborted);
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
    listeners: Arc<crate::server::Listeners>,
    started: Instant,
}

impl Running {
    fn start(
        mut server: Server,
        config: Config,
        listen: SocketAddr,
        services: Arc<RuntimeServices>,
        generation: u64,
    ) -> Self {
        let grace = config.shutdown_grace_ms;
        let resolver = server.resolver().clone();
        let doh_listen = server
            .encrypted_addrs()
            .into_iter()
            .find(|(name, _)| *name == "doh")
            .map(|(_, address)| address);
        let (stop, stopped) = oneshot::channel();
        // The server publishes ready or degraded for this generation as its
        // listeners come and go; this owner publishes only the terminal state.
        server.set_generation(generation);
        server.publish_health();
        let listeners = server.listeners();
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
            match code {
                Some("listener_failed") => completion
                    .resolver
                    .force_shutdown(ForcedShutdown::ListenerFailed),
                Some("task_panic") => completion
                    .resolver
                    .force_shutdown(ForcedShutdown::TaskPanic),
                // drain_deadline: a listener already recorded its own cause.
                _ => {}
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
            listeners,
            started: Instant::now(),
        }
    }

    async fn stop(mut self) -> (Arc<crate::resolver::Resolver>, Config, bool) {
        let _ = self.stop.send(());
        let result = timeout(self.grace, &mut self.task).await;
        let clean = matches!(result, Ok(Ok(Ok(()))));
        if result.is_err() {
            self.resolver
                .force_shutdown(ForcedShutdown::ManagedStopTimeout);
            self.task.abort();
            let _ = self.task.await;
        }
        let quiescent = clean && self.resolver.is_quiescent();
        (self.resolver, self.config, quiescent)
    }
}

pub(super) struct Manager {
    pub filters: Arc<crate::filter_subscriptions::service::Service>,
    pub updates: Arc<super::updates::Coordinator>,
    pub services: Arc<RuntimeServices>,
    persistence: Arc<CachePersistence>,
    cache_report: SnapshotReport,
    cache_ready: bool,
    pub store: Arc<Store>,
    pub saved: Option<Stored>,
    /// Turnstile keys, published after their own Store write.
    pub turnstile: super::turnstile::State,
    running: Option<Running>,
    pub last_error: Option<String>,
    generation: u64,
    address: SocketAddr,
    active: Arc<Mutex<Active>>,
    last_reload: Option<Value>,
    reload_attempt: u64,
    startup: Option<StartupFailure>,
    /// The saved local rules could not be compiled at startup, so the filter
    /// service holds an empty placeholder. No start, retry or rollback restore
    /// may serve it; only applying a configuration whose rules compile clears it.
    local_error: Option<String>,
    /// Why the saved rollback copy cannot be used. Any apply replaces that copy.
    pub rollback_error: Option<String>,
}

/// Subscription fingerprints of the saved rollback copy. Startup needs them,
/// so candidate preflight rejects a copy this version cannot read.
pub(super) fn previous_fingerprints(saved: &Stored, dir: &std::path::Path) -> Result<Vec<String>> {
    let Some(text) = &saved.previous else {
        return Ok(Vec::new());
    };
    Config::parse_in(text, dir)
        .and_then(|config| config.filter_subscriptions.fingerprints())
        .context("saved rollback configuration (previous revision) is invalid")
}

/// How long a validation waits for the subscription worker, within the
/// management connection's 90 s limit.
const VALIDATION_WORKER_WAIT: Duration = Duration::from_secs(60);

/// A candidate's checks that run without the Manager lock.
pub(super) struct Validation {
    dir: PathBuf,
    address: SocketAddr,
    filters: Arc<crate::filter_subscriptions::service::Service>,
    revision: u64,
    updates_only: bool,
}

impl Validation {
    pub async fn run(self, toml: String) -> Result<Config> {
        if self.updates_only {
            return Config::parse_in(&toml, &self.dir);
        }
        let config = prepare(self.dir, self.address, toml).await?.0;
        let local = config.local_policy_source();
        let _candidate = self
            .filters
            .check_config_from_source(
                config.filter_subscriptions.clone(),
                local,
                self.revision,
                VALIDATION_WORKER_WAIT,
            )
            .await?;
        Ok(config)
    }
}

async fn prepare(dir: PathBuf, address: SocketAddr, toml: String) -> Result<(Config, Snapshot)> {
    ensure!(toml.len() <= MAX_CONFIG, "configuration exceeds 256 KiB");
    tokio::task::spawn_blocking(move || {
        let config = Config::parse_in(&toml, &dir)?;
        super::store::check_rule_file(&dir, &config)?;
        let snapshot = Snapshot::prepare(&config, address)?;
        config.check_non_identity_files()?;
        Ok((config, snapshot))
    })
    .await?
}

/// The saved configuration failed to start (for example a port held by another
/// program). `dns_retry` restarts it with bounded backoff until it succeeds, a
/// new configuration is applied, or management shuts down.
struct StartupFailure {
    attempts: u32,
    failed_at: Instant,
    log: crate::server::FailureLog,
}

impl Manager {
    pub async fn open(
        store: Store,
        address: SocketAddr,
        active: Arc<Mutex<Active>>,
    ) -> Result<Self> {
        let update_dir = store.dir.clone();
        let updates = Arc::new(
            tokio::task::spawn_blocking(move || super::updates::Coordinator::open(&update_dir))
                .await?,
        );
        let saved = store.read()?;
        let turnstile = super::turnstile::State::load(&store.dir)?;
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
        let filter_settings = config
            .as_ref()
            .map(|c| c.filter_subscriptions.clone())
            .unwrap_or_default();
        let local = config.as_ref().map(Config::load_policy).transpose();
        let local_error = local.as_ref().err().map(ToString::to_string);
        // An unreadable rollback copy (for example after a downgrade) leaves
        // management and the saved revision available; only rollback is not.
        let (previous, rollback_error) = match saved
            .as_ref()
            .map(|saved| previous_fingerprints(saved, &store.dir))
            .transpose()
        {
            Ok(previous) => (previous.unwrap_or_default(), None),
            Err(error) => {
                eprintln!("PariNS rollback unavailable: {error:#}");
                (Vec::new(), Some(format!("{error:#}")))
            }
        };
        // A broken local file must keep management available; Manager keeps
        // local_error so no start path ever serves this placeholder policy.
        let local = local.ok().flatten().unwrap_or_default();
        use crate::filter_subscriptions::service::Service as Filters;
        let filters = Filters::open(
            store.dir.join("filter-subscriptions"),
            local,
            filter_settings,
            revision,
            previous,
            updates.frozen.load(std::sync::atomic::Ordering::Acquire),
        )
        .await?;
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
        let mut cache_report = match &consumed {
            Ok(snapshot) => snapshot.report().clone(),
            Err(error) => SnapshotReport {
                reason: Some(format!("clean snapshot consumption failed: {error:#}")),
                ..Default::default()
            },
        };
        let skip_restore = updates.skip_restore();
        if skip_restore && cache_ready {
            cache_report.reason = Some("software update recovery requires a cold cache".into());
        }
        if let Some(config) = config {
            let snapshot = tokio::task::spawn_blocking(move || {
                if config.doh.is_some() {
                    Snapshot::prepare(&config, address)
                } else {
                    match Snapshot::prepare(&config, address) {
                        Ok(snapshot) => Ok(snapshot),
                        Err(error) if error.is::<super::transport::CertificateInvalid>() => {
                            Snapshot::describe(&config, address)
                        }
                        Err(error) => Err(error),
                    }
                }
            })
            .await?
            .map_err(|error| anyhow!("management TLS stage failed: {error:#}"))?;
            active.lock().unwrap().snapshot = Arc::new(snapshot);
        }
        let mut this = Self {
            filters,
            updates,
            services,
            persistence,
            cache_report,
            cache_ready,
            store: Arc::new(store),
            saved,
            turnstile,
            running: None,
            last_error: None,
            generation: 0,
            address,
            active,
            last_reload: None,
            reload_attempt: 0,
            startup: None,
            local_error,
            rollback_error,
        };
        if let Some(saved) = this.saved.clone() {
            this.restore(&saved.toml, if skip_restore { None } else { consumed.ok() })
                .await;
        }
        Ok(this)
    }

    pub fn status(&self) -> serde_json::Value {
        let health = self.services.health.snapshot();
        // A degraded generation is running: its other listeners serve.
        let running = self.running.as_ref().filter(|r| {
            (health.ready || health.state == HealthState::Degraded) && !r.task.is_finished()
        });
        serde_json::json!({
            "running": running.is_some(),
            "last_error": if self.running.is_some() && running.is_none() {
                Some("DNS task exited; restarting automatically".to_string())
            } else { self.last_error.clone() },
            "revision": self.saved.as_ref().map_or(0, |s| s.revision),
            "rollback_unavailable": self.rollback_error,
            "resolution_mode": running.map(|r| r.resolver.resolution_mode()),
            "metrics": self.services.metrics.snapshot(),
            "metrics_scope": "process",
            "dns_health": health,
            "diagnostics": {"cache":running.map(|r| r.resolver.cache().diagnostics_snapshot()),"quic":self.services.metrics.quic.snapshot(),"upstreams":running.and_then(|r| r.resolver.upstream_diagnostics()),"recursion":running.and_then(|r| r.resolver.recursion_diagnostics())},
            "certificates": self.certificate_status(),
            "storage": self.services.storage.status(),
            "cache_persistence": self.cache_report,
            // Addresses are reported only for listeners that are serving.
            "listen": running
                .filter(|r| r.listeners.serving(crate::server::ListenerKind::Dns))
                .map(|r| r.listen.to_string()),
            "listeners": running.map(|r| r.listeners.snapshot()),
            "doh_listen": running
                .filter(|r| r.listeners.serving(crate::server::ListenerKind::Doh))
                .and_then(|r| r.doh_listen)
                .map(|a| a.to_string()),
            "doh_http3": running.is_some_and(|r| r.listeners.serving(crate::server::ListenerKind::Doh3)),
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
        self.reload_attempt = self.reload_attempt.saturating_add(1);
        let started = crate::clock::unix_ms();
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
            serde_json::json!({"attempt_id":self.reload_attempt,"source":source,"started_at_ms":started,"completed_at_ms":crate::clock::unix_ms(),"outcome":outcome,"error_code":error_code}),
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
            serde_json::json!({"outcome":outcome,"certificate_generation":status["certificate_generation"],"certificates":status}),
        )
    }

    pub fn resolver(&self) -> Option<&Arc<crate::resolver::Resolver>> {
        self.running
            .as_ref()
            .filter(|r| !r.task.is_finished())
            .map(|r| &r.resolver)
    }

    /// Management-only changes do not depend on live DNS or its material files.
    fn updates_only(&self, next: &str) -> Result<bool> {
        let Some(saved) = &self.saved else {
            return Ok(false);
        };
        let stripped = |text: &str| -> Result<toml::Value> {
            let mut value: toml::Value = toml::from_str(text)?;
            value
                .as_table_mut()
                .expect("configuration table")
                .remove("updates");
            Ok(value)
        };
        Ok(saved.toml != next && stripped(&saved.toml)? == stripped(next)?)
    }

    /// Compare source documents, not compiled rules. Updates-only changes also
    /// retain a stopped DNS state; cache/storage/filter changes require live DNS.
    pub fn hot_update(&self, next: &str) -> Option<bool> {
        if self.updates_only(next).ok()? {
            return Some(false);
        }
        self.cache_only_change(next, &Config::parse(next).ok()?.cache)
    }

    /// Whether `next` differs from the running configuration only in cache,
    /// storage, statistics, update or filter settings, and if so whether the
    /// cache itself changes. `next_cache` is the cache section parsed from `next`.
    fn cache_only_change(&self, next: &str, next_cache: &CacheConfig) -> Option<bool> {
        let running = self.running.as_ref().filter(|r| !r.task.is_finished())?;
        let previous = self.saved.as_ref()?;
        let split =
            |text: &str, cache: &CacheConfig| -> Option<(toml::Value, Value, toml::Value)> {
                let mut value: toml::Value = toml::from_str(text).ok()?;
                let original = value.clone();
                for field in [
                    "storage",
                    "query_log",
                    "statistics",
                    "updates",
                    "filter",
                    "filter_file",
                    "filter_subscriptions",
                    "cache",
                ] {
                    value.as_table_mut()?.remove(field);
                }
                let mut cache = serde_json::to_value(cache).ok()?;
                cache.as_object_mut()?.remove("persistence");
                Some((value, cache, original))
            };
        // The running generation's Config is the parsed saved configuration.
        match (
            split(&previous.toml, &running.config.cache),
            split(next, next_cache),
        ) {
            (Some((a, old, original)), Some((b, new, candidate)))
                if a == b && original != candidate =>
            {
                Some(old != new)
            }
            _ => None,
        }
    }

    /// Copies what validating `toml` needs, so the caller can release the
    /// Manager lock before certificates are read and rules compiled.
    pub fn validation(&self, toml: &str) -> Result<Validation> {
        ensure!(toml.len() <= MAX_CONFIG, "configuration exceeds 256 KiB");
        Ok(Validation {
            dir: self.store.dir.clone(),
            address: self.address,
            filters: self.filters.clone(),
            revision: self.saved.as_ref().map_or(0, |s| s.revision),
            updates_only: self.updates_only(toml)?,
        })
    }

    async fn restore(&mut self, toml: &str, snapshot: Option<ConsumedSnapshot>) {
        if let Some(error) = &self.local_error {
            // Retrying cannot help: the local rules are compiled only when a
            // configuration is applied. Keep the real reason visible.
            self.last_error = Some(format!("DNS unavailable: {error}"));
            return;
        }
        let result = async {
            ensure!(
                self.cache_ready,
                "cannot start DNS before clean snapshot is durably consumed"
            );
            let config = Config::parse_in(toml, &self.store.dir)?;
            ensure!(
                self.filters.ready(),
                "subscription_material_missing: DNS policy unavailable"
            );
            let current = self.active.lock().unwrap().snapshot.clone();
            // HTTP management can start while DNS identities are unavailable.
            // A successful DNS start publishes the same set its listeners use.
            let candidate = if current.certificates.is_none() {
                let config = config.clone();
                let address = self.address;
                Some(
                    tokio::task::spawn_blocking(move || Snapshot::prepare(&config, address))
                        .await??,
                )
            } else {
                None
            };
            let certificates = candidate.as_ref().unwrap_or(&current).certificates.clone();
            let server = Server::bind_with_policy(
                config.clone(),
                certificates,
                self.services.clone(),
                self.filters.handle(),
            )
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
            let listen = server.local_addr();
            self.generation = self.generation.saturating_add(1);
            Ok::<_, anyhow::Error>((
                Running::start(
                    server,
                    config,
                    listen,
                    self.services.clone(),
                    self.generation,
                ),
                candidate,
            ))
        }
        .await;
        match result {
            Ok((running, candidate)) => {
                if let Some(candidate) = candidate {
                    self.publish(candidate);
                }
                self.running = Some(running);
                self.last_error = None;
                self.startup_recovered();
            }
            Err(error) => {
                // The full chain names the listener, e.g. "bind DoH 0.0.0.0:443:
                // Address in use"; the outer message alone does not.
                let error = format!("{error:#}");
                self.last_error = Some(format!("DNS unavailable: {error}"));
                self.services.set_dns_health(
                    HealthState::Failed,
                    self.generation,
                    Some("startup_failed"),
                );
                self.startup_failed(error);
            }
        }
    }

    /// Counts the failure; logging is rate limited by [`crate::server::FailureLog`].
    fn startup_failed(&mut self, error: String) {
        let now = Instant::now();
        let failure = self.startup.get_or_insert_with(|| StartupFailure {
            attempts: 0,
            failed_at: now,
            log: Default::default(),
        });
        failure.attempts = failure.attempts.saturating_add(1);
        failure.failed_at = now;
        if failure.log.due(&error) {
            eprintln!("PariNS DNS unavailable: {error}; retrying automatically");
        }
    }

    /// A running generation that ended without a stop request (a panic, or a
    /// listener failure the Server could not ride out) is handed to `dns_retry`,
    /// which restarts the saved revision as after a failed start.
    pub fn reap_exited(&mut self) {
        let Some(running) = self.running.take_if(|running| running.task.is_finished()) else {
            return;
        };
        let reason = match running.task.now_or_never() {
            Some(Ok(Err(error))) => format!("DNS task exited: {error:#}"),
            Some(Ok(Ok(()))) => "DNS task exited without a stop request".into(),
            Some(Err(_)) | None => "DNS task aborted".into(),
        };
        self.last_error = Some(format!("DNS unavailable: {reason}"));
        self.startup_failed(reason);
    }

    fn startup_recovered(&mut self) {
        if let Some(failure) = self.startup.take() {
            eprintln!(
                "PariNS DNS started after {} failed attempt(s)",
                failure.attempts
            );
        }
    }

    /// When the next automatic restart is due: 5 s after the first failure,
    /// doubling to at most 60 s. `None` when nothing needs restarting.
    pub fn startup_retry_at(&self) -> Option<Instant> {
        let failure = self.startup.as_ref()?;
        if self.running.is_some() || self.saved.is_none() {
            return None;
        }
        Some(failure.failed_at + crate::server::backoff(failure.attempts))
    }

    /// Ends the running generation's task without a stop request.
    #[cfg(test)]
    pub(super) fn simulate_exit(&self) {
        self.running.as_ref().expect("running DNS").task.abort();
    }

    /// Stops DNS and records a startup failure, as a non-bind start error would.
    #[cfg(test)]
    pub(super) async fn simulate_startup_failure(&mut self) {
        self.stop().await;
        self.startup_failed("simulated certificate material unavailable".into());
    }

    /// Restarts the saved configuration. The caller holds the mutation permit
    /// and has seen [`Self::startup_retry_at`] due under the Manager lock, which
    /// guarantees a saved configuration and no running generation.
    pub async fn retry_startup(&mut self) {
        let toml = self
            .saved
            .as_ref()
            .expect("a due startup retry has a saved configuration")
            .toml
            .clone();
        self.restore(&toml, None).await;
    }

    /// Returns whether listener restart was necessary.
    pub async fn apply(&mut self, next: Stored) -> Result<bool> {
        // Management-only settings do not inspect changed certificate files,
        // restart a stopped DNS service, or publish new storage/cache settings.
        if self.updates_only(&next.toml)? {
            let config = Config::parse_in(&next.toml, &self.store.dir)?;
            let store = self.store.clone();
            let copy = next.clone();
            tokio::task::spawn_blocking(move || store.save(&copy)).await??;
            if let Some(running) = &mut self.running {
                running.config.updates = config.updates;
            }
            self.filters.set_revision(next.revision);
            self.saved = Some(next);
            self.rollback_error = None;
            return Ok(false);
        }
        ensure!(
            self.cache_ready,
            "cannot start DNS before clean snapshot is durably consumed; restart after resolving the storage error"
        );
        let (config, candidate) =
            prepare(self.store.dir.clone(), self.address, next.toml.clone()).await?;
        let local = config.local_policy_source();
        let filter_candidate = self
            .filters
            .prepare_config_from_source(
                config.filter_subscriptions.clone(),
                local,
                self.saved.as_ref().map_or(0, |s| s.revision),
            )
            .await?;
        if let Some(replace_cache) = self.cache_only_change(&next.toml, &config.cache) {
            // Manager's mutex serializes this transaction. There is no fallible
            // IO after persist and no await between live swap and revision update.
            let store = self.store.clone();
            let saved = next.clone();
            let resolver = self.resolver().expect("running cache-only target").clone();
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            self.filters.publish_config(filter_candidate, next.revision);
            if replace_cache {
                resolver.replace_cache(config.cache.clone());
            }
            self.services
                .storage
                .publish_settings(RuntimeSettings::from_config(&config), next.revision);
            self.running
                .as_mut()
                .expect("running hot-update target")
                .config = config;
            self.saved = Some(next);
            self.rollback_error = None;
            self.last_error = None;
            return Ok(false);
        }
        let previous = self.saved.clone();
        self.stop().await;
        let result = async {
            let server = Server::bind_with_policy(
                config.clone(),
                candidate.certificates.clone(),
                self.services.clone(),
                self.filters.handle(),
            )
            .await?;
            // An administrator's change is all or nothing: a candidate whose
            // listeners cannot all bind is rejected before persistence, and the
            // previous configuration is restored. Only restarts retry listeners.
            if let Some(unbound) = server.unbound() {
                return Err(unbound.into());
            }
            let listen = server.local_addr();
            let store = self.store.clone();
            let saved = next.clone();
            // Persist only after all candidate sockets and material are prepared.
            tokio::task::spawn_blocking(move || store.save(&saved)).await??;
            self.filters.publish_config(filter_candidate, next.revision);
            self.services
                .storage
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
                self.rollback_error = None;
                self.last_error = None;
                self.local_error = None;
                self.startup_recovered();
                Ok(true)
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.restore(&previous.toml, None).await;
                }
                // A failed restart of the previous configuration already explains
                // what dns_retry keeps restarting, and unavailable local rules
                // explain why it was not restarted; keep those messages.
                if self.running.is_none() && self.startup.is_none() && self.local_error.is_none() {
                    self.last_error = Some(format!("Apply failed; DNS is stopped: {error:#}"));
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
        self.filters.close();
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
