//! Listener ownership, admission limits, and task lifetime.

use std::{
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use crate::{
    config::Config,
    metrics::{Counter, Metrics},
    protocol,
    resolver::Resolver,
    transport::tcp,
};

pub struct Server {
    config: Config,
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
    resolver: Arc<Resolver>,
    encrypted: Vec<Encrypted>,
    admin: Option<TcpListener>,
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

enum Encrypted {
    Dot(TcpListener, Arc<rustls::ServerConfig>),
    Doh(TcpListener, Arc<rustls::ServerConfig>, Option<u16>),
    Quic(crate::quic::Listener, crate::quic::Protocol),
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

    pub async fn bind_with_policy(
        mut config: Config,
        certificates: Option<Arc<crate::tls::CertificateSet>>,
        services: Arc<crate::runtime_services::RuntimeServices>,
        policy: Arc<crate::filter_subscriptions::handle::PolicyHandle>,
    ) -> Result<Self> {
        config.validate()?;
        let certificates = match certificates {
            Some(prepared) => prepared,
            None => {
                let sources = config.certificate_sources();
                tokio::task::spawn_blocking(move || crate::tls::CertificateSet::prepare(sources))
                    .await??
            }
        };
        use crate::tls::CertificateRole as Role;
        let tcp = TcpListener::bind(config.listen).await?;
        let udp = Arc::new(UdpSocket::bind(tcp.local_addr()?).await?);
        config.listen = tcp.local_addr()?;
        let admin = match config.admin_listen {
            Some(address) => Some(TcpListener::bind(address).await?),
            None => None,
        };
        let mut encrypted = Vec::new();
        if let Some(settings) = &mut config.dot {
            let tls = certificates.server_config(Role::Dot, &[b"dot"])?;
            let listener = TcpListener::bind(settings.listen).await?;
            settings.listen = listener.local_addr()?;
            encrypted.push(Encrypted::Dot(listener, tls));
        }
        if let Some(settings) = &mut config.doh {
            let tls = certificates.server_config(Role::Doh, &[b"h2"])?;
            // Select the TCP port first. H3 must bind the exact same address;
            // failure drops the whole candidate before it can serve requests.
            let listener = TcpListener::bind(settings.listen).await?;
            let address = listener.local_addr()?;
            settings.listen = address;
            if settings.http3 {
                encrypted.push(Encrypted::Quic(
                    crate::quic::bind(
                        address,
                        certificates.server_config(Role::Doh, &[b"h3"])?,
                        config.max_inflight.min(1024),
                        crate::quic::Protocol::H3,
                    )?,
                    crate::quic::Protocol::H3,
                ));
            }
            encrypted.push(Encrypted::Doh(
                listener,
                tls,
                settings.http3.then_some(address.port()),
            ));
        }
        if let Some(settings) = &mut config.doq {
            let tls = certificates.server_config(Role::Doq, &[b"doq"])?;
            let endpoint = crate::quic::bind(
                settings.listen,
                tls,
                config.max_inflight.min(1024),
                crate::quic::Protocol::Doq,
            )?;
            settings.listen = endpoint.local_addr()?;
            encrypted.push(Encrypted::Quic(endpoint, crate::quic::Protocol::Doq));
        }
        let resolver = Arc::new(Resolver::with_services_and_policy(
            &config, services, policy,
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
            udp,
            tcp,
            resolver,
            encrypted,
            admin,
            reload,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.tcp.local_addr()?)
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
        self.admin
            .as_ref()
            .map(TcpListener::local_addr)
            .transpose()
            .map_err(Into::into)
    }

    pub fn encrypted_addrs(&self) -> Result<Vec<(&'static str, SocketAddr)>> {
        self.encrypted
            .iter()
            .map(|listener| {
                Ok(match listener {
                    Encrypted::Dot(listener, _) => ("dot", listener.local_addr()?),
                    Encrypted::Doh(listener, _, _) => ("doh", listener.local_addr()?),
                    Encrypted::Quic(endpoint, protocol) => (
                        match protocol {
                            crate::quic::Protocol::Doq => "doq",
                            crate::quic::Protocol::H3 => "doh3",
                        },
                        endpoint.local_addr()?,
                    ),
                })
            })
            .collect()
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let resolver = self.resolver;
        let metrics = resolver.metrics().clone();
        let run_id = rand::random::<u64>();
        let started = std::time::Instant::now();
        let report = self.config.metrics.interval_secs > 0;
        let period = Duration::from_secs(self.config.metrics.interval_secs.max(1));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let queries = Arc::new(Semaphore::new(self.config.max_inflight));
        let connections = Arc::new(Semaphore::new(self.config.max_tcp_connections));
        let io_timeout = Duration::from_millis(self.config.tcp_io_timeout_ms);
        let (stopping, stop) = watch::channel(false);
        let mut tasks = JoinSet::new();
        let mut adapters = JoinSet::new();
        let mut quic_runtimes = Vec::new();
        let ingress = crate::ingress::Ingress {
            resolver: resolver.clone(),
            queries: queries.clone(),
            connections: connections.clone(),
            source_limits: Arc::new(crate::limits::Limiter::new(&self.config.source_limits)?),
            stop: stop.clone(),
            io_timeout,
            shutdown_grace: Duration::from_millis(self.config.shutdown_grace_ms),
            max_streams: self.config.max_inflight.min(1024),
        };
        for listener in self.encrypted {
            if let Encrypted::Quic(endpoint, _) = &listener {
                quic_runtimes.push(endpoint.runtime());
            }
            let ingress = ingress.clone();
            adapters.spawn(async move {
                match listener {
                    Encrypted::Dot(listener, tls) => {
                        crate::tls::serve(listener, tls, ingress).await
                    }
                    Encrypted::Doh(listener, tls, h3_port) => {
                        crate::doh::serve(listener, tls, ingress, h3_port).await
                    }
                    Encrypted::Quic(endpoint, protocol) => {
                        crate::quic::serve(endpoint, protocol, ingress).await
                    }
                }
            });
        }
        if let Some(listener) = self.admin {
            let ingress = ingress.clone();
            adapters.spawn(crate::admin::serve(listener, ingress));
        }
        let mut buffer = vec![0; protocol::MAX_MESSAGE];
        tokio::pin!(shutdown);
        let outcome: Result<()> = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                Some(result) = adapters.join_next(), if !adapters.is_empty() => {
                    break match result {
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(error.into()),
                        Ok(Ok(())) => Err(anyhow::anyhow!("encrypted listener stopped unexpectedly")),
                    };
                }
                _ = ticker.tick(), if report => emit_metrics(&metrics, run_id, started, "periodic"),
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result { break Err(error.into()); }
                }
                accepted = self.tcp.accept() => {
                    let (stream, peer) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
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
                received = self.udp.recv_from(&mut buffer) => {
                    let (length, peer) = match received { Ok(value) => value, Err(error) => break Err(error.into()) };
                    metrics.inc(Counter::UdpReceived);
                    let Some(source) = ingress.admit_query(peer.ip()) else {
                        metrics.inc(Counter::UdpDropped);
                        resolver.log_rejected(&buffer[..length], peer.ip(), "udp", None);
                        continue;
                    };
                    let Ok(permit) = queries.clone().try_acquire_owned() else {
                        metrics.inc(Counter::UdpDropped);
                        resolver.log_rejected(&buffer[..length], peer.ip(), "udp", None);
                        continue
                    };
                    let bytes = buffer[..length].to_vec();
                    let socket = self.udp.clone();
                    let resolver = resolver.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _source = source;
                        if let Some(reply) = resolver.resolve_with_transport(&bytes, peer.ip(), "udp").await
                            && let Ok(bytes) = protocol::encode_udp(&reply.message, reply.udp_limit)
                        {
                            let _ = timeout(io_timeout, socket.send_to(&bytes, peer)).await;
                        }
                    });
                }
            }
        };
        self.reload.close();
        drop(self.tcp);
        let _ = stopping.send(true);
        let drained = timeout(
            Duration::from_millis(self.config.shutdown_grace_ms),
            async {
                while let Some(result) = tasks.join_next().await {
                    result?;
                }
                while let Some(result) = adapters.join_next().await {
                    result??;
                }
                Ok::<_, anyhow::Error>(())
            },
        )
        .await;
        if !matches!(&drained, Ok(Ok(()))) || outcome.is_err() {
            resolver.force_shutdown();
        }
        tasks.shutdown().await;
        adapters.shutdown().await;
        // The owner outlives adapter cancellation at the shutdown deadline.
        for runtime in quic_runtimes {
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
