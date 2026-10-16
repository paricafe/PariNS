//! Shared admission and DNS handling for encrypted transport adapters.
use crate::limits::{Denied, Limiter, Permit};
use crate::{
    metrics::Counter,
    protocol,
    resolver::{ForcedShutdown, Resolver},
};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

#[derive(Clone)]
pub struct Ingress {
    pub resolver: Arc<Resolver>,
    pub queries: Arc<Semaphore>,
    pub connections: Arc<Semaphore>,
    pub source_limits: Arc<Limiter>,
    pub stop: watch::Receiver<bool>,
    pub io_timeout: Duration,
    pub shutdown_grace: Duration,
    pub max_streams: usize,
}

impl Ingress {
    pub fn admit_connection(&self, peer: IpAddr) -> Option<Permit> {
        self.source_limits
            .try_connection(peer)
            .map_err(|reason| {
                self.rejected(reason, Counter::SourceConnectionsRejected);
            })
            .ok()
    }

    pub fn admit_query(&self, peer: IpAddr) -> Option<Permit> {
        self.source_limits
            .try_query(peer)
            .map_err(|reason| {
                self.rejected(reason, Counter::SourceQueriesRejected);
            })
            .ok()
    }

    /// Resolves once the generation starts stopping (or its stop owner is gone).
    /// Adapters race it against receiving a query: a stream without a complete
    /// query owns no resolver work and is cancelled instead of being drained.
    pub async fn stopped(&self) {
        let mut stop = self.stop.clone();
        let _ = stop.wait_for(|stopping| *stopping).await;
    }

    /// Waits up to the shutdown grace for adapter tasks, then aborts the rest.
    /// Only a stopping generation records `cause`: forcing also shuts down the
    /// shared upstream pool, which must keep serving when one listener exits on
    /// an error and is restarted.
    pub async fn drain(&self, tasks: &mut JoinSet<()>, cause: ForcedShutdown) {
        if timeout(self.shutdown_grace, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            if *self.stop.borrow() {
                self.resolver.force_shutdown(cause);
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }

    fn rejected(&self, reason: Denied, counter: Counter) {
        self.resolver.metrics().inc(counter);
        if matches!(reason, Denied::TableFull) {
            self.resolver.metrics().inc(Counter::SourceTableFull);
        }
    }

    pub async fn handle_with_transport(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> Option<Vec<u8>> {
        self.resolver.metrics().inc(Counter::EncryptedReceived);
        let Some(_source) = self.admit_query(peer) else {
            self.resolver.metrics().inc(Counter::EncryptedRejected);
            let reply = rejected_response(bytes);
            self.resolver
                .log_rejected(bytes, peer, transport, reply.as_ref());
            return encode_rejected(bytes, &reply?);
        };
        let permit = self.queries.clone().try_acquire_owned();
        match permit {
            Ok(ref _permit) => self
                .resolver
                .resolve_with_transport(bytes, peer, transport)
                .await
                .and_then(|r| r.padding.encode_response(&r.message, true).ok()),
            Err(_) => {
                self.resolver.metrics().inc(Counter::EncryptedRejected);
                let reply = rejected_response(bytes);
                self.resolver
                    .log_rejected(bytes, peer, transport, reply.as_ref());
                reply.and_then(|reply| encode_rejected(bytes, &reply))
            }
        }
    }
}

fn encode_rejected(bytes: &[u8], reply: &hickory_proto::op::Message) -> Option<Vec<u8>> {
    let padding = protocol::decode(bytes)
        .as_ref()
        .map(protocol::Padding::from_query)
        .unwrap_or_default();
    padding.encode_response(reply, true).ok()
}

pub(crate) fn rejected_response(bytes: &[u8]) -> Option<hickory_proto::op::Message> {
    match protocol::request(bytes) {
        protocol::Request::Forward(q) => Some(protocol::error_response(
            &q,
            hickory_proto::op::ResponseCode::ServFail,
        )),
        protocol::Request::Reply(r) => Some(r),
        protocol::Request::Drop => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingress(stopping: bool) -> (watch::Sender<bool>, Ingress) {
        let config = crate::config::Config::parse(include_str!("../parins.example.toml")).unwrap();
        let (stop, receiver) = watch::channel(stopping);
        let ingress = Ingress {
            resolver: Arc::new(Resolver::from_config(&config)),
            queries: Arc::new(Semaphore::new(1)),
            connections: Arc::new(Semaphore::new(1)),
            source_limits: Arc::new(Limiter::new(&config.source_limits).unwrap()),
            stop: receiver,
            io_timeout: Duration::from_secs(1),
            shutdown_grace: Duration::from_millis(20),
            max_streams: 1,
        };
        (stop, ingress)
    }

    #[tokio::test]
    async fn drain_forces_only_a_stopping_generation() {
        for stopping in [false, true] {
            let (_stop, ingress) = ingress(stopping);
            let mut tasks = JoinSet::new();
            tasks.spawn(std::future::pending());
            ingress.drain(&mut tasks, ForcedShutdown::QuicDrain).await;
            assert!(tasks.is_empty());
            // An error exit outside a stop keeps the shared upstream pool serving.
            assert_eq!(
                ingress.resolver.forced_shutdown(),
                stopping.then_some(ForcedShutdown::QuicDrain)
            );
        }
    }
}
