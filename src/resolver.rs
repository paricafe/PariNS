//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
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
    query_log::{Entry, QueryLog, Trace},
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
    pub padding: protocol::Padding,
}

pub struct Resolver {
    upstream: Arc<crate::upstreams::Pool>,
    timeout: Duration,
    ecs: EcsConfig,
    generation: RwLock<Arc<Generation>>,
    coalescing: CoalescingConfig,
    metrics: Arc<Metrics>,
    policy: RwLock<Policy>,
    query_log: Arc<QueryLog>,
    services: Arc<crate::runtime_services::RuntimeServices>,
    retired_refresh: Mutex<Vec<Arc<refresh::Refresh>>>,
    shutdown_clean: AtomicBool,
    shutdown_forced: AtomicBool,
}

impl Resolver {
    pub fn from_config(config: &Config) -> Self {
        Self::try_from_config(config).expect("validated resolver configuration")
    }

    pub fn try_from_config(config: &Config) -> anyhow::Result<Self> {
        let services = crate::runtime_services::RuntimeServices::ephemeral(
            crate::storage::RuntimeSettings::from_config(config),
        );
        Self::with_services(config, services)
    }

    pub fn with_services(
        config: &Config,
        services: Arc<crate::runtime_services::RuntimeServices>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            upstream: Arc::new(crate::upstreams::Pool::new(&config.upstreams, config)?),
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
            generation: RwLock::new(Arc::new(Generation::new(
                config.cache.clone(),
                &config.coalescing,
            ))),
            coalescing: config.coalescing.clone(),
            metrics: services.metrics.clone(),
            policy: RwLock::new(config.load_policy()?),
            query_log: services.query_log.clone(),
            services,
            retired_refresh: Mutex::new(Vec::new()),
            shutdown_clean: AtomicBool::new(false),
            shutdown_forced: AtomicBool::new(false),
        })
    }

    pub async fn resolve(&self, bytes: &[u8], peer: IpAddr) -> Option<Reply> {
        self.resolve_with_transport(bytes, peer, "unknown").await
    }

    pub fn query_log(&self) -> Arc<QueryLog> {
        self.query_log.clone()
    }

    /// Admission failures do not enter the resolver pipeline, but remain visible
    /// in opt-in history. Transport adapters pass the actual response or drop.
    pub(crate) fn log_rejected(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
        reply: Option<&Message>,
    ) {
        if let Some(epoch) = self.query_log.begin() {
            let mut entry = Entry::request(protocol::decode(bytes).ok().as_ref(), peer, transport);
            entry.finish(reply, Duration::ZERO, Trace::default());
            self.query_log.record(epoch, entry);
        }
    }

    pub async fn resolve_with_transport(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> Option<Reply> {
        let mut guard = self.metrics.track(Timer::Request);
        let log = self.query_log.pending(bytes, peer, transport);
        let mut trace = Trace::default();
        let reply = self.resolve_inner(bytes, peer, &mut trace).await;
        if let Some(log) = log {
            log.finish(reply.as_ref().map(|r| &r.message), trace);
        }
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

    pub fn services(&self) -> &Arc<crate::runtime_services::RuntimeServices> {
        &self.services
    }
    pub fn policy_digest(&self) -> [u8; 32] {
        self.policy
            .read()
            .expect("policy lock poisoned")
            .semantic_digest()
    }
    pub(crate) fn force_shutdown(&self) {
        self.shutdown_forced.store(true, Ordering::Release);
        self.upstream.shutdown();
    }
    pub fn upstream_diagnostics(&self) -> serde_json::Value {
        self.upstream.diagnostics_snapshot()
    }
    pub(crate) fn finish_shutdown(&self) {
        self.shutdown_clean.store(
            !self.shutdown_forced.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
    pub fn is_quiescent(&self) -> bool {
        self.shutdown_clean.load(Ordering::Acquire)
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
        let mut retired = self.retired_refresh.lock().expect("retired refresh lock");
        retired.retain(|refresh| refresh.snapshot().active > 0);
        retired.push(current.refresh.clone());
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
        self.upstream.shutdown();
        let generation = self
            .generation
            .read()
            .expect("cache generation poisoned")
            .clone();
        generation.refresh.shutdown().await;
        let retired =
            std::mem::take(&mut *self.retired_refresh.lock().expect("retired refresh lock"));
        for refresh in retired {
            refresh.shutdown().await;
        }
    }

    async fn resolve_inner(&self, bytes: &[u8], peer: IpAddr, trace: &mut Trace) -> Option<Reply> {
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
                    padding: protocol::decode(bytes)
                        .as_ref()
                        .map(protocol::Padding::from_query)
                        .unwrap_or_default(),
                });
            }
            Request::Forward(query) => query,
        };
        let udp_limit = protocol::udp_limit(&query);
        let padding = protocol::Padding::from_query(&query);
        let (outbound, context) = match Context::prepare(&query, peer, &self.ecs) {
            Ok(prepared) => prepared,
            Err(code) => {
                return Some(Reply {
                    message: protocol::error_response(&query, code),
                    udp_limit,
                    padding,
                });
            }
        };
        if policy.blocks_query(&query) {
            trace.cache = Some("blocked");
            self.metrics.inc(Counter::QueryBlocked);
            let mut message = protocol::error_response(&query, ResponseCode::NoError);
            context.finish(&query, &mut message, None);
            return Some(Reply {
                message,
                udp_limit,
                padding,
            });
        }
        let cached = cache.lookup_with_decision(&query, context.outgoing, Instant::now(), false);
        self.metrics.record_cache_lookup(cached.decision);
        trace.cache_lookup = Some(cached.decision);
        if let Some(hit) = cached.hit {
            trace.cache_scope = Some(hit.scope.tag());
            trace.cache = Some("fresh");
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
                    trace_enabled: self.query_log.begin().is_some(),
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
                trace.cache = Some("blocked");
                self.metrics.inc(Counter::ResponseBlocked);
            }
            context.finish(&query, &mut message, scope.reply_scope());
            return Some(Reply {
                message,
                udp_limit,
                padding,
            });
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
            trace_enabled: self.query_log.begin().is_some(),
        };
        let work = exchange.run(false).boxed();
        let flights = generation.flights(epoch, &self.coalescing);
        let result = match flights.join(&wire_query, work) {
            Ok((future, role)) => {
                self.metrics.inc(flight_counter(&role));
                let result = future.await;
                if !result.cached {
                    use crate::query_log::UpstreamRelation as Relation;
                    trace.upstream_relation = Some(match role {
                        Role::Leader => Relation::Leader,
                        Role::Bypass => Relation::Bypass,
                        Role::Joined if result.prefetch => Relation::PrefetchFollower,
                        Role::Joined => Relation::Follower,
                    });
                }
                result
            }
            Err(()) => {
                self.metrics.inc(Counter::FlightRejected);
                Answer {
                    response: Err(()),
                    stale_eligible: false,
                    admitted: false,
                    upstream: None,
                    outgoing_ecs: None,
                    cached: false,
                    cache_store: None,
                    cache_scope: None,
                    upstream_trace: None,
                    failure_stage: None,
                    failure_reason: None,
                    prefetch: false,
                }
            }
        };
        trace.cache = Some(if result.cached {
            "fresh"
        } else if result.response.is_err() {
            "error"
        } else {
            "upstream"
        });
        trace.upstream = result.upstream;
        trace.outgoing_ecs = result.outgoing_ecs;
        trace.cache_store = result.cache_store;
        trace.cache_scope = result.cache_scope;
        trace.upstream_trace = result.upstream_trace;
        trace.failure_stage = result.failure_stage;
        trace.failure_reason = result.failure_reason;
        // Fallback belongs to each foreground consumer, never to the shared work:
        // background refresh reports only actual admissions, and stale-hit counts
        // describe replies even when many callers shared one failed exchange.
        let response = if result.stale_eligible {
            cache
                .lookup_with_decision(&query, context.outgoing, Instant::now(), true)
                .hit
                .map(|hit| {
                    let decision = crate::cache::LookupDecision {
                        outcome: if hit.stale {
                            crate::cache::LookupOutcome::Stale
                        } else {
                            crate::cache::LookupOutcome::Fresh
                        },
                        reason: None,
                    };
                    self.metrics.record_cache_lookup(decision);
                    trace.cache_lookup = Some(decision);
                    trace.cache_scope = Some(hit.scope.tag());
                    trace.cache = Some(if hit.stale { "stale" } else { "fresh" });
                    (hit.message, hit.scope.reply_scope())
                })
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
            trace.cache = Some("blocked");
            self.metrics.inc(Counter::ResponseBlocked);
        }
        context.finish(&query, &mut message, scope);
        Some(Reply {
            message,
            udp_limit,
            padding,
        })
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
    upstream: Arc<crate::upstreams::Pool>,
    timeout: Duration,
    cache: Arc<Cache>,
    query: Message,
    context: Context,
    outbound: Message,
    epoch: u64,
    metrics: Arc<Metrics>,
    trace_enabled: bool,
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
                response: Ok((hit.message, hit.scope.reply_scope())),
                stale_eligible: false,
                admitted: false,
                upstream: None,
                outgoing_ecs: None,
                cached: true,
                cache_store: None,
                cache_scope: Some(hit.scope.tag()),
                upstream_trace: None,
                failure_stage: None,
                failure_reason: None,
                prefetch: refreshing,
            };
        }
        let _timer = self.metrics.track(Timer::Upstream);
        let metrics = self.metrics.clone();
        let operation = crate::upstreams::diagnostics::Operation::new(
            self.trace_enabled,
            Some(Arc::new(move |attempt| {
                metrics.record_upstream_attempt(attempt)
            })),
        );
        let mut retried = false;
        let mut endpoint = None;
        let deadline = tokio::time::Instant::now() + self.timeout;
        let result = tokio::time::timeout_at(deadline, async {
            let exchange = self
                .upstream
                .exchange_observed(&self.outbound, deadline, &operation)
                .await?;
            endpoint = Some(exchange.upstream);
            let mut response = exchange.message;
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
                endpoint = None;
                let exchange = self
                    .upstream
                    .exchange_observed(&self.outbound, deadline, &operation)
                    .await?;
                endpoint = Some(exchange.upstream);
                response = exchange.message;
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
            Ok(Err(error)) => {
                if error.is::<tokio::time::error::Elapsed>() {
                    self.metrics.inc(Counter::UpstreamTimeouts);
                }
                true
            }
        };
        if failed {
            self.metrics.inc(Counter::UpstreamFailures);
        }
        let upstream_trace = operation.trace();
        // A valid DNS SERVFAIL is not a fabricated transport failure. Retain
        // actual operation failures even when a foreground caller serves stale.
        let failure =
            (!matches!(&result, Ok(Ok(_))))
                .then(|| {
                    upstream_trace
                        .as_ref()
                        .and_then(|trace| {
                            trace.attempts.iter().rev().find(|a| {
                                a.outcome == crate::upstreams::diagnostics::Outcome::Failed
                            })
                        })
                        .map(|a| (a.stage, a.reason))
                })
                .flatten();
        match result {
            Ok(Ok(response)) => {
                let decision = self.context.response_scope(&response);
                let store = (!retried).then(|| {
                    let store = decision.cache.map_or_else(
                        || self.cache.record_unusable_scope(),
                        |scope| {
                            self.cache.insert_decision_if_epoch(
                                &self.query,
                                &response,
                                scope,
                                Instant::now(),
                                self.epoch,
                            )
                        },
                    );
                    self.metrics.record_cache_store(store);
                    store
                });
                let admitted = store.is_some_and(|store| store.admitted());
                let scope = if retried { None } else { decision.reply };
                Answer {
                    response: Ok((response, scope)),
                    admitted,
                    stale_eligible: failed && !retried,
                    upstream: endpoint,
                    outgoing_ecs: crate::query_log::subnet(&self.outbound),
                    cached: false,
                    cache_store: store,
                    cache_scope: if retried {
                        None
                    } else {
                        decision.cache.map(|scope| scope.tag())
                    },
                    upstream_trace,
                    failure_stage: failure.map(|f| f.0),
                    failure_reason: failure.and_then(|f| f.1),
                    prefetch: refreshing,
                }
            }
            _ => Answer {
                response: Err(()),
                admitted: false,
                stale_eligible: failed && !retried,
                upstream: endpoint,
                outgoing_ecs: crate::query_log::subnet(&self.outbound),
                cached: false,
                cache_store: None,
                cache_scope: None,
                upstream_trace,
                failure_stage: failure.map(|f| f.0),
                failure_reason: failure.and_then(|f| f.1),
                prefetch: refreshing,
            },
        }
    }
}
