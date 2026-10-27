//! Shared query semantics and one deadline for the complete upstream operation.

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use hickory_proto::op::{Message, ResponseCode};

use crate::{
    cache::{Cache, Failure, LookupOutcome},
    config::{CacheConfig, CoalescingConfig, Config, EcsConfig, ResolutionMode},
    ecs::{self, Context},
    filter_subscriptions::handle::PolicyHandle,
    flight::{Answer, Flights, Hold, Role},
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

/// A resolved request until its transport hands the answer over. Dropping it
/// undelivered (a failed write, a peer that left, a deadline) counts the
/// request as cancelled and records its history entry so.
pub(crate) struct Delivery<'a> {
    metrics: &'a Metrics,
    log: Option<crate::query_log::Pending<'a>>,
    delivered: bool,
}

impl Delivery<'_> {
    pub(crate) fn delivered(mut self) {
        self.delivered = true;
        if let Some(log) = self.log.take() {
            log.deliver();
        }
    }
}

impl Drop for Delivery<'_> {
    fn drop(&mut self) {
        self.metrics.inc(if self.delivered {
            Counter::Completed
        } else {
            Counter::Cancelled
        });
    }
}

pub struct Reply {
    pub message: Message,
    pub udp_limit: usize,
    pub padding: protocol::Padding,
}

/// Why a shutdown stopped waiting for resolver work. Finite and free of
/// client data so that terminal diagnostics can name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ForcedShutdown {
    ListenerDrain = 1,
    ListenerFailed,
    DohDrain,
    DotDrain,
    QuicDrain,
    TaskAborted,
    TaskPanic,
    ManagedStopTimeout,
}

impl ForcedShutdown {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::ListenerDrain,
            2 => Self::ListenerFailed,
            3 => Self::DohDrain,
            4 => Self::DotDrain,
            5 => Self::QuicDrain,
            6 => Self::TaskAborted,
            7 => Self::TaskPanic,
            8 => Self::ManagedStopTimeout,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ListenerDrain => "listener_drain",
            Self::ListenerFailed => "listener_failed",
            Self::DohDrain => "doh_drain",
            Self::DotDrain => "dot_drain",
            Self::QuicDrain => "quic_drain",
            Self::TaskAborted => "task_aborted",
            Self::TaskPanic => "task_panic",
            Self::ManagedStopTimeout => "managed_stop_timeout",
        }
    }
}

/// Where cache misses are resolved; fixed for the life of a Resolver.
pub(crate) enum Source {
    Forward(Box<crate::upstreams::Pool>),
    Recursive(Box<crate::recursion::Recursion>),
}

impl Source {
    fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(match config.resolution.mode {
            ResolutionMode::Forward => Self::Forward(Box::new(crate::upstreams::Pool::new(
                &config.upstreams,
                config,
            )?)),
            ResolutionMode::Recursive => Self::Recursive(Box::new(
                crate::recursion::Recursion::new(&config.recursion)?,
            )),
        })
    }

    async fn exchange(
        &self,
        query: &Message,
        deadline: tokio::time::Instant,
        operation: &crate::upstreams::diagnostics::Operation,
    ) -> anyhow::Result<crate::upstreams::Exchange> {
        match self {
            Self::Forward(pool) => pool.exchange_observed(query, deadline, operation).await,
            Self::Recursive(recursion) => Ok(crate::upstreams::Exchange {
                message: {
                    // A started recursion has its queries in flight at once.
                    operation.mark_sent();
                    tokio::time::timeout_at(deadline, recursion.exchange(query)).await??
                },
                upstream: "recursive".into(),
            }),
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Forward(pool) => pool.shutdown(),
            Self::Recursive(recursion) => recursion.shutdown(),
        }
    }

    /// A DNSSEC validation failure is not an unreachable source: it is answered
    /// as SERVFAIL with EDE 6, never replaced by stale data.
    fn validation_failed(&self, response: &Message) -> bool {
        matches!(self, Self::Recursive(_)) && crate::recursion::is_bogus(response)
    }

    /// Only the validating recursor vouches for data; a forwarder's AD bit is not ours.
    fn authenticates(&self) -> bool {
        matches!(self, Self::Recursive(_))
    }
}

pub struct Resolver {
    upstream: Arc<Source>,
    timeout: Duration,
    ecs: EcsConfig,
    generation: RwLock<Arc<Generation>>,
    coalescing: CoalescingConfig,
    metrics: Arc<Metrics>,
    policy: Arc<PolicyHandle>,
    query_log: Arc<QueryLog>,
    retired_refresh: Mutex<Vec<Arc<refresh::Refresh>>>,
    shutdown_clean: AtomicBool,
    shutdown_forced: AtomicU8,
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
        anyhow::ensure!(
            config.filter_subscriptions.effective().next().is_none(),
            "subscription_material_missing: subscriptions require a prepared policy"
        );
        Self::with_services_and_policy(config, services, PolicyHandle::new(config.load_policy()?))
    }

    pub fn with_services_and_policy(
        config: &Config,
        services: Arc<crate::runtime_services::RuntimeServices>,
        policy: Arc<PolicyHandle>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            upstream: Arc::new(Source::new(config)?),
            timeout: Duration::from_millis(config.query_timeout_ms),
            ecs: config.ecs.clone(),
            generation: RwLock::new(Arc::new(Generation::new(
                config.cache.clone(),
                &config.coalescing,
            ))),
            coalescing: config.coalescing.clone(),
            metrics: services.metrics.clone(),
            policy,
            query_log: services.query_log.clone(),
            retired_refresh: Mutex::new(Vec::new()),
            shutdown_clean: AtomicBool::new(false),
            shutdown_forced: AtomicU8::new(0),
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
        let (reply, delivery) = self.resolve_for_delivery(bytes, peer, transport).await;
        delivery.delivered();
        reply
    }

    /// Resolves a query whose answer the caller writes afterwards. The request
    /// counts as completed, and its history entry as answered, only after
    /// `Delivery::delivered`.
    pub(crate) async fn resolve_for_delivery(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> (Option<Reply>, Delivery<'_>) {
        let mut guard = self.metrics.track(Timer::Request);
        let mut log = self.query_log.pending(bytes, peer, transport);
        let mut untraced = Trace::default();
        let trace = log.as_mut().map_or(&mut untraced, |log| &mut log.trace);
        let reply = self.resolve_inner(bytes, peer, trace).await;
        if let Some(log) = &mut log {
            log.resolve(reply.as_ref().map(|r| &r.message));
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
        guard.defer();
        (
            reply,
            Delivery {
                metrics: &self.metrics,
                log,
                delivered: false,
            },
        )
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn policy_digest(&self) -> [u8; 32] {
        self.policy.snapshot().policy.semantic_digest()
    }
    /// Keeps the first cause; later forces only repeat upstream cancellation.
    pub(crate) fn force_shutdown(&self, cause: ForcedShutdown) {
        let _ = self.shutdown_forced.compare_exchange(
            0,
            cause as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.upstream.shutdown();
    }
    pub(crate) fn forced_shutdown(&self) -> Option<ForcedShutdown> {
        ForcedShutdown::from_u8(self.shutdown_forced.load(Ordering::Acquire))
    }
    pub fn upstream_diagnostics(&self) -> Option<serde_json::Value> {
        match &*self.upstream {
            Source::Forward(pool) => Some(pool.diagnostics_snapshot()),
            Source::Recursive(_) => None,
        }
    }
    pub fn resolution_mode(&self) -> ResolutionMode {
        match &*self.upstream {
            Source::Forward(_) => ResolutionMode::Forward,
            Source::Recursive(_) => ResolutionMode::Recursive,
        }
    }
    pub fn recursion_metrics(&self) -> Vec<(&'static str, &'static str, u64, bool)> {
        match &*self.upstream {
            Source::Recursive(recursion) => recursion.metrics(),
            Source::Forward(_) => Vec::new(),
        }
    }
    pub fn recursion_diagnostics(&self) -> Option<serde_json::Value> {
        match &*self.upstream {
            Source::Recursive(recursion) => Some(recursion.diagnostics_snapshot()),
            Source::Forward(_) => None,
        }
    }
    pub(crate) fn finish_shutdown(&self) {
        self.shutdown_clean.store(
            self.shutdown_forced.load(Ordering::Acquire) == 0,
            Ordering::Release,
        );
    }
    pub fn is_quiescent(&self) -> bool {
        self.shutdown_clean.load(Ordering::Acquire)
    }

    pub fn replace_policy(&self, policy: Policy) -> anyhow::Result<()> {
        self.policy.try_publish(policy)
    }
    pub(crate) fn replace_policy_with(
        &self,
        policy: Policy,
        before_publish: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.policy.try_publish_with(policy, before_publish)
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
        // Keep the generation owner, not just its index, across every await.
        let policy_generation = self.policy.snapshot();
        let policy = &policy_generation.policy;
        let generation = self
            .generation
            .read()
            .expect("cache generation poisoned")
            .clone();
        let cache = generation.cache.clone();
        let epoch = cache.epoch();
        let query = match protocol::request(bytes) {
            Request::Drop => return None,
            Request::Reply(message, padding) => {
                return Some(Reply {
                    message,
                    udp_limit: 512,
                    padding,
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
        if let Some((mut message, block)) = policy.block_query(&query) {
            trace.cache = Some("blocked");
            trace.block = Some(block);
            self.metrics.inc(Counter::QueryBlocked);
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
        if let Some(mut hit) = cached.hit {
            trace.cache_scope = Some(hit.scope.tag());
            trace.cache = Some(if hit.failure.is_some() {
                "servfail"
            } else {
                "fresh"
            });
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
                    sent: Default::default(),
                };
                let sent = work.sent.clone();
                let flights = generation.flights(epoch, &self.coalescing);
                let metrics = self.metrics.clone();
                generation
                    .refresh
                    .schedule(&outbound, epoch, &cache.config().prefetch, || {
                        // Explicitly disabling coalescing also disables sharing with
                        // foreground misses; the refresh owner's own limits still apply.
                        let joined = flights.join(&outbound, work.run(true).boxed(), sent);
                        metrics.inc(match &joined {
                            Ok((_, role, _)) => flight_counter(role),
                            Err(()) => Counter::FlightRejected,
                        });
                        async move {
                            match joined {
                                Ok((mut hold, _, _)) => hold.result().await.admitted,
                                Err(()) => false,
                            }
                        }
                    });
            }
            // A cached failure replies with a usable stale answer, as the
            // failure it records would have; it sends no upstream query.
            if hit.failure == Some(Failure::StaleFallback)
                && let crate::cache::LookupResult {
                    hit: Some(stale),
                    decision,
                } = cache.lookup_with_decision(&query, context.outgoing, Instant::now(), true)
            {
                self.metrics.record_cache_lookup(decision);
                trace.cache_lookup = Some(decision);
                trace.cache_scope = Some(stale.scope.tag());
                trace.cache = Some(if stale.stale { "stale" } else { "fresh" });
                hit = stale;
            }
            let mut authentic = self.authentic(&query, hit.authentic);
            let (mut message, scope) = (hit.message, hit.scope);
            if let Some(block) = policy.apply_response(&query, &mut message) {
                trace.cache = Some("blocked");
                trace.block = Some(block);
                self.metrics.inc(Counter::ResponseBlocked);
                authentic = false;
            }
            context.finish(&query, &mut message, scope.reply_scope(context.outgoing));
            message.metadata.authentic_data = authentic;
            return Some(Reply {
                message,
                udp_limit,
                padding,
            });
        }
        self.metrics.inc(Counter::CacheMisses);
        let wire_query = outbound.clone();
        let sent = Arc::new(AtomicBool::new(false));
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
            sent: sent.clone(),
        };
        let work = exchange.run(false).boxed();
        let flights = generation.flights(epoch, &self.coalescing);
        let result = match flights.join(&wire_query, work, sent) {
            Ok((hold, role, sent)) => {
                self.metrics.inc(flight_counter(&role));
                let mut waiting = Waiting {
                    hold: Some(hold),
                    sent,
                    fill: (cached.decision.outcome == LookupOutcome::Miss).then(|| Fill {
                        refresh: &generation.refresh,
                        query: &wire_query,
                        epoch,
                        config: &cache.config().prefetch,
                    }),
                };
                let result = waiting.result().await;
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
            // The cache stores SERVFAIL only as a cached failure.
            if matches!(&result.response, Ok((m, _)) if m.response_code == ResponseCode::ServFail) {
                "servfail"
            } else {
                "fresh"
            }
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
        let fallback = result
            .stale_eligible
            .then(|| cache.lookup_with_decision(&query, context.outgoing, Instant::now(), true));
        let response = match fallback {
            Some(crate::cache::LookupResult {
                hit: Some(mut hit),
                decision,
            }) => {
                self.metrics.record_cache_lookup(decision);
                trace.cache_lookup = Some(decision);
                trace.cache_scope = Some(hit.scope.tag());
                trace.cache = Some(if hit.stale { "stale" } else { "fresh" });
                hit.message.metadata.authentic_data = hit.authentic;
                Ok((hit.message, hit.scope.reply_scope(context.outgoing)))
            }
            _ => result.response,
        };
        let (mut message, scope) = response.unwrap_or_else(|()| {
            (
                protocol::error_response(&query, ResponseCode::ServFail),
                None,
            )
        });
        // A source answer and a fresh cached answer carry their AD bit; stale
        // answers never do.
        let mut authentic = self.authentic(&query, message.metadata.authentic_data);
        // Cache retains the original upstream response; policy applies equally
        // on misses and hits and never inserts its synthesized answer.
        if let Some(block) = policy.apply_response(&query, &mut message) {
            trace.cache = Some("blocked");
            trace.block = Some(block);
            self.metrics.inc(Counter::ResponseBlocked);
            authentic = false;
        }
        context.finish(&query, &mut message, scope);
        message.metadata.authentic_data = authentic;
        Some(Reply {
            message,
            udp_limit,
            padding,
        })
    }
}

impl Resolver {
    /// AD is answered only to clients that signal DNSSEC awareness (RFC 6840 §5.7).
    fn authentic(&self, query: &Message, validated: bool) -> bool {
        self.upstream.authenticates()
            && validated
            && (query.metadata.authentic_data
                || query.edns.as_ref().is_some_and(|e| e.flags().dnssec_ok))
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
    upstream: Arc<Source>,
    timeout: Duration,
    cache: Arc<Cache>,
    query: Message,
    context: Context,
    outbound: Message,
    epoch: u64,
    metrics: Arc<Metrics>,
    trace_enabled: bool,
    /// Set once a query of this operation reached a source.
    sent: Arc<AtomicBool>,
}

/// A foreground caller's share of upstream work. A caller that leaves last,
/// after the query was sent, hands the work to the generation's bounded refresh
/// owner, which lets it finish only to fill the cache. Unsent work, work over
/// that budget and work of a stopped generation are cancelled with the caller.
struct Waiting<'a> {
    hold: Option<Hold>,
    sent: Arc<AtomicBool>,
    fill: Option<Fill<'a>>,
}

struct Fill<'a> {
    refresh: &'a Arc<refresh::Refresh>,
    query: &'a Message,
    epoch: u64,
    config: &'a crate::config::PrefetchConfig,
}

impl Waiting<'_> {
    async fn result(&mut self) -> Answer {
        let answer = self
            .hold
            .as_mut()
            .expect("unfinished upstream work")
            .result()
            .await;
        self.hold = None;
        answer
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        // Other holders keep shared work running without a refresh slot.
        if let (Some(hold), Some(fill)) = (self.hold.take(), self.fill.take())
            && self.sent.load(Ordering::Acquire)
            && let Some(mut hold) = hold.leave()
        {
            fill.refresh
                .schedule(fill.query, fill.epoch, fill.config, || async move {
                    hold.result().await.admitted
                });
        }
    }
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
            // Carry the stored AD bit like a fresh answer's; reply rules still decide it.
            let mut message = hit.message;
            message.metadata.authentic_data = hit.authentic;
            return Answer {
                response: Ok((message, hit.scope.reply_scope(self.context.outgoing))),
                // A cached failure lets each caller fall back like the failure would.
                stale_eligible: hit.failure == Some(Failure::StaleFallback),
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
        )
        .reporting_sent(self.sent.clone());
        let mut retried = false;
        let mut endpoint = None;
        let deadline = tokio::time::Instant::now() + self.timeout;
        // Each pool exchange enforces this deadline; Elapsed identifies a timeout.
        let result = async {
            let exchange = self
                .upstream
                .exchange(&self.outbound, deadline, &operation)
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
                    .exchange(&self.outbound, deadline, &operation)
                    .await?;
                endpoint = Some(exchange.upstream);
                response = exchange.message;
            }
            Ok::<_, anyhow::Error>(response)
        }
        .await;
        let bogus = result
            .as_ref()
            .is_ok_and(|response| self.upstream.validation_failed(response));
        let failed = match &result {
            Ok(response) => response.response_code == ResponseCode::ServFail,
            Err(error) => {
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
            result
                .is_err()
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
            Ok(response) => {
                let decision = self.context.response_scope(&response);
                let servfail = response.response_code == ResponseCode::ServFail;
                let store = (!retried).then(|| {
                    // A failure is cached only for the exact sent network.
                    let scope = if servfail {
                        self.context.source_scope()
                    } else {
                        decision.cache
                    };
                    let store = scope.map_or_else(
                        || self.cache.record_unusable_scope(),
                        |scope| {
                            if servfail {
                                self.cache.insert_failure_if_epoch(
                                    &self.query,
                                    &response,
                                    scope,
                                    Instant::now(),
                                    self.epoch,
                                    if bogus {
                                        Failure::Final
                                    } else {
                                        Failure::StaleFallback
                                    },
                                )
                            } else {
                                self.cache.insert_decision_if_epoch(
                                    &self.query,
                                    &response,
                                    scope,
                                    Instant::now(),
                                    self.epoch,
                                )
                            }
                        },
                    );
                    self.metrics.record_cache_store(store);
                    store
                });
                // Refresh and cache fill succeed only with an answer; a cached
                // failure is still a failure for their accounting.
                let admitted = !servfail && store.is_some_and(|store| store.admitted());
                let scope = if retried { None } else { decision.reply };
                Answer {
                    response: Ok((response, scope)),
                    admitted,
                    stale_eligible: failed && !retried && !bogus,
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
            Err(_) => Answer {
                response: Err(()),
                admitted: false,
                stale_eligible: !retried,
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
