//! Bounded, in-memory aggregate history; no query names or client identities.
use std::{collections::VecDeque, time::Instant};

use serde::Serialize;

use crate::metrics::Snapshot;

pub(super) const INTERVAL: u64 = 60;
const SAMPLES: usize = 1440;

#[derive(Default)]
pub(super) struct History {
    points: VecDeque<Point>,
    previous: Option<Baseline>,
}

struct Baseline {
    at: Instant,
    generation: u64,
    snapshot: Option<Snapshot>,
}

#[derive(Serialize)]
struct Point {
    timestamp_ms: u64,
    elapsed_seconds: f64,
    running: bool,
    generation: u64,
    requests: u64,
    cache_hits: u64,
    cache_misses: u64,
    blocked: u64,
    upstream_failures: u64,
    latency_count: u64,
    latency_sum_micros: u64,
}

impl History {
    pub(super) fn record(
        &mut self,
        at: Instant,
        timestamp_ms: u64,
        generation: u64,
        snapshot: Option<Snapshot>,
    ) {
        let mut point = Point {
            timestamp_ms,
            elapsed_seconds: 0.0,
            running: snapshot.is_some(),
            generation,
            requests: 0,
            cache_hits: 0,
            cache_misses: 0,
            blocked: 0,
            upstream_failures: 0,
            latency_count: 0,
            latency_sum_micros: 0,
        };
        if let (Some(old), Some(current)) = (&self.previous, &snapshot)
            && old.generation == generation
            && let Some(previous) = &old.snapshot
        {
            point.elapsed_seconds = at.saturating_duration_since(old.at).as_secs_f64();
            let delta = |name| {
                current
                    .counters
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(previous.counters.get(name).copied().unwrap_or(0))
            };
            point.requests = delta("requests");
            point.cache_hits = delta("cache_hits");
            point.cache_misses = delta("cache_misses");
            point.blocked = delta("query_blocked").saturating_add(delta("response_blocked"));
            point.upstream_failures = delta("upstream_failures");
            point.latency_count = current
                .request_latency
                .count
                .saturating_sub(previous.request_latency.count);
            point.latency_sum_micros = current
                .request_latency
                .sum_micros
                .saturating_sub(previous.request_latency.sum_micros);
        }
        // A zero-duration point marks startup/restart/unavailability, not a
        // measured zero-traffic interval. Consumers must leave a chart gap.
        self.previous = Some(Baseline {
            at,
            generation,
            snapshot,
        });
        while self
            .points
            .front()
            .is_some_and(|p| timestamp_ms.saturating_sub(p.timestamp_ms) >= 86_400_000)
        {
            self.points.pop_front();
        }
        if self.points.len() == SAMPLES {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    pub(super) fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "interval_seconds": INTERVAL,
            "retention_seconds": 86400,
            "samples": self.points,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Counter, Metrics, Timer};
    use std::time::Duration;

    #[test]
    fn intervals_are_deltas_and_empty_history_is_explicit() {
        let mut history = History::default();
        assert_eq!(history.view()["samples"], serde_json::json!([]));
        let metrics = Metrics::default();
        let at = Instant::now();
        history.record(at, 1000, 1, Some(metrics.snapshot()));
        metrics.inc(Counter::CacheHits);
        metrics.inc(Counter::QueryBlocked);
        metrics.inc(Counter::ResponseBlocked);
        metrics.track(Timer::Request).complete();
        history.record(
            at + Duration::from_secs(60),
            61_000,
            1,
            Some(metrics.snapshot()),
        );
        let point = &history.points[1];
        assert_eq!(point.requests, 1);
        assert_eq!(point.cache_hits, 1);
        assert_eq!(point.blocked, 2);
        assert_eq!(point.latency_count, 1);
        assert_eq!(point.elapsed_seconds, 60.0);
        history.record(
            at + Duration::from_secs(120),
            121_000,
            1,
            Some(metrics.snapshot()),
        );
        assert_eq!(history.points[2].requests, 0);
        assert_eq!(history.points[2].elapsed_seconds, 60.0);
    }

    #[test]
    fn restart_and_unavailable_periods_are_gaps_not_negative_rates() {
        let mut history = History::default();
        let metrics = Metrics::default();
        let at = Instant::now();
        metrics.inc(Counter::Requests);
        history.record(at, 1000, 1, Some(metrics.snapshot()));
        history.record(
            at + Duration::from_secs(60),
            61_000,
            2,
            Some(Metrics::default().snapshot()),
        );
        assert_eq!(history.points[1].elapsed_seconds, 0.0);
        assert_eq!(history.points[1].requests, 0);
        history.record(at + Duration::from_secs(120), 121_000, 2, None);
        assert!(!history.points[2].running);
        history.record(
            at + Duration::from_secs(180),
            181_000,
            2,
            Some(metrics.snapshot()),
        );
        assert_eq!(history.points[3].elapsed_seconds, 0.0);
    }

    #[test]
    fn count_and_retention_remain_bounded_even_with_clock_changes() {
        let mut history = History::default();
        let at = Instant::now();
        for _ in 0..1500 {
            history.record(at, 1000, 0, None);
        }
        assert_eq!(history.points.len(), SAMPLES);
        history.record(at, 86_401_000, 0, None);
        assert_eq!(history.points.len(), 1);
    }
}
