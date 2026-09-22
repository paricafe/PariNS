//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use hickory_proto::op::{Message, ResponseCode};

use crate::{
    cache::Cache,
    config::{CacheConfig, CoalescingConfig, Config, EcsConfig},
    ecs::{self, Context},
    flight::{Flights, Role},
    metrics::{Counter, Metrics, Timer},
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
    cache: Arc<Mutex<Cache>>,
    flights: Flights,
    metrics: Arc<Metrics>,
    policy: Policy,
}

impl Resolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self {
            upstream,
            timeout,
            ecs: EcsConfig::default(),
            cache: Arc::new(Mutex::new(Cache::new(CacheConfig::default()))),
            flights: Flights::new(CoalescingConfig::default()),
            metrics: Arc::new(Metrics::default()),
            policy: Policy::default(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self {
            upstream: config.upstream,
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
            cache: Arc::new(Mutex::new(Cache::new(config.cache.clone()))),
            flights: Flights::new(config.coalescing.clone()),
            metrics: Arc::new(Metrics::default()),
            policy: config.filter.clone(),
        }
    }

    pub async fn resolve(&self, bytes: &[u8], peer: IpAddr) -> Option<Reply> {
        let mut guard = self.metrics.track(Timer::Request);
        let reply = self.resolve_inner(bytes, peer).await;
        self.metrics
            .inc(match reply.as_ref().map(|r| r.message.response_code) {
                Some(ResponseCode::NoError) => Counter::ResponsesNoerror,
                Some(ResponseCode::NXDomain) => Counter::ResponsesNxdomain,
                Some(ResponseCode::ServFail) => Counter::ResponsesServfail,
                Some(ResponseCode::Refused) => Counter::ResponsesRefused,
                Some(_) => Counter::ResponsesOther,
                None => Counter::Dropped,
            });
        guard.complete();
        reply
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    async fn resolve_inner(&self, bytes: &[u8], peer: IpAddr) -> Option<Reply> {
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
            self.metrics.inc(Counter::QueryBlocked);
            let mut message = protocol::error_response(&query, ResponseCode::NoError);
            context.finish(&query, &mut message, None);
            return Some(Reply { message, udp_limit });
        }
        let cached = self.cache.lock().expect("cache lock poisoned").get(
            &query,
            context.outgoing,
            Instant::now(),
        );
        if let Some((mut message, scope)) = cached {
            self.metrics.inc(Counter::CacheHits);
            if self.policy.apply_response(&query, &mut message) {
                self.metrics.inc(Counter::ResponseBlocked);
            }
            context.finish(&query, &mut message, Some(scope.prefix_len()));
            return Some(Reply { message, udp_limit });
        }
        self.metrics.inc(Counter::CacheMisses);
        let wire_query = outbound.clone();
        let cache = self.cache.clone();
        let cache_query = query.clone();
        let cache_context = context.clone();
        let metrics = self.metrics.clone();
        let upstream = self.upstream;
        let timeout = self.timeout;
        let work = async move {
            // Close the cache-miss/admission race if another group completed meanwhile.
            let cached = cache.lock().expect("cache lock poisoned").get(
                &cache_query,
                cache_context.outgoing,
                Instant::now(),
            );
            if let Some((message, scope)) = cached {
                return Ok((message, Some(scope.prefix_len())));
            }
            let _timer = metrics.track(Timer::Upstream);
            let result = tokio::time::timeout(timeout, async {
                let mut response = upstream::exchange(&outbound, upstream).await?;
                let retry = response.response_code == ResponseCode::Refused
                    && cache_context
                        .outgoing
                        .is_some_and(|ecs| ecs.source_prefix() > 0);
                if retry {
                    metrics.inc(Counter::EcsRetries);
                    let mut anonymous = cache_context.outgoing.expect("nonzero ECS checked above");
                    anonymous.set_source_prefix(0);
                    anonymous.set_addr(if anonymous.addr().is_ipv4() {
                        "0.0.0.0".parse().unwrap()
                    } else {
                        "::".parse().unwrap()
                    });
                    ecs::set_subnet(&mut outbound, Some(anonymous));
                    response = upstream::exchange(&outbound, upstream).await?;
                }
                Ok::<_, anyhow::Error>((response, retry))
            })
            .await;
            match result {
                Ok(Ok((response, retry))) => {
                    if !retry && let Some(scope) = cache_context.cache_scope(&response) {
                        cache.lock().expect("cache lock poisoned").insert(
                            &cache_query,
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
                    Ok((response, scope))
                }
                Err(_) => {
                    metrics.inc(Counter::UpstreamTimeouts);
                    metrics.inc(Counter::UpstreamFailures);
                    Err(())
                }
                Ok(Err(_)) => {
                    metrics.inc(Counter::UpstreamFailures);
                    Err(())
                }
            }
        }
        .boxed();
        let result = match self.flights.join(&wire_query, work) {
            Ok((future, role)) => {
                self.metrics.inc(match role {
                    Role::Bypass => Counter::FlightBypassed,
                    Role::Leader => Counter::FlightLeaders,
                    Role::Joined => Counter::FlightJoined,
                });
                future.await
            }
            Err(()) => {
                self.metrics.inc(Counter::FlightRejected);
                Err(())
            }
        };
        let (mut message, scope) = result.unwrap_or_else(|()| {
            (
                protocol::error_response(&query, ResponseCode::ServFail),
                None,
            )
        });
        // Cache retains the original upstream response; policy applies equally
        // on misses and hits and never inserts its synthesized answer.
        if self.policy.apply_response(&query, &mut message) {
            self.metrics.inc(Counter::ResponseBlocked);
        }
        context.finish(&query, &mut message, scope);
        Some(Reply { message, udp_limit })
    }
}
