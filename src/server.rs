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
    identities: Vec<crate::tls::Identity>,
    filter_file: Option<PathBuf>,
    serial: Arc<Mutex<()>>,
}

impl ReloadHandle {
    /// Call from a blocking worker: all candidates are validated before any publication.
    pub fn reload(&self) -> Result<()> {
        let _serial = self.serial.lock().expect("reload lock poisoned");
        let policy = self
            .filter_file
            .as_deref()
            .map(crate::policy::Policy::load)
            .transpose()?;
        let keys = self
            .identities
            .iter()
            .map(crate::tls::Identity::prepare)
            .collect::<Result<Vec<_>>>()?;
        for (identity, key) in self.identities.iter().zip(keys) {
            identity.install(key);
        }
        if let Some(policy) = policy {
            self.resolver.replace_policy(policy);
        }
        Ok(())
    }
}

enum Encrypted {
    Dot(TcpListener, Arc<rustls::ServerConfig>),
    Doh(TcpListener, Arc<rustls::ServerConfig>),
    Quic(quinn::Endpoint, crate::quic::Protocol),
}

impl Server {
    /// Bind both protocols before accepting traffic. Port zero selects one shared port.
    pub async fn bind(config: Config) -> Result<Self> {
        config.validate()?;
        let tcp = TcpListener::bind(config.listen).await?;
        let udp = Arc::new(UdpSocket::bind(tcp.local_addr()?).await?);
        let resolver = Arc::new(Resolver::try_from_config(&config)?);
        let admin = match config.admin_listen {
            Some(address) => Some(TcpListener::bind(address).await?),
            None => None,
        };
        let mut encrypted = Vec::new();
        let mut identities = Vec::new();
        if let Some(settings) = &config.dot {
            let (tls, identity) = crate::tls::reloading_server_config(&settings.files, &[b"dot"])?;
            identities.push(identity);
            encrypted.push(Encrypted::Dot(
                TcpListener::bind(settings.listen).await?,
                tls,
            ));
        }
        if let Some(settings) = &config.doh {
            let (tls, identity) = crate::tls::reloading_server_config(&settings.files, &[b"h2"])?;
            identities.push(identity);
            encrypted.push(Encrypted::Doh(
                TcpListener::bind(settings.listen).await?,
                tls,
            ));
        }
        for (settings, protocol, alpn) in [
            (&config.doq, crate::quic::Protocol::Doq, b"doq".as_slice()),
            (&config.doh3, crate::quic::Protocol::H3, b"h3".as_slice()),
        ] {
            if let Some(settings) = settings {
                let (tls, identity) =
                    crate::tls::reloading_server_config(&settings.files, &[alpn])?;
                identities.push(identity);
                encrypted.push(Encrypted::Quic(
                    crate::quic::bind(
                        settings.listen,
                        tls,
                        config.max_inflight.min(1024),
                        protocol,
                    )?,
                    protocol,
                ));
            }
        }
        let reload = ReloadHandle {
            resolver: resolver.clone(),
            identities,
            filter_file: config.filter_file.clone(),
            serial: Arc::new(Mutex::new(())),
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
                    Encrypted::Doh(listener, _) => ("doh", listener.local_addr()?),
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
            let ingress = ingress.clone();
            adapters.spawn(async move {
                match listener {
                    Encrypted::Dot(listener, tls) => {
                        crate::tls::serve(listener, tls, ingress).await
                    }
                    Encrypted::Doh(listener, tls) => {
                        crate::doh::serve(listener, tls, ingress).await
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
                        continue;
                    };
                    let Ok(permit) = queries.clone().try_acquire_owned() else {
                        metrics.inc(Counter::UdpDropped);
                        continue
                    };
                    let bytes = buffer[..length].to_vec();
                    let socket = self.udp.clone();
                    let resolver = resolver.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _source = source;
                        if let Some(reply) = resolver.resolve(&bytes, peer.ip()).await
                            && let Ok(bytes) = protocol::encode_udp(&reply.message, reply.udp_limit)
                        {
                            let _ = timeout(io_timeout, socket.send_to(&bytes, peer)).await;
                        }
                    });
                }
            }
        };
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
        tasks.shutdown().await;
        adapters.shutdown().await;
        resolver.shutdown_refresh().await;
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
                .resolve(&bytes, peer)
                .await
                .map(|reply| reply.message),
            _ => {
                resolver.metrics().inc(Counter::TcpRejected);
                crate::ingress::rejected_response(&bytes)
            }
        };
        let Some(response) = response else { return };
        let Ok(bytes) = response.to_vec() else { return };
        if !matches!(
            timeout(io_timeout, tcp::write_frame(&mut stream, &bytes)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}
