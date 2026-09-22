//! Fixed-cardinality process metrics. Concurrent snapshots are approximate, not transactional.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Instant,
};

use serde::Serialize;

const BOUNDS: [u64; 8] = [
    1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
];
const NAMES: [&str; 31] = [
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

#[derive(Default)]
pub struct Metrics {
    counters: [AtomicU64; NAMES.len()],
    inflight: [AtomicU64; 2],
    latency: [Histogram; 2],
}

impl Metrics {
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
                .zip(self.counters.iter().map(|v| v.load(Relaxed)))
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

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub counters: BTreeMap<&'static str, u64>,
    pub request_inflight: u64,
    pub upstream_inflight: u64,
    pub request_latency: HistogramSnapshot,
    pub upstream_latency: HistogramSnapshot,
}

#[derive(Debug, Serialize)]
pub struct HistogramSnapshot {
    pub buckets: [Bucket; 9],
    pub count: u64,
    pub sum_micros: u64,
}

#[derive(Debug, Serialize)]
pub struct Bucket {
    /// Inclusive upper bound; `None` represents positive infinity.
    pub upper_bound_micros: Option<u64>,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_increment_independently() {
        let metrics = Metrics::default();
        assert_eq!(metrics.snapshot().counters.len(), NAMES.len());
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
