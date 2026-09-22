//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use hickory_proto::op::{Message, ResponseCode};

use crate::{
    config::{Config, EcsConfig},
    ecs::{self, Context},
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
    ecs: EcsConfig,
}

impl Resolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self {
            upstream,
            timeout,
            ecs: EcsConfig::default(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self {
            upstream: config.upstream,
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
        }
    }

    pub async fn resolve(&self, bytes: &[u8], peer: IpAddr) -> Option<Reply> {
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
        let (mut outbound, context) = match Context::prepare(&query, peer, &self.ecs) {
            Ok(prepared) => prepared,
            Err(code) => {
                return Some(Reply {
                    message: protocol::error_response(&query, code),
                    udp_limit,
                });
            }
        };
        let result = tokio::time::timeout(self.timeout, async {
            let mut response = upstream::exchange(&outbound, self.upstream).await?;
            let retry = response.response_code == ResponseCode::Refused
                && context.outgoing.is_some_and(|ecs| ecs.source_prefix() > 0);
            if retry {
                let mut anonymous = context.outgoing.expect("nonzero ECS checked above");
                anonymous.set_source_prefix(0);
                anonymous.set_addr(if anonymous.addr().is_ipv4() {
                    "0.0.0.0".parse().unwrap()
                } else {
                    "::".parse().unwrap()
                });
                ecs::set_subnet(&mut outbound, Some(anonymous));
                response = upstream::exchange(&outbound, self.upstream).await?;
            }
            Ok::<_, anyhow::Error>((response, retry))
        })
        .await;
        let (mut message, scope) = match result {
            Ok(Ok((response, retry))) => {
                let scope = if retry {
                    None
                } else {
                    ecs::subnet(&response).map(|ecs| ecs.scope_prefix())
                };
                (response, scope)
            }
            _ => (
                protocol::error_response(&query, ResponseCode::ServFail),
                None,
            ),
        };
        context.finish(&query, &mut message, scope);
        Some(Reply { message, udp_limit })
    }
}
