//! Shared admission and DNS handling for encrypted transport adapters.
use crate::limits::{Denied, Limiter, Permit};
use crate::{
    metrics::{Counter, Diagnostic},
    protocol,
    resolver::{Delivery, ForcedShutdown, Resolver},
};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    sync::{Semaphore, watch},
    task::{JoinError, JoinSet},
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
    pub fn settle<T>(&self, listener: &str, result: Result<T, JoinError>) {
        if let Err(error) = result
            && error.is_panic()
        {
            self.resolver.metrics().record(Diagnostic::QueryTaskPanics);
            eprintln!("PariNS {listener} task panicked; listener keeps serving: {error}");
        }
    }

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

    pub async fn stopped(&self) {
        let mut stop = self.stop.clone();
        let _ = stop.wait_for(|stopping| *stopping).await;
    }

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
        let (response, delivery) = self.handle_for_delivery(bytes, peer, transport).await;
        if let Some(delivery) = delivery {
            delivery.delivered();
        }
        response
    }

    pub(crate) async fn handle_for_delivery(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> (Option<Vec<u8>>, Option<Delivery<'_>>) {
        self.resolver.metrics().inc(Counter::EncryptedReceived);
        let Some(_source) = self.admit_query(peer) else {
            self.resolver.metrics().inc(Counter::EncryptedRejected);
            return self.rejected_reply(bytes, peer, transport);
        };
        match self.queries.clone().try_acquire_owned() {
            Ok(_permit) => {
                let (reply, delivery) = self
                    .resolver
                    .resolve_for_delivery(bytes, peer, transport)
                    .await;
                (
                    reply.and_then(|r| r.padding.encode_response(&r.message, true).ok()),
                    Some(delivery),
                )
            }
            Err(_) => {
                self.resolver.metrics().inc(Counter::EncryptedRejected);
                self.rejected_reply(bytes, peer, transport)
            }
        }
    }

    fn rejected_reply(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> (Option<Vec<u8>>, Option<Delivery<'_>>) {
        let reply = rejected_response(bytes);
        let delivery = self.resolver.rejected_for_delivery(
            bytes,
            peer,
            transport,
            reply.as_ref().map(|(r, _)| r),
        );
        (
            reply.and_then(|(reply, padding)| padding.encode_response(&reply, true).ok()),
            Some(delivery),
        )
    }
}

pub(crate) fn rejected_response(
    bytes: &[u8],
) -> Option<(hickory_proto::op::Message, protocol::Padding)> {
    match protocol::request(bytes) {
        protocol::Request::Forward(q) => Some((
            protocol::error_response(&q, hickory_proto::op::ResponseCode::ServFail),
            protocol::Padding::from_query(&q),
        )),
        protocol::Request::Reply(r, padding) => Some((r, padding)),
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
            source_limits: Arc::new(Limiter::new(&config.source_limits)),
            stop: receiver,
            io_timeout: Duration::from_secs(1),
            shutdown_grace: Duration::from_millis(20),
            max_streams: 1,
        };
        (stop, ingress)
    }

    #[tokio::test]
    async fn a_panicked_task_is_counted_and_a_cancelled_one_is_not() {
        let (_stop, ingress) = ingress(false);
        let panics = || {
            ingress
                .resolver
                .metrics()
                .diagnostics()
                .find(|(name, ..)| *name == "query_task_panics")
                .unwrap()
                .2
        };
        let mut tasks = JoinSet::new();
        tasks.spawn(async { panic!("test connection task panic") });
        ingress.settle("test", tasks.join_next().await.unwrap());
        assert_eq!(panics(), 1);
        tasks.spawn(std::future::pending::<()>()).abort();
        ingress.settle("test", tasks.join_next().await.unwrap());
        assert_eq!(panics(), 1);
    }

    #[tokio::test]
    async fn drain_forces_only_a_stopping_generation() {
        for stopping in [false, true] {
            let (_stop, ingress) = ingress(stopping);
            let mut tasks = JoinSet::new();
            tasks.spawn(std::future::pending());
            ingress.drain(&mut tasks, ForcedShutdown::QuicDrain).await;
            assert!(tasks.is_empty());
            assert_eq!(
                ingress.resolver.forced_shutdown(),
                stopping.then_some(ForcedShutdown::QuicDrain)
            );
        }
    }
}
