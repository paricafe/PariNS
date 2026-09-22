//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use hickory_proto::op::{Message, ResponseCode};

use crate::{
    cache::Cache,
    config::{CacheConfig, CoalescingConfig, Config, EcsConfig},
    ecs::{self, Context},
    flight::{Answer, Flights, Role},
    metrics::{Counter, Metrics, Timer},
    policy::Policy,
    protocol::{self, Request},
};

mod refresh;
pub use refresh::Snapshot as RefreshSnapshot;

struct Generation {
    cache: Arc<Cache>,
    flights: Mutex<(u64, Arc<Flights>)>,
    refresh: Arc<refresh::Refresh>,
}

impl Generation {
    fn new(cache: CacheConfig, coalescing: &CoalescingConfig) -> Self {
        Self {
            cache: Arc::new(Cache::new(cache)),
            flights: Mutex::new((0, Arc::new(Flights::new(coalescing.clone())))),
            refresh: Arc::new(refresh::Refresh::default()),
        }
    }

    fn flights(&self, epoch: u64, config: &CoalescingConfig) -> Arc<Flights> {
        let mut current = self.flights.lock().expect("flight generation poisoned");
        if current.0 == epoch {
            return current.1.clone();
        }
        let flights = Arc::new(Flights::new(config.clone()));
        // A request captured before invalidation must not move the registry backwards.
        if epoch > current.0 {
            *current = (epoch, flights.clone());
        }
        flights
    }
}

pub struct Reply {
    pub message: Message,
    pub udp_limit: usize,
}

pub struct Resolver {
    upstream: crate::scheduler::Client,
    timeout: Duration,
    ecs: EcsConfig,
    generation: RwLock<Arc<Generation>>,
    coalescing: CoalescingConfig,
    metrics: Arc<Metrics>,
    policy: RwLock<Policy>,
}

impl Resolver {
    pub fn new(upstream: SocketAddr, timeout: Duration) -> Self {
        Self {
            upstream: crate::scheduler::Client::new(upstream, None, None).expect("single upstream"),
            timeout,
            ecs: EcsConfig::default(),
            generation: RwLock::new(Arc::new(Generation::new(
                CacheConfig::default(),
                &CoalescingConfig::default(),
            ))),
            coalescing: CoalescingConfig::default(),
            metrics: Arc::new(Metrics::default()),
            policy: RwLock::new(Policy::default()),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::try_from_config(config).expect("validated resolver configuration")
    }

    pub fn try_from_config(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            upstream: crate::scheduler::Client::new(
                config.upstream,
                config
                    .upstream_tls
                    .as_ref()
                    .map(|settings| {
                        crate::tls::Upstream::with_pool(settings, &config.upstream_pool)
                    })
                    .transpose()?,
                config.scheduler.clone(),
            )?,
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
            generation: RwLock::new(Arc::new(Generation::new(
                config.cache.clone(),
                &config.coalescing,
            ))),
            coalescing: config.coalescing.clone(),
            metrics: Arc::new(Metrics::default()),
            policy: RwLock::new(config.load_policy()?),
        })
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

    pub fn replace_policy(&self, policy: Policy) {
        *self.policy.write().expect("policy lock poisoned") = policy;
    }

    pub fn cache(&self) -> Arc<Cache> {
        self.generation
            .read()
            .expect("cache generation poisoned")
            .cache
            .clone()
    }

    /// Cache configuration changes publish a new namespace, without restarting listeners.
    pub fn replace_cache(&self, config: CacheConfig) {
        let next = Arc::new(Generation::new(config, &self.coalescing));
        let mut current = self.generation.write().expect("cache generation poisoned");
        current.refresh.cancel();
        *current = next;
    }

    pub fn refresh_snapshot(&self) -> RefreshSnapshot {
        self.generation
            .read()
            .expect("cache generation poisoned")
            .refresh
            .snapshot()
    }

    pub async fn shutdown_refresh(&self) {
        let generation = self
            .generation
            .read()
            .expect("cache generation poisoned")
            .clone();
        generation.refresh.shutdown().await;
    }

    async fn resolve_inner(&self, bytes: &[u8], peer: IpAddr) -> Option<Reply> {
        // One immutable generation governs this request, including across awaits.
        // The Arc-backed trie clone releases the lock before parsing or network IO.
        let policy = self.policy.read().expect("policy lock poisoned").clone();
        let generation = self
            .generation
            .read()
            .expect("cache generation poisoned")
            .clone();
        let cache = generation.cache.clone();
        let epoch = cache.epoch();
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
        let (outbound, context) = match Context::prepare(&query, peer, &self.ecs) {
            Ok(prepared) => prepared,
            Err(code) => {
                return Some(Reply {
                    message: protocol::error_response(&query, code),
                    udp_limit,
                });
            }
        };
        if policy.blocks_query(&query) {
            self.metrics.inc(Counter::QueryBlocked);
            let mut message = protocol::error_response(&query, ResponseCode::NoError);
            context.finish(&query, &mut message, None);
            return Some(Reply { message, udp_limit });
        }
        let cached = cache.lookup(&query, context.outgoing, Instant::now(), false);
        if let Some(hit) = cached {
            self.metrics.inc(Counter::CacheHits);
            if hit.refresh {
                let work = Exchange {
                    upstream: self.upstream.clone(),
                    timeout: self.timeout,
                    cache: cache.clone(),
                    query: query.clone(),
                    context: context.clone(),
                    outbound: outbound.clone(),
                    epoch,
                    metrics: self.metrics.clone(),
                };
                let flights = generation.flights(epoch, &self.coalescing);
                let metrics = self.metrics.clone();
                generation
                    .refresh
                    .schedule(&outbound, epoch, &cache.config().prefetch, || {
                        // Explicitly disabling coalescing also disables sharing with
                        // foreground misses; the refresh owner's own limits still apply.
                        let joined = flights.join(&outbound, work.run(true).boxed());
                        metrics.inc(match &joined {
                            Ok((_, role)) => flight_counter(role),
                            Err(()) => Counter::FlightRejected,
                        });
                        async move {
                            match joined {
                                Ok((future, _)) => future.await.admitted,
                                Err(()) => false,
                            }
                        }
                    });
            }
            let (mut message, scope) = (hit.message, hit.scope);
            if policy.apply_response(&query, &mut message) {
                self.metrics.inc(Counter::ResponseBlocked);
            }
            context.finish(&query, &mut message, Some(scope.prefix_len()));
            return Some(Reply { message, udp_limit });
        }
        self.metrics.inc(Counter::CacheMisses);
        let wire_query = outbound.clone();
        let exchange = Exchange {
            upstream: self.upstream.clone(),
            timeout: self.timeout,
            cache: cache.clone(),
            query: query.clone(),
            context: context.clone(),
            outbound,
            epoch,
            metrics: self.metrics.clone(),
        };
        let work = exchange.run(false).boxed();
        let flights = generation.flights(epoch, &self.coalescing);
        let result = match flights.join(&wire_query, work) {
            Ok((future, role)) => {
                self.metrics.inc(flight_counter(&role));
                future.await
            }
            Err(()) => {
                self.metrics.inc(Counter::FlightRejected);
                Answer {
                    response: Err(()),
                    stale_eligible: false,
                    admitted: false,
                }
            }
        };
        // Fallback belongs to each foreground consumer, never to the shared work:
        // background refresh reports only actual admissions, and stale-hit counts
        // describe replies even when many callers shared one failed exchange.
        let response = if result.stale_eligible {
            cache
                .lookup(&query, context.outgoing, Instant::now(), true)
                .map(|hit| (hit.message, Some(hit.scope.prefix_len())))
                .map(Ok)
                .unwrap_or(result.response)
        } else {
            result.response
        };
        let (mut message, scope) = response.unwrap_or_else(|()| {
            (
                protocol::error_response(&query, ResponseCode::ServFail),
                None,
            )
        });
        // Cache retains the original upstream response; policy applies equally
        // on misses and hits and never inserts its synthesized answer.
        if policy.apply_response(&query, &mut message) {
            self.metrics.inc(Counter::ResponseBlocked);
        }
        context.finish(&query, &mut message, scope);
        Some(Reply { message, udp_limit })
    }
}

fn flight_counter(role: &Role) -> Counter {
    match role {
        Role::Bypass => Counter::FlightBypassed,
        Role::Leader => Counter::FlightLeaders,
        Role::Joined => Counter::FlightJoined,
    }
}

/// Network work is shared by the foreground and refresh paths; only foreground
/// failures may consult stale data. A privacy retry never uses the original scope.
struct Exchange {
    upstream: crate::scheduler::Client,
    timeout: Duration,
    cache: Arc<Cache>,
    query: Message,
    context: Context,
    outbound: Message,
    epoch: u64,
    metrics: Arc<Metrics>,
}

impl Exchange {
    async fn run(mut self, refreshing: bool) -> Answer {
        // Close miss/admission and scheduled-refresh/replacement races without
        // counting a second lookup or re-fetching an already renewed entry.
        if let Some(hit) =
            self.cache
                .peek(&self.query, self.context.outgoing, Instant::now(), false)
            && (!refreshing || !hit.refresh)
        {
            return Answer {
                response: Ok((hit.message, Some(hit.scope.prefix_len()))),
                stale_eligible: false,
                admitted: false,
            };
        }
        let _timer = self.metrics.track(Timer::Upstream);
        let mut retried = false;
        let result = tokio::time::timeout(self.timeout, async {
            let mut response = self.upstream.exchange(&self.outbound).await?;
            if response.response_code == ResponseCode::Refused
                && self
                    .context
                    .outgoing
                    .is_some_and(|ecs| ecs.source_prefix() > 0)
            {
                retried = true;
                self.metrics.inc(Counter::EcsRetries);
                let mut anonymous = self.context.outgoing.expect("nonzero ECS checked above");
                anonymous.set_source_prefix(0);
                anonymous.set_addr(if anonymous.addr().is_ipv4() {
                    "0.0.0.0".parse().unwrap()
                } else {
                    "::".parse().unwrap()
                });
                ecs::set_subnet(&mut self.outbound, Some(anonymous));
                response = self.upstream.exchange(&self.outbound).await?;
            }
            Ok::<_, anyhow::Error>(response)
        })
        .await;
        let failed = match &result {
            Ok(Ok(response)) => response.response_code == ResponseCode::ServFail,
            Err(_) => {
                self.metrics.inc(Counter::UpstreamTimeouts);
                true
            }
            Ok(Err(_)) => true,
        };
        if failed {
            self.metrics.inc(Counter::UpstreamFailures);
        }
        match result {
            Ok(Ok(response)) => {
                let admitted = !retried
                    && self.context.cache_scope(&response).is_some_and(|scope| {
                        self.cache.insert_if_epoch(
                            &self.query,
                            &response,
                            scope,
                            Instant::now(),
                            self.epoch,
                        )
                    });
                let scope = if retried {
                    None
                } else {
                    ecs::subnet(&response).map(|ecs| ecs.scope_prefix())
                };
                Answer {
                    response: Ok((response, scope)),
                    admitted,
                    stale_eligible: failed && !retried,
                }
            }
            _ => Answer {
                response: Err(()),
                admitted: false,
                stale_eligible: failed && !retried,
            },
        }
    }
}
