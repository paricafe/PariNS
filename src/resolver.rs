//! Shared query semantics and one deadline for the complete upstream operation.

use std::{net::SocketAddr, time::Duration};

use hickory_proto::op::{Message, ResponseCode};

use crate::{
    protocol::{self, Request},
    upstream,
};

pub struct Reply {
    pub message: Message,
    pub udp_limit: usize,
}

pub struct Resolver {
    upstream: SocketAddr,
    timeout: Duration,
}

impl Resolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self { upstream, timeout }
    }

    pub async fn resolve(&self, bytes: &[u8]) -> Option<Reply> {
        let query = match protocol::request(bytes) {
            Request::Drop => return None,
            Request::Reply(message) => {
                return Some(Reply {
                    message,
                    udp_limit: 512,
                });
            }
            Request::Forward(query) => query,
        };
        let udp_limit = protocol::udp_limit(&query);
        let result =
            tokio::time::timeout(self.timeout, upstream::exchange(&query, self.upstream)).await;
        let mut message = match result {
            Ok(Ok(response)) => response,
            _ => protocol::error_response(&query, ResponseCode::ServFail),
        };
        // Plain UDP/TCP does not authenticate the upstream's DNSSEC assertion.
        message.metadata.authentic_data = false;
        Some(Reply { message, udp_limit })
    }
}
