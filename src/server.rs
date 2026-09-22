//! Listener ownership, admission limits, and task lifetime.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{net::UdpSocket, sync::Semaphore, task::JoinSet};

use crate::{config::Config, protocol, resolver::Resolver};

pub async fn run(config: Config, shutdown: impl Future<Output = ()>) -> Result<()> {
    config.validate()?;
    let socket = Arc::new(UdpSocket::bind(config.listen).await?);
    let resolver = Arc::new(Resolver::new(
        config.upstream,
        Duration::from_millis(config.query_timeout_ms),
    ));
    let budget = Arc::new(Semaphore::new(config.max_inflight));
    let mut tasks = JoinSet::new();
    let mut buffer = vec![0; protocol::MAX_MESSAGE];
    tokio::pin!(shutdown);
    eprintln!("PariNS listening on {} (UDP)", socket.local_addr()?);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => { result?; }
            received = socket.recv_from(&mut buffer) => {
                let (length, peer) = received?;
                let Ok(permit) = budget.clone().try_acquire_owned() else { continue };
                let bytes = buffer[..length].to_vec();
                let socket = socket.clone();
                let resolver = resolver.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Some(reply) = resolver.resolve(&bytes).await
                        && let Ok(bytes) = protocol::encode_udp(&reply.message, reply.udp_limit)
                    {
                        let _ = socket.send_to(&bytes, peer).await;
                    }
                });
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_millis(config.shutdown_grace_ms), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.shutdown().await;
    Ok(())
}
