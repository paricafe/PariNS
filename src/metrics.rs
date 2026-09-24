//! Fixed-cardinality process metrics. Concurrent snapshots are approximate, not transactional.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Instant,
};

use serde::{Deserialize, Serialize};

const BOUNDS: [u64; 8] = [
    1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
];
const NAMES: [&str; 50] = [
    "requests",
    "completed",
    "cancelled",
    "cache_hits",
    "cache_misses",
    "query_blocked",
    "response_blocked",
    "upstream_operations",
    "upstream_failures",
    "upstream_timeouts",
    "ecs_retries",
    "flight_leaders",
    "flight_joined",
    "flight_rejected",
    "flight_bypassed",
    "udp_received",
    "tcp_received",
    "udp_dropped",
    "tcp_rejected",
    "connections_rejected",
    "responses_noerror",
    "responses_servfail",
    "responses_refused",
    "responses_nxdomain",
    "responses_other",
    "dropped",
    "encrypted_received",
    "encrypted_rejected",
    "source_queries_rejected",
    "source_connections_rejected",
    "source_table_full",
    "cache_lookup_fresh",
    "cache_lookup_stale",
    "cache_lookup_miss",
    "cache_lookup_bypass",
    "cache_lookup_disabled",
    "cache_store_admitted",
    "cache_store_replaced",
    "cache_store_skipped",
    "cache_store_superseded_only",
    "cache_reason_unsupported_query",
    "cache_reason_unsupported_edns",
    "cache_reason_ecs_unusable",
    "cache_reason_ttl_zero",
    "cache_reason_negative_without_soa",
    "cache_reason_uncacheable_response",
    "cache_reason_policy_disabled",
    "cache_reason_capacity",
    "cache_reason_variant_limit",
    "cache_reason_epoch_changed",
];

#[derive(Clone, Copy, Debug)]
pub enum Counter {
    Requests,
    Completed,
    Cancelled,
    CacheHits,
    CacheMisses,
    QueryBlocked,
    ResponseBlocked,
    UpstreamOperations,
    UpstreamFailures,
    UpstreamTimeouts,
    EcsRetries,
    FlightLeaders,
    FlightJoined,
    FlightRejected,
    FlightBypassed,
    UdpReceived,
    TcpReceived,
    UdpDropped,
    TcpRejected,
    ConnectionsRejected,
    ResponsesNoerror,
    ResponsesServfail,
    ResponsesRefused,
    ResponsesNxdomain,
    ResponsesOther,
    Dropped,
    EncryptedReceived,
    EncryptedRejected,
    SourceQueriesRejected,
    SourceConnectionsRejected,
    SourceTableFull,
    CacheLookupFresh,
    CacheLookupStale,
    CacheLookupMiss,
    CacheLookupBypass,
    CacheLookupDisabled,
    CacheStoreAdmitted,
    CacheStoreReplaced,
    CacheStoreSkipped,
    CacheStoreSupersededOnly,
    CacheReasonUnsupportedQuery,
    CacheReasonUnsupportedEdns,
    CacheReasonEcsUnusable,
    CacheReasonTtlZero,
    CacheReasonNegativeWithoutSoa,
    CacheReasonUncacheableResponse,
    CacheReasonPolicyDisabled,
    CacheReasonCapacity,
    CacheReasonVariantLimit,
    CacheReasonEpochChanged,
}

#[derive(Clone, Copy, Debug)]
pub enum Timer {
    Request,
    Upstream,
}

#[derive(Default)]
struct Histogram {
    // Exclusive buckets permit a snapshot to derive consistent cumulative bucket counts.
    buckets: [AtomicU64; 9],
    sum_micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, micros: u64) {
        let index = BOUNDS.partition_point(|bound| *bound < micros);
        self.sum_micros.fetch_add(micros, Relaxed);
        self.buckets[index].fetch_add(1, Relaxed);
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut count = 0;
        let buckets = std::array::from_fn(|index| {
            count += self.buckets[index].load(Relaxed);
            Bucket {
                upper_bound_micros: BOUNDS.get(index).copied(),
                count,
            }
        });
        HistogramSnapshot {
            buckets,
            count,
            sum_micros: self.sum_micros.load(Relaxed),
        }
    }
}

pub struct Metrics {
    pub quic: crate::quic::diagnostics::Counters,
    upstream_attempts: [AtomicU64; 4],
    upstream_reasons: [AtomicU64; 9],
    counters: [AtomicU64; NAMES.len()],
    inflight: [AtomicU64; 2],
    latency: [Histogram; 2],
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            quic: Default::default(),
            upstream_attempts: Default::default(),
            upstream_reasons: Default::default(),
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            inflight: Default::default(),
            latency: Default::default(),
        }
    }
}

impl Metrics {
    pub fn record_upstream_attempt(&self, attempt: &crate::upstreams::diagnostics::AttemptRecord) {
        use crate::upstreams::diagnostics::{Outcome, Reason};
        let outcome = match attempt.outcome {
            Outcome::Succeeded => 0,
            Outcome::Failed => 1,
            Outcome::Cancelled => 2,
            Outcome::Skipped => 3,
        };
        self.upstream_attempts[outcome].fetch_add(1, Relaxed);
        if let Some(reason) = attempt.reason {
            let index = match reason {
                Reason::Deadline => 0,
                Reason::ConnectIo => 1,
                Reason::Tls => 2,
                Reason::HttpStatus => 3,
                Reason::ProtocolInvalid => 4,
                Reason::PeerClosed => 5,
                Reason::CallerCancelled => 6,
                Reason::Shutdown => 7,
                Reason::Internal => 8,
            };
            self.upstream_reasons[index].fetch_add(1, Relaxed);
        }
    }
    fn upstream_counters(&self) -> impl Iterator<Item = (String, u64)> + '_ {
        ["succeeded", "failed", "cancelled", "skipped"]
            .into_iter()
            .zip(&self.upstream_attempts)
            .map(|(name, value)| (format!("upstream_attempt_{name}"), value.load(Relaxed)))
            .chain(
                [
                    "deadline",
                    "connect_io",
                    "tls",
                    "http_status",
                    "protocol_invalid",
                    "peer_closed",
                    "caller_cancelled",
                    "shutdown",
                    "internal",
                ]
                .into_iter()
                .zip(&self.upstream_reasons)
                .map(|(name, value)| {
                    (
                        format!("upstream_attempt_reason_{name}"),
                        value.load(Relaxed),
                    )
                }),
            )
    }
    pub fn record_cache_lookup(&self, decision: crate::cache::LookupDecision) {
        use crate::cache::LookupOutcome;
        self.inc(match decision.outcome {
            LookupOutcome::Fresh => Counter::CacheLookupFresh,
            LookupOutcome::Stale => Counter::CacheLookupStale,
            LookupOutcome::Miss => Counter::CacheLookupMiss,
            LookupOutcome::Bypass => Counter::CacheLookupBypass,
            LookupOutcome::Disabled => Counter::CacheLookupDisabled,
        });
        if let Some(reason) = decision.reason {
            self.record_cache_reason(reason);
        }
    }
    pub fn record_cache_store(&self, decision: crate::cache::StoreDecision) {
        use crate::cache::StoreOutcome;
        self.inc(match decision.outcome {
            StoreOutcome::Admitted => Counter::CacheStoreAdmitted,
            StoreOutcome::Replaced => Counter::CacheStoreReplaced,
            StoreOutcome::Skipped => Counter::CacheStoreSkipped,
            StoreOutcome::SupersededOnly => Counter::CacheStoreSupersededOnly,
        });
        if let Some(reason) = decision.reason {
            self.record_cache_reason(reason);
        }
    }
    fn record_cache_reason(&self, reason: crate::cache::DecisionReason) {
        use crate::cache::DecisionReason;
        self.inc(match reason {
            DecisionReason::UnsupportedQuery => Counter::CacheReasonUnsupportedQuery,
            DecisionReason::UnsupportedEdns => Counter::CacheReasonUnsupportedEdns,
            DecisionReason::EcsUnusable => Counter::CacheReasonEcsUnusable,
            DecisionReason::TtlZero => Counter::CacheReasonTtlZero,
            DecisionReason::NegativeWithoutSoa => Counter::CacheReasonNegativeWithoutSoa,
            DecisionReason::UncacheableResponse => Counter::CacheReasonUncacheableResponse,
            DecisionReason::PolicyDisabled => Counter::CacheReasonPolicyDisabled,
            DecisionReason::Capacity => Counter::CacheReasonCapacity,
            DecisionReason::VariantLimit => Counter::CacheReasonVariantLimit,
            DecisionReason::EpochChanged => Counter::CacheReasonEpochChanged,
        });
    }
    pub fn inc(&self, counter: Counter) {
        self.counters[counter as usize].fetch_add(1, Relaxed);
    }

    pub fn track(&self, timer: Timer) -> Guard<'_> {
        self.inc(match timer {
            Timer::Request => Counter::Requests,
            Timer::Upstream => Counter::UpstreamOperations,
        });
        self.inflight[timer as usize].fetch_add(1, Relaxed);
        Guard {
            metrics: self,
            timer,
            started: Instant::now(),
            completed: false,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            counters: NAMES
                .into_iter()
                .map(str::to_owned)
                .zip(self.counters.iter().map(|v| v.load(Relaxed)))
                .chain(self.quic.counters())
                .chain(self.upstream_counters())
                .collect(),
            request_inflight: self.inflight[Timer::Request as usize].load(Relaxed),
            upstream_inflight: self.inflight[Timer::Upstream as usize].load(Relaxed),
            request_latency: self.latency[Timer::Request as usize].snapshot(),
            upstream_latency: self.latency[Timer::Upstream as usize].snapshot(),
        }
    }
}

pub struct Guard<'a> {
    metrics: &'a Metrics,
    timer: Timer,
    started: Instant,
    completed: bool,
}

impl Guard<'_> {
    /// Mark a request as resolved (including DNS error responses), rather than cancelled.
    pub fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let micros = self.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.metrics.latency[self.timer as usize].observe(micros);
        if matches!(self.timer, Timer::Request) {
            self.metrics.inc(if self.completed {
                Counter::Completed
            } else {
                Counter::Cancelled
            });
        }
        self.metrics.inflight[self.timer as usize].fetch_sub(1, Relaxed);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub counters: BTreeMap<String, u64>,
    pub request_inflight: u64,
    pub upstream_inflight: u64,
    pub request_latency: HistogramSnapshot,
    pub upstream_latency: HistogramSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    pub buckets: [Bucket; 9],
    pub count: u64,
    pub sum_micros: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bucket {
    /// Inclusive upper bound; `None` represents positive infinity.
    pub upper_bound_micros: Option<u64>,
    pub count: u64,
}

impl Snapshot {
    /// Durable counters exclude process-local gauges. Saturation makes overflow
    /// explicit to the sampler without ever manufacturing negative traffic.
    pub fn delta(&self, previous: &Self) -> Self {
        let mut result = self.clone();
        for (name, value) in &mut result.counters {
            *value = value.saturating_sub(previous.counters.get(name).copied().unwrap_or(0));
        }
        result.request_inflight = 0;
        result.upstream_inflight = 0;
        result.request_latency = self.request_latency.delta(&previous.request_latency);
        result.upstream_latency = self.upstream_latency.delta(&previous.upstream_latency);
        result
    }

    pub fn plus(&self, other: &Self) -> Self {
        let mut result = self.clone();
        for (name, value) in &other.counters {
            let target = result.counters.entry(name.clone()).or_default();
            *target = target.saturating_add(*value);
        }
        result.request_inflight = 0;
        result.upstream_inflight = 0;
        result.request_latency = self.request_latency.plus(&other.request_latency);
        result.upstream_latency = self.upstream_latency.plus(&other.upstream_latency);
        result
    }

    pub fn regressed(&self, previous: &Self) -> bool {
        self.counters
            .iter()
            .any(|(name, value)| *value < previous.counters.get(name).copied().unwrap_or(0))
            || self.request_latency.count < previous.request_latency.count
            || self.request_latency.sum_micros < previous.request_latency.sum_micros
            || self.upstream_latency.count < previous.upstream_latency.count
            || self.upstream_latency.sum_micros < previous.upstream_latency.sum_micros
    }
}

impl HistogramSnapshot {
    fn delta(&self, previous: &Self) -> Self {
        Self {
            buckets: std::array::from_fn(|i| Bucket {
                upper_bound_micros: self.buckets[i].upper_bound_micros,
                count: self.buckets[i]
                    .count
                    .saturating_sub(previous.buckets[i].count),
            }),
            count: self.count.saturating_sub(previous.count),
            sum_micros: self.sum_micros.saturating_sub(previous.sum_micros),
        }
    }
    fn plus(&self, other: &Self) -> Self {
        Self {
            buckets: std::array::from_fn(|i| Bucket {
                upper_bound_micros: self.buckets[i].upper_bound_micros,
                count: self.buckets[i].count.saturating_add(other.buckets[i].count),
            }),
            count: self.count.saturating_add(other.count),
            sum_micros: self.sum_micros.saturating_add(other.sum_micros),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_increment_independently() {
        let metrics = Metrics::default();
        assert_eq!(
            metrics.snapshot().counters.len(),
            NAMES.len() + metrics.quic.counters().len() + 13
        );
        assert!(
            metrics
                .snapshot()
                .counters
                .values()
                .all(|value| *value == 0)
        );
        metrics.inc(Counter::CacheHits);
        metrics.inc(Counter::CacheHits);
        metrics.inc(Counter::ConnectionsRejected);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.counters["cache_hits"], 2);
        assert_eq!(snapshot.counters["cache_misses"], 0);
        assert_eq!(snapshot.counters["connections_rejected"], 1);
    }

    #[test]
    fn guards_account_for_completion_cancellation_and_inflight() {
        let metrics = Metrics::default();
        let mut completed = metrics.track(Timer::Request);
        let cancelled = metrics.track(Timer::Request);
        let upstream = metrics.track(Timer::Upstream);
        assert_eq!(metrics.snapshot().request_inflight, 2);
        assert_eq!(metrics.snapshot().upstream_inflight, 1);
        completed.complete();
        completed.complete();
        drop((completed, cancelled, upstream));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.counters["requests"], 2);
        assert_eq!(snapshot.counters["completed"], 1);
        assert_eq!(snapshot.counters["cancelled"], 1);
        assert_eq!(snapshot.counters["upstream_operations"], 1);
        assert_eq!(snapshot.request_inflight, 0);
        assert_eq!(snapshot.upstream_inflight, 0);
        assert_eq!(snapshot.request_latency.count, 2);
        assert_eq!(snapshot.upstream_latency.count, 1);
    }

    #[test]
    fn histogram_boundaries_are_inclusive_and_cumulative() {
        let histogram = Histogram::default();
        let observations = [0, 1_000, 1_001, 5_000_000, 5_000_001];
        for micros in observations {
            histogram.observe(micros);
        }
        let snapshot = histogram.snapshot();
        assert_eq!(
            snapshot.buckets.each_ref().map(|bucket| bucket.count),
            [2, 3, 3, 3, 3, 3, 3, 4, 5]
        );
        assert_eq!(snapshot.count, 5);
        assert_eq!(snapshot.sum_micros, observations.into_iter().sum::<u64>());
        assert_eq!(snapshot.buckets[0].upper_bound_micros, Some(1_000));
        assert_eq!(snapshot.buckets[8].upper_bound_micros, None);
    }
}
