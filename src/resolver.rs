//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use hickory_proto::op::{Message, ResponseCode};

use crate::{
    cache::Cache,
    config::{CacheConfig, Config, EcsConfig},
    ecs::{self, Context},
    policy::Policy,
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
    cache: Mutex<Cache>,
    policy: Policy,
}

impl Resolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self {
            upstream,
            timeout,
            ecs: EcsConfig::default(),
            cache: Mutex::new(Cache::new(CacheConfig::default())),
            policy: Policy::default(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self {
            upstream: config.upstream,
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
            cache: Mutex::new(Cache::new(config.cache.clone())),
            policy: config.filter.clone(),
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
        if self.policy.blocks_query(&query) {
            let mut message = protocol::error_response(&query, ResponseCode::NoError);
            context.finish(&query, &mut message, None);
            return Some(Reply { message, udp_limit });
        }
        if let Some((mut message, scope)) = self.cache.lock().expect("cache lock poisoned").get(
            &query,
            context.outgoing,
            Instant::now(),
        ) {
            self.policy.apply_response(&query, &mut message);
            context.finish(&query, &mut message, Some(scope.prefix_len()));
            return Some(Reply { message, udp_limit });
        }
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
                if !retry && let Some(scope) = context.cache_scope(&response) {
                    self.cache.lock().expect("cache lock poisoned").insert(
                        &query,
                        &response,
                        scope,
                        Instant::now(),
                    );
                }
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
        // Cache retains the original upstream response; policy applies equally
        // on misses and hits and never inserts its synthesized answer.
        self.policy.apply_response(&query, &mut message);
        context.finish(&query, &mut message, scope);
        Some(Reply { message, udp_limit })
    }
}
