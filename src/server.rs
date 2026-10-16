//! Listener ownership, admission limits, and task lifetime.
//!
//! Each configured listener has one supervisor owned by the generation. A listener
//! that cannot bind, or whose adapter exits while the generation runs, is rebound
//! alone after 5 s doubling to 60 s; the others, the Resolver, cache and upstream
//! pool keep serving. Health is ready only while every listener runs, otherwise
//! degraded. Identities are prepared once per generation, so a certificate error
//! still fails the whole start. DoH over HTTP/3 binds the DoH TCP address and its
//! actual port. Stopping the generation cancels pending retries and releases
//! every socket, including Quinn drivers.

use std::{
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use crate::{
    config::Config,
    metrics::{Counter, Metrics},
    protocol,
    resolver::{ForcedShutdown, Resolver},
    transport::{tcp, udp},
};

pub struct Server {
    config: Config,
    slots: Vec<Slot>,
    tls: Arc<ListenerTls>,
    listeners: Arc<Listeners>,
    services: Arc<crate::runtime_services::RuntimeServices>,
    generation: u64,
    resolver: Arc<Resolver>,
    reload: ReloadHandle,
}

/// Reloads only file-backed rules and listener certificates, never routing/cache ownership.
#[derive(Clone)]
pub struct ReloadHandle {
    resolver: Arc<Resolver>,
    certificates: Arc<crate::tls::CertificateSet>,
    filter_file: Option<PathBuf>,
    local_policy: crate::policy::LocalRules,
    filter_settings: crate::filter_subscriptions::settings::Settings,
    filters: Option<Arc<crate::filter_subscriptions::service::Service>>,
    serial: Arc<Mutex<()>>,
    publication_closed: Arc<Mutex<bool>>,
}

impl ReloadHandle {
    /// Call from a blocking worker: all candidates are validated before any publication.
    pub fn reload(&self) -> Result<()> {
        anyhow::ensure!(
            self.filters.is_none(),
            "subscription reload requires reload_async"
        );
        let _serial = self.serial.lock().expect("reload lock poisoned");
        let policy = self
            .filter_file
            .as_deref()
            .map(crate::policy::Policy::load)
            .transpose()?;
        let prepared = self.certificates.prepare_reload()?;
        let closed = self
            .publication_closed
            .lock()
            .expect("reload publication lock");
        anyhow::ensure!(!*closed, "DNS generation stopped before reload publication");
        if let Some(policy) =
            policy.filter(|policy| policy.semantic_digest() != self.resolver.policy_digest())
        {
            self.resolver.replace_policy_with(policy, || {
                self.certificates.publish(prepared)?;
                Ok(())
            })?;
        } else {
            self.certificates.publish(prepared)?;
        }
        Ok(())
    }

    /// File-mode subscription reload never downloads. IO and compilation happen
    /// outside publication; shutdown, certificates and the aggregate share one cut.
    pub async fn reload_async(&self) -> Result<()> {
        let Some(filters) = &self.filters else {
            let reload = self.clone();
            return tokio::task::spawn_blocking(move || reload.reload()).await?;
        };
        let revision = filters.config_revision();
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("filter revision exhausted"))?;
        let reload = self.clone();
        let certificates =
            tokio::task::spawn_blocking(move || reload.certificates.prepare_reload()).await??;
        let local = match &self.filter_file {
            Some(path) => crate::policy::LocalPolicySource::File(path.clone()),
            None => crate::policy::LocalPolicySource::Inline(self.local_policy.clone()),
        };
        let candidate = filters
            .prepare_config_from_source(self.filter_settings.clone(), local, revision)
            .await?;
        // Lock order: file publication gate -> Service/PolicyHandle -> identities.
        // The candidate holds the one worker lease, excluding refresh publication.
        let closed = self
            .publication_closed
            .lock()
            .expect("reload publication lock");
        anyhow::ensure!(!*closed, "DNS generation stopped before reload publication");
        filters.validate_candidate(&candidate)?;
        self.certificates.publish(certificates)?;
        filters.publish_config(candidate, next_revision);
        Ok(())
    }

    fn close(&self) {
        *self
            .publication_closed
            .lock()
            .expect("reload publication lock") = true;
        if let Some(filters) = &self.filters {
            filters.request_close();
        }
    }
}

/// One configured listener. A slot that failed to bind, or whose adapter exited
/// while the generation keeps running, is rebound on its own with bounded backoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerKind {
    Dns,
    Admin,
    Dot,
    Doh,
    Doh3,
    Doq,
}
impl ListenerKind {
    fn label(self) -> &'static str {
        match self {
            Self::Dns => "DNS",
            Self::Admin => "admin",
            Self::Dot => "DoT",
            Self::Doh => "DoH",
            Self::Doh3 => "DoH HTTP/3",
            Self::Doq => "DoQ",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerState {
    Running,
    Retrying,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ListenerStatus {
    pub kind: ListenerKind,
    pub address: SocketAddr,
    pub state: ListenerState,
    pub error: Option<String>,
    pub failures: u32,
    #[serde(skip)]
    logged: Option<(String, std::time::Instant)>,
    #[serde(skip)]
    recovery_logged: Option<std::time::Instant>,
}

/// Per-listener state of one generation, published to DNS health and status.
pub struct Listeners {
    status: Mutex<Vec<ListenerStatus>>,
    changed: watch::Sender<()>,
}
const LOG_INTERVAL: Duration = Duration::from_secs(600);
impl Listeners {
    pub fn snapshot(&self) -> Vec<ListenerStatus> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn serving(&self, kind: ListenerKind) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|l| l.kind == kind && l.state == ListenerState::Running)
    }
    pub fn all_running(&self) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .all(|l| l.state == ListenerState::Running)
    }
    /// `failures` keeps counting until the supervisor sees the listener serve
    /// stably, so one that binds and exits again keeps backing off.
    fn running(&self, index: usize, address: SocketAddr) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let listener = &mut status[index];
        let now = std::time::Instant::now();
        if listener.failures > 0
            && listener
                .recovery_logged
                .is_none_or(|at| now.duration_since(at) >= LOG_INTERVAL)
        {
            eprintln!(
                "PariNS {} {address} listening again after {} failed attempt(s)",
                listener.kind.label(),
                listener.failures
            );
            listener.recovery_logged = Some(now);
        }
        listener.address = address;
        listener.state = ListenerState::Running;
        listener.error = None;
        drop(status);
        self.changed.send_replace(());
    }
    /// Logs the first failure, a changed error, and the same error at most every
    /// ten minutes, so a long outage stays visible without flooding the journal.
    fn failed(&self, index: usize, error: String, failures: u32) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let listener = &mut status[index];
        let now = std::time::Instant::now();
        if listener
            .logged
            .as_ref()
            .is_none_or(|(logged, at)| *logged != error || now.duration_since(*at) >= LOG_INTERVAL)
        {
            eprintln!(
                "PariNS {} {} unavailable: {error}; retrying automatically",
                listener.kind.label(),
                listener.address
            );
            listener.logged = Some((error.clone(), now));
        }
        listener.state = ListenerState::Retrying;
        listener.error = Some(error);
        listener.failures = failures;
        drop(status);
        self.changed.send_replace(());
    }
}

/// Retry delay after `failures` consecutive failures: 5 s doubling to 60 s.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(5)
        .saturating_mul(1 << failures.saturating_sub(1).min(4))
        .min(Duration::from_secs(60))
}

/// Server identities are prepared once per generation; only sockets are rebound.
struct ListenerTls {
    dot: Option<Arc<rustls::ServerConfig>>,
    doh: Option<Arc<rustls::ServerConfig>>,
    doh3: Option<Arc<rustls::ServerConfig>>,
    doq: Option<Arc<rustls::ServerConfig>>,
    max_streams: usize,
    h3_port: Option<u16>,
}

enum Bound {
    Dns(TcpListener, Arc<udp::Socket>),
    Tcp(TcpListener),
    Quic(crate::quic::Listener),
}
impl Bound {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Dns(listener, _) | Self::Tcp(listener) => listener.local_addr(),
            Self::Quic(listener) => listener.local_addr(),
        }
    }
}

struct Slot {
    kind: ListenerKind,
    address: SocketAddr,
    bound: Option<Bound>,
}

// Name the protocol and address in bind errors: "address in use" alone does not
// tell an operator which listener collided.
async fn bind_slot(kind: ListenerKind, address: SocketAddr, tls: &ListenerTls) -> Result<Bound> {
    use crate::quic::Protocol;
    let quic = |config: &Option<Arc<rustls::ServerConfig>>, protocol| {
        crate::quic::bind(
            address,
            config.clone().expect("prepared QUIC identity"),
            tls.max_streams,
            protocol,
        )
    };
    Ok(match kind {
        ListenerKind::Dns => {
            let tcp = TcpListener::bind(address)
                .await
                .with_context(|| format!("bind DNS TCP {address}"))?;
            let actual = tcp.local_addr()?;
            let udp = udp::Socket::bind(actual)
                .await
                .with_context(|| format!("bind DNS UDP {actual}"))?;
            Bound::Dns(tcp, Arc::new(udp))
        }
        ListenerKind::Admin | ListenerKind::Dot | ListenerKind::Doh => Bound::Tcp(
            TcpListener::bind(address)
                .await
                .with_context(|| format!("bind {} {address}", kind.label()))?,
        ),
        ListenerKind::Doh3 => Bound::Quic(
            quic(&tls.doh3, Protocol::H3)
                .with_context(|| format!("bind DoH HTTP/3 UDP {address}"))?,
        ),
        ListenerKind::Doq => Bound::Quic(
            quic(&tls.doq, Protocol::Doq).with_context(|| format!("bind DoQ {address}"))?,
        ),
    })
}

async fn serve_bound(
    kind: ListenerKind,
    bound: Bound,
    tls: &ListenerTls,
    ingress: crate::ingress::Ingress,
) -> Result<()> {
    use crate::quic::Protocol;
    match (kind, bound) {
        (_, Bound::Dns(tcp, udp)) => serve_plain(tcp, udp, ingress).await,
        (ListenerKind::Admin, Bound::Tcp(listener)) => crate::admin::serve(listener, ingress).await,
        (ListenerKind::Dot, Bound::Tcp(listener)) => {
            crate::tls::serve(listener, tls.dot.clone().expect("DoT identity"), ingress).await
        }
        (_, Bound::Tcp(listener)) => {
            crate::doh::serve(
                listener,
                tls.doh.clone().expect("DoH identity"),
                ingress,
                tls.h3_port,
            )
            .await
        }
        (ListenerKind::Doh3, Bound::Quic(listener)) => {
            crate::quic::serve(listener, Protocol::H3, ingress).await
        }
        (_, Bound::Quic(listener)) => crate::quic::serve(listener, Protocol::Doq, ingress).await,
    }
}

/// Owns one listener for the generation: serves it, and after a bind failure or
/// an adapter exit rebinds only this listener until the generation stops.
async fn supervise(
    index: usize,
    mut slot: Slot,
    tls: Arc<ListenerTls>,
    listeners: Arc<Listeners>,
    runtimes: Arc<Mutex<Vec<Option<Arc<crate::quic::ListenerRuntime>>>>>,
    ingress: crate::ingress::Ingress,
) -> Result<()> {
    // A listener that served this long counts as recovered: the next failure
    // starts the backoff again from 5 seconds.
    const STABLE: Duration = Duration::from_secs(60);
    let mut stop = ingress.stop.clone();
    let mut failures = listeners.snapshot()[index].failures;
    loop {
        let bound = match slot.bound.take() {
            Some(bound) => bound,
            None => {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(backoff(failures)) => {},
                }
                if *stop.borrow() {
                    return Ok(());
                }
                match bind_slot(slot.kind, slot.address, &tls).await {
                    Ok(bound) => bound,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        listeners.failed(index, format!("{error:#}"), failures);
                        continue;
                    }
                }
            }
        };
        let address = bound.local_addr()?;
        if let Bound::Quic(listener) = &bound {
            // The owner outlives adapter cancellation at the shutdown deadline.
            runtimes.lock().expect("QUIC runtimes")[index] = Some(listener.runtime());
        }
        listeners.running(index, address);
        let serving = std::time::Instant::now();
        let result = serve_bound(slot.kind, bound, &tls, ingress.clone()).await;
        if *stop.borrow() {
            return result;
        }
        if serving.elapsed() >= STABLE {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        listeners.failed(
            index,
            match result {
                Ok(()) => "listener stopped unexpectedly".into(),
                Err(error) => format!("{error:#}"),
            },
            failures,
        );
    }
}

impl Server {
    /// File-mode service is closed with the DNS generation; managed mode keeps
    /// its process-owned service outside individual listener generations.
    pub async fn bind_with_filter_service(
        config: Config,
        certificates: Option<Arc<crate::tls::CertificateSet>>,
        services: Arc<crate::runtime_services::RuntimeServices>,
        filters: Arc<crate::filter_subscriptions::service::Service>,
    ) -> Result<Self> {
        anyhow::ensure!(
            filters.ready(),
            "subscription_material_missing: complete policy required before binding"
        );
        let mut server =
            Self::bind_with_policy(config, certificates, services, filters.handle()).await?;
        server.reload.filters = Some(filters);
        Ok(server)
    }
    /// Bind both protocols before accepting traffic. Port zero selects one shared port.
    pub async fn bind(config: Config) -> Result<Self> {
        let services = crate::runtime_services::RuntimeServices::ephemeral(
            crate::storage::RuntimeSettings::from_config(&config),
        );
        Self::bind_with_services(config, None, services).await
    }

    pub async fn bind_with_services(
        config: Config,
        certificates: Option<Arc<crate::tls::CertificateSet>>,
        services: Arc<crate::runtime_services::RuntimeServices>,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.filter_subscriptions.effective().next().is_none(),
            "subscription_material_missing: subscriptions require a prepared policy"
        );
        let policy = crate::filter_subscriptions::handle::PolicyHandle::new(config.load_policy()?);
        Self::bind_with_policy(config, certificates, services, policy).await
    }

    /// `config` has passed `Config::validate`; listener and upstream checks are not repeated.
    pub async fn bind_with_policy(
        mut config: Config,
        certificates: Option<Arc<crate::tls::CertificateSet>>,
        services: Arc<crate::runtime_services::RuntimeServices>,
        policy: Arc<crate::filter_subscriptions::handle::PolicyHandle>,
    ) -> Result<Self> {
        let certificates = match certificates {
            Some(prepared) => prepared,
            None => {
                let sources = config.certificate_sources();
                tokio::task::spawn_blocking(move || crate::tls::CertificateSet::prepare(sources))
                    .await??
            }
        };
        use crate::tls::CertificateRole as Role;
        let tls = ListenerTls {
            dot: config
                .dot
                .as_ref()
                .map(|_| certificates.server_config(Role::Dot, &[b"dot"]))
                .transpose()?,
            doh: config
                .doh
                .as_ref()
                .map(|_| certificates.server_config(Role::Doh, &[b"h2"]))
                .transpose()?,
            doh3: config
                .doh
                .as_ref()
                .filter(|d| d.http3)
                .map(|_| certificates.server_config(Role::Doh, &[b"h3"]))
                .transpose()?,
            doq: config
                .doq
                .as_ref()
                .map(|_| certificates.server_config(Role::Doq, &[b"doq"]))
                .transpose()?,
            max_streams: config.max_inflight.min(1024),
            h3_port: None,
        };
        let mut planned = vec![(ListenerKind::Dns, config.listen)];
        planned.extend(config.admin_listen.map(|a| (ListenerKind::Admin, a)));
        planned.extend(config.dot.as_ref().map(|d| (ListenerKind::Dot, d.listen)));
        if let Some(doh) = &config.doh {
            planned.push((ListenerKind::Doh, doh.listen));
            if doh.http3 {
                // H3 binds the exact DoH TCP address and actual port over UDP.
                planned.push((ListenerKind::Doh3, doh.listen));
            }
        }
        planned.extend(config.doq.as_ref().map(|d| (ListenerKind::Doq, d.listen)));
        // A listener that cannot bind does not stop the others; its supervisor
        // retries it once the generation runs.
        let mut slots = Vec::with_capacity(planned.len());
        let mut status = Vec::with_capacity(planned.len());
        let mut doh_actual = None;
        for (kind, mut address) in planned {
            if kind == ListenerKind::Doh3
                && let Some(actual) = doh_actual
            {
                address = actual;
            }
            let (bound, error) = match bind_slot(kind, address, &tls).await {
                Ok(bound) => {
                    address = bound.local_addr()?;
                    (Some(bound), None)
                }
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            match kind {
                ListenerKind::Dns => config.listen = address,
                ListenerKind::Admin => config.admin_listen = Some(address),
                ListenerKind::Dot => config.dot.as_mut().expect("DoT").listen = address,
                ListenerKind::Doh => {
                    config.doh.as_mut().expect("DoH").listen = address;
                    doh_actual = Some(address);
                }
                ListenerKind::Doh3 => {}
                ListenerKind::Doq => config.doq.as_mut().expect("DoQ").listen = address,
            }
            if let Some(error) = &error {
                eprintln!(
                    "PariNS {} {address} unavailable: {error}; retrying automatically",
                    kind.label()
                );
            }
            status.push(ListenerStatus {
                kind,
                address,
                state: if error.is_some() {
                    ListenerState::Retrying
                } else {
                    ListenerState::Running
                },
                failures: u32::from(error.is_some()),
                logged: error.clone().map(|e| (e, std::time::Instant::now())),
                recovery_logged: None,
                error,
            });
            slots.push(Slot {
                kind,
                address,
                bound,
            });
        }
        let tls = ListenerTls {
            h3_port: config
                .doh
                .as_ref()
                .filter(|d| d.http3)
                .map(|d| d.listen.port()),
            ..tls
        };
        let resolver = Arc::new(Resolver::with_services_and_policy(
            &config,
            services.clone(),
            policy,
        )?);
        let reload = ReloadHandle {
            resolver: resolver.clone(),
            certificates,
            filter_file: config.filter_file.clone(),
            local_policy: config.filter.clone(),
            filter_settings: config.filter_subscriptions.clone(),
            filters: None,
            serial: Arc::new(Mutex::new(())),
            publication_closed: Arc::new(Mutex::new(false)),
        };
        Ok(Self {
            config,
            slots,
            tls: Arc::new(tls),
            listeners: Arc::new(Listeners {
                status: Mutex::new(status),
                changed: watch::channel(()).0,
            }),
            services,
            generation: 1,
            resolver,
            reload,
        })
    }

    /// The DNS health generation this server publishes (file mode uses 1).
    pub fn set_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    pub fn listeners(&self) -> Arc<Listeners> {
        self.listeners.clone()
    }

    /// Whether a listener of this kind is currently serving.
    pub fn serving(&self, kind: ListenerKind) -> bool {
        self.listeners.serving(kind)
    }

    /// Bind errors of listeners that are not yet serving, for callers that must
    /// not commit a partially bound candidate.
    pub fn unbound(&self) -> Option<String> {
        let errors: Vec<_> = self
            .listeners
            .snapshot()
            .into_iter()
            .filter_map(|listener| listener.error)
            .collect();
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    /// The plain DNS address: actual when bound, configured while it retries.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.config.listen)
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        self.resolver.metrics()
    }

    pub fn resolver(&self) -> &Arc<Resolver> {
        &self.resolver
    }

    pub fn reload_handle(&self) -> ReloadHandle {
        self.reload.clone()
    }

    pub fn admin_addr(&self) -> Result<Option<SocketAddr>> {
        Ok(self.config.admin_listen)
    }

    pub fn encrypted_addrs(&self) -> Result<Vec<(&'static str, SocketAddr)>> {
        Ok(self
            .slots
            .iter()
            .filter_map(|slot| {
                Some((
                    match slot.kind {
                        ListenerKind::Dot => "dot",
                        ListenerKind::Doh => "doh",
                        ListenerKind::Doh3 => "doh3",
                        ListenerKind::Doq => "doq",
                        ListenerKind::Dns | ListenerKind::Admin => return None,
                    },
                    slot.address,
                ))
            })
            .collect())
    }

    /// Publishes ready when every listener runs, degraded otherwise.
    pub fn publish_health(&self) {
        use crate::runtime_health::State;
        // Republishing an unchanged state would move its changed_at timestamp.
        let current = self.services.health.snapshot();
        let state = if self.listeners.all_running() {
            State::Running
        } else {
            State::Degraded
        };
        if current.generation == self.generation && current.state == state {
            return;
        }
        if state == State::Running {
            self.services
                .set_dns_health(State::Running, self.generation, None);
        } else {
            self.services.set_dns_health(
                State::Degraded,
                self.generation,
                Some("listener_unavailable"),
            );
        }
    }

    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let resolver = self.resolver.clone();
        let metrics = resolver.metrics().clone();
        let run_id = rand::random::<u64>();
        let started = std::time::Instant::now();
        let report = self.config.metrics.interval_secs > 0;
        let period = Duration::from_secs(self.config.metrics.interval_secs.max(1));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (stopping, stop) = watch::channel(false);
        let ingress = crate::ingress::Ingress {
            resolver: resolver.clone(),
            queries: Arc::new(Semaphore::new(self.config.max_inflight)),
            connections: Arc::new(Semaphore::new(self.config.max_tcp_connections)),
            source_limits: Arc::new(crate::limits::Limiter::new(&self.config.source_limits)?),
            stop: stop.clone(),
            io_timeout: Duration::from_millis(self.config.tcp_io_timeout_ms),
            shutdown_grace: Duration::from_millis(self.config.shutdown_grace_ms),
            max_streams: self.config.max_inflight.min(1024),
        };
        let mut changed = self.listeners.changed.subscribe();
        self.publish_health();
        let runtimes = Arc::new(Mutex::new(vec![None; self.slots.len()]));
        let mut supervisors = JoinSet::new();
        for (index, slot) in std::mem::take(&mut self.slots).into_iter().enumerate() {
            supervisors.spawn(supervise(
                index,
                slot,
                self.tls.clone(),
                self.listeners.clone(),
                runtimes.clone(),
                ingress.clone(),
            ));
        }
        tokio::pin!(shutdown);
        let outcome: Result<()> = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                _ = changed.changed() => self.publish_health(),
                _ = ticker.tick(), if report => emit_metrics(&metrics, run_id, started, "periodic"),
                // Supervisors return only after the stop signal, or by panicking.
                Some(result) = supervisors.join_next() => {
                    break match result {
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(error.into()),
                        Ok(Ok(())) => Err(anyhow::anyhow!("listener supervisor stopped unexpectedly")),
                    };
                }
            }
        };
        self.reload.close();
        let _ = stopping.send(true);
        let drained = timeout(
            Duration::from_millis(self.config.shutdown_grace_ms),
            async {
                while let Some(result) = supervisors.join_next().await {
                    result??;
                }
                Ok::<_, anyhow::Error>(())
            },
        )
        .await;
        match (&drained, &outcome) {
            (Ok(Ok(())), Ok(_)) => {}
            (Err(_), Ok(_)) => resolver.force_shutdown(ForcedShutdown::ListenerDrain),
            _ => resolver.force_shutdown(ForcedShutdown::ListenerFailed),
        }
        supervisors.shutdown().await;
        let runtimes = std::mem::take(&mut *runtimes.lock().expect("QUIC runtimes"));
        for runtime in runtimes.into_iter().flatten() {
            runtime.shutdown().await;
        }
        resolver.shutdown_refresh().await;
        resolver.finish_shutdown();
        if report {
            emit_metrics(&metrics, run_id, started, "shutdown");
        }
        if let Ok(result) = drained {
            result?;
        }
        outcome
    }
}

/// Plain DNS over UDP and TCP on one address.
async fn serve_plain(
    tcp: TcpListener,
    udp: Arc<udp::Socket>,
    mut ingress: crate::ingress::Ingress,
) -> Result<()> {
    let resolver = ingress.resolver.clone();
    let metrics = resolver.metrics().clone();
    let mut tasks = JoinSet::new();
    let mut buffer = vec![0; protocol::MAX_MESSAGE];
    let outcome: Result<()> = loop {
        if *ingress.stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = ingress.stop.changed() => break Ok(()),
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result { break Err(error.into()); }
            }
            accepted = tcp.accept() => {
                let (stream, peer) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let Ok(permit) = ingress.connections.clone().try_acquire_owned() else {
                    metrics.inc(Counter::ConnectionsRejected);
                    continue
                };
                let Some(source) = ingress.admit_connection(peer.ip()) else {
                    metrics.inc(Counter::ConnectionsRejected);
                    continue;
                };
                let context = ingress.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    serve_tcp(stream, peer.ip(), context).await;
                });
            }
            received = udp.recv(&mut buffer) => {
                let received = match received { Ok(value) => value, Err(error) => break Err(error.into()) };
                metrics.inc(Counter::UdpReceived);
                let Some((length, peer, source_address)) = received else {
                    metrics.inc(Counter::UdpDropped);
                    continue;
                };
                let Some(source) = ingress.admit_query(peer.ip()) else {
                    metrics.inc(Counter::UdpDropped);
                    resolver.log_rejected(&buffer[..length], peer.ip(), "udp", None);
                    continue;
                };
                let Ok(permit) = ingress.queries.clone().try_acquire_owned() else {
                    metrics.inc(Counter::UdpDropped);
                    resolver.log_rejected(&buffer[..length], peer.ip(), "udp", None);
                    continue
                };
                let bytes = buffer[..length].to_vec();
                let socket = udp.clone();
                let resolver = resolver.clone();
                let io_timeout = ingress.io_timeout;
                tasks.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    if let Some(reply) = resolver.resolve_with_transport(&bytes, peer.ip(), "udp").await
                        && let Ok(bytes) = protocol::encode_udp(&reply.message, reply.udp_limit)
                    {
                        let _ = timeout(io_timeout, socket.reply(&bytes, peer, source_address)).await;
                    }
                });
            }
        }
    };
    drop(tcp);
    if timeout(ingress.shutdown_grace, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        // Only a generation stop forces it (and its upstream pool) down; after an
        // error exit the supervisor rebinds this listener while others serve.
        if *ingress.stop.borrow() {
            resolver.force_shutdown(ForcedShutdown::ListenerDrain);
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    outcome
}

fn emit_metrics(metrics: &Metrics, run_id: u64, started: std::time::Instant, reason: &str) {
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "parins_metrics", "entry_point": "server", "run_id": run_id,
            "reason": reason, "uptime_secs": started.elapsed().as_secs(), "metrics": metrics.snapshot()
        })
    );
}

async fn serve_tcp(
    mut stream: TcpStream,
    peer: std::net::IpAddr,
    mut ingress: crate::ingress::Ingress,
) {
    let resolver = &ingress.resolver;
    let io_timeout = ingress.io_timeout;
    loop {
        let frame = tokio::select! {
            biased;
            _ = ingress.stop.changed() => return,
            result = timeout(io_timeout, tcp::read_frame(&mut stream)) => result,
        };
        let Ok(Ok(bytes)) = frame else { return };
        resolver.metrics().inc(Counter::TcpReceived);
        let source = ingress.admit_query(peer);
        let permit = ingress.queries.clone().try_acquire_owned();
        let response = match (source.as_ref(), &permit) {
            (Some(_), Ok(_)) => resolver
                .resolve_with_transport(&bytes, peer, "tcp")
                .await
                .map(|reply| reply.message),
            _ => {
                resolver.metrics().inc(Counter::TcpRejected);
                let reply = crate::ingress::rejected_response(&bytes);
                resolver.log_rejected(&bytes, peer, "tcp", reply.as_ref());
                reply
            }
        };
        let Some(response) = response else { return };
        let Ok(bytes) = protocol::encode_hop(&response, false, false, 468, protocol::MAX_MESSAGE)
        else {
            return;
        };
        if !matches!(
            timeout(io_timeout, tcp::write_frame(&mut stream, &bytes)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_health::State;

    #[tokio::test]
    async fn an_exiting_listener_is_retried_alone_and_stop_cancels_its_retry() {
        let mut config = Config::parse(
            "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=200\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n",
        )
        .unwrap();
        // Validation rejects this; here it makes the admin adapter exit with an
        // error after binding, standing in for a listener failing at runtime.
        config.admin_listen = Some("0.0.0.0:0".parse().unwrap());
        let services = crate::runtime_services::RuntimeServices::ephemeral(
            crate::storage::RuntimeSettings::from_config(&config),
        );
        let server = Server::bind_with_services(config, None, services.clone())
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let resolver = server.resolver().clone();
        let listeners = server.listeners();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        let degraded = tokio::time::timeout(Duration::from_secs(2), async {
            while services.health.snapshot().state != State::Degraded {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(degraded.is_ok());
        let snapshot = listeners.snapshot();
        let admin = snapshot
            .iter()
            .find(|l| l.kind == ListenerKind::Admin)
            .unwrap();
        assert_eq!(admin.state, ListenerState::Retrying);
        assert!(admin.error.as_deref().unwrap().contains("loopback"));
        let dns = snapshot
            .iter()
            .find(|l| l.kind == ListenerKind::Dns)
            .unwrap();
        assert_eq!(dns.state, ListenerState::Running);
        // The failing listener did not force the generation or its upstreams down.
        assert_eq!(resolver.forced_shutdown(), None);
        // It binds again after 5 s and exits at once: that is not a recovery, so
        // the failure count (and with it the backoff) keeps growing.
        let failures = |listeners: &Listeners| {
            listeners
                .snapshot()
                .into_iter()
                .find(|l| l.kind == ListenerKind::Admin)
                .unwrap()
                .failures
        };
        assert_eq!(failures(&listeners), 1);
        let grew = tokio::time::timeout(Duration::from_secs(8), async {
            while failures(&listeners) < 2 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(grew.is_ok());
        assert_eq!(backoff(failures(&listeners)), Duration::from_secs(10));
        // Stopping cancels the pending retry and releases every socket at once.
        let started = std::time::Instant::now();
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(resolver.forced_shutdown(), None);
        let rebound = TcpListener::bind(address).await;
        assert!(rebound.is_ok(), "DNS port released on stop");
    }
}
