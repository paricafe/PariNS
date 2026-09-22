//! Listener ownership, admission limits, and task lifetime.

use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use hickory_proto::op::ResponseCode;
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
}

impl Server {
    /// Bind both protocols before accepting traffic. Port zero selects one shared port.
    pub async fn bind(config: Config) -> Result<Self> {
        config.validate()?;
        let tcp = TcpListener::bind(config.listen).await?;
        let udp = Arc::new(UdpSocket::bind(tcp.local_addr()?).await?);
        let resolver = Arc::new(Resolver::from_config(&config));
        Ok(Self {
            config,
            udp,
            tcp,
            resolver,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.tcp.local_addr()?)
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        self.resolver.metrics()
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
        let mut buffer = vec![0; protocol::MAX_MESSAGE];
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = ticker.tick(), if report => emit_metrics(&metrics, run_id, started, "periodic"),
                Some(result) = tasks.join_next(), if !tasks.is_empty() => { result?; }
                accepted = self.tcp.accept() => {
                    let (stream, peer) = accepted?;
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        metrics.inc(Counter::ConnectionsRejected);
                        continue
                    };
                    let resolver = resolver.clone();
                    let queries = queries.clone();
                    let stop = stop.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        serve_tcp(stream, peer.ip(), resolver, queries, stop, io_timeout).await;
                    });
                }
                received = self.udp.recv_from(&mut buffer) => {
                    let (length, peer) = received?;
                    metrics.inc(Counter::UdpReceived);
                    let Ok(permit) = queries.clone().try_acquire_owned() else {
                        metrics.inc(Counter::UdpDropped);
                        continue
                    };
                    let bytes = buffer[..length].to_vec();
                    let socket = self.udp.clone();
                    let resolver = resolver.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        if let Some(reply) = resolver.resolve(&bytes, peer.ip()).await
                            && let Ok(bytes) = protocol::encode_udp(&reply.message, reply.udp_limit)
                        {
                            let _ = timeout(io_timeout, socket.send_to(&bytes, peer)).await;
                        }
                    });
                }
            }
        }
        drop(self.tcp);
        let _ = stopping.send(true);
        let drained = timeout(
            Duration::from_millis(self.config.shutdown_grace_ms),
            async {
                while let Some(result) = tasks.join_next().await {
                    result?;
                }
                Ok::<_, tokio::task::JoinError>(())
            },
        )
        .await;
        tasks.shutdown().await;
        if report {
            emit_metrics(&metrics, run_id, started, "shutdown");
        }
        if let Ok(result) = drained {
            result?;
        }
        Ok(())
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
    resolver: Arc<Resolver>,
    queries: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
    io_timeout: Duration,
) {
    loop {
        let frame = tokio::select! {
            biased;
            _ = stop.changed() => return,
            result = timeout(io_timeout, tcp::read_frame(&mut stream)) => result,
        };
        let Ok(Ok(bytes)) = frame else { return };
        resolver.metrics().inc(Counter::TcpReceived);
        let permit = queries.clone().try_acquire_owned();
        let response = match permit {
            Ok(ref _permit) => resolver
                .resolve(&bytes, peer)
                .await
                .map(|reply| reply.message),
            Err(_) => {
                resolver.metrics().inc(Counter::TcpRejected);
                match protocol::request(&bytes) {
                    protocol::Request::Forward(query) => {
                        Some(protocol::error_response(&query, ResponseCode::ServFail))
                    }
                    protocol::Request::Reply(reply) => Some(reply),
                    protocol::Request::Drop => None,
                }
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
