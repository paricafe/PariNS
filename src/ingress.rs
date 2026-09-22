//! Shared admission and DNS handling for encrypted transport adapters.
use crate::{metrics::Counter, protocol, resolver::Resolver};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, watch};

#[derive(Clone)]
pub struct Ingress {
    pub resolver: Arc<Resolver>,
    pub queries: Arc<Semaphore>,
    pub connections: Arc<Semaphore>,
    pub stop: watch::Receiver<bool>,
    pub io_timeout: Duration,
    pub shutdown_grace: Duration,
    pub max_streams: usize,
}

impl Ingress {
    pub async fn handle(&self, bytes: &[u8], peer: IpAddr) -> Option<Vec<u8>> {
        self.resolver.metrics().inc(Counter::EncryptedReceived);
        let permit = self.queries.clone().try_acquire_owned();
        let message = match permit {
            Ok(ref _permit) => self.resolver.resolve(bytes, peer).await.map(|r| r.message),
            Err(_) => {
                self.resolver.metrics().inc(Counter::EncryptedRejected);
                match protocol::request(bytes) {
                    protocol::Request::Forward(q) => Some(protocol::error_response(
                        &q,
                        hickory_proto::op::ResponseCode::ServFail,
                    )),
                    protocol::Request::Reply(r) => Some(r),
                    protocol::Request::Drop => None,
                }
            }
        };
        message?.to_vec().ok()
    }
}
