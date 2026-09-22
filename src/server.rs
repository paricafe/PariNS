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

use crate::{config::Config, protocol, resolver::Resolver, transport::tcp};

pub struct Server {
    config: Config,
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
}

impl Server {
    /// Bind both protocols before accepting traffic. Port zero selects one shared port.
    pub async fn bind(config: Config) -> Result<Self> {
        config.validate()?;
        let tcp = TcpListener::bind(config.listen).await?;
        let udp = Arc::new(UdpSocket::bind(tcp.local_addr()?).await?);
        Ok(Self { config, udp, tcp })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.tcp.local_addr()?)
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let resolver = Arc::new(Resolver::new(
            self.config.upstream,
            Duration::from_millis(self.config.query_timeout_ms),
        ));
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
                Some(result) = tasks.join_next(), if !tasks.is_empty() => { result?; }
                accepted = self.tcp.accept() => {
                    let (stream, _) = accepted?;
                    let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                    let resolver = resolver.clone();
                    let queries = queries.clone();
                    let stop = stop.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        serve_tcp(stream, resolver, queries, stop, io_timeout).await;
                    });
                }
                received = self.udp.recv_from(&mut buffer) => {
                    let (length, peer) = received?;
                    let Ok(permit) = queries.clone().try_acquire_owned() else { continue };
                    let bytes = buffer[..length].to_vec();
                    let socket = self.udp.clone();
                    let resolver = resolver.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        if let Some(reply) = resolver.resolve(&bytes).await
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
        if let Ok(result) = drained {
            result?;
        }
        Ok(())
    }
}

async fn serve_tcp(
    mut stream: TcpStream,
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
        let permit = queries.clone().try_acquire_owned();
        let response = match permit {
            Ok(ref _permit) => resolver.resolve(&bytes).await.map(|reply| reply.message),
            Err(_) => match protocol::request(&bytes) {
                protocol::Request::Forward(query) => {
                    Some(protocol::error_response(&query, ResponseCode::ServFail))
                }
                protocol::Request::Reply(reply) => Some(reply),
                protocol::Request::Drop => None,
            },
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
