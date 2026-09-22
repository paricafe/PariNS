//! Shared admission and DNS handling for encrypted transport adapters.
use crate::limits::{Denied, Limiter, Permit};
use crate::{metrics::Counter, protocol, resolver::Resolver};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, watch};

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

    fn rejected(&self, reason: Denied, counter: Counter) {
        self.resolver.metrics().inc(counter);
        if matches!(reason, Denied::TableFull) {
            self.resolver.metrics().inc(Counter::SourceTableFull);
        }
    }

    pub async fn handle(&self, bytes: &[u8], peer: IpAddr) -> Option<Vec<u8>> {
        self.handle_with_transport(bytes, peer, "unknown").await
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
            return reply?.to_vec().ok();
        };
        let permit = self.queries.clone().try_acquire_owned();
        let message = match permit {
            Ok(ref _permit) => self
                .resolver
                .resolve_with_transport(bytes, peer, transport)
                .await
                .map(|r| r.message),
            Err(_) => {
                self.resolver.metrics().inc(Counter::EncryptedRejected);
                let reply = rejected_response(bytes);
                self.resolver
                    .log_rejected(bytes, peer, transport, reply.as_ref());
                reply
            }
        };
        message?.to_vec().ok()
    }
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
