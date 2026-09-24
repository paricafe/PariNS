//! A single sampler owns raw-offset/base arithmetic and both reset cut points.
use super::{Error, Result, Status, now_ms};
use crate::metrics::{Metrics, Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Totals {
    pub scope: String,
    pub epoch: u64,
    pub since_ms: u64,
    pub metrics: Snapshot,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Point {
    #[serde(skip, default = "Instant::now")]
    pub(super) sampled_at: Instant,
    pub history_epoch: u64,
    pub totals_epoch: u64,
    pub run_id: String,
    pub seq: u64,
    pub start_ms: u64,
    pub timestamp_ms: u64,
    pub elapsed_seconds: f64,
    pub running: bool,
    pub generation: u64,
    pub gap: Option<String>,
    pub requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub blocked: u64,
    pub upstream_failures: u64,
    pub latency_count: u64,
    pub latency_sum_micros: u64,
    pub metrics: Snapshot,
}
impl Point {
    fn update_summary(&mut self) {
        self.requests = self.metrics.counters["requests"];
        self.cache_hits = self.metrics.counters["cache_hits"];
        self.cache_misses = self.metrics.counters["cache_misses"];
        self.blocked = self.metrics.counters["query_blocked"]
            .saturating_add(self.metrics.counters["response_blocked"]);
        self.upstream_failures = self.metrics.counters["upstream_failures"];
        self.latency_count = self.metrics.request_latency.count;
        self.latency_sum_micros = self.metrics.request_latency.sum_micros;
    }
    pub(super) fn merge(&mut self, other: Self) {
        if self.gap.is_some()
            || other.gap.is_some()
            || self.history_epoch != other.history_epoch
            || self.totals_epoch != other.totals_epoch
            || self.run_id != other.run_id
            || self.running != other.running
            || self.generation != other.generation
        {
            self.gap = Some("aggregation_boundary".into());
        }
        self.timestamp_ms = self.timestamp_ms.max(other.timestamp_ms);
        self.elapsed_seconds += other.elapsed_seconds;
        self.metrics = self.metrics.plus(&other.metrics);
        self.update_summary();
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryOptions {
    pub range: Option<String>,
    pub from_ms: Option<u64>,
    pub to_ms: Option<u64>,
    pub max_points: Option<usize>,
}
impl HistoryOptions {
    pub fn validate(&self) -> Result<()> {
        if self
            .range
            .as_deref()
            .is_some_and(|r| !matches!(r, "1h" | "24h" | "7d" | "custom"))
        {
            return Err(Error::Invalid("invalid statistics range".into()));
        }
        if !(1..=1000).contains(&self.max_points.unwrap_or(1000)) {
            return Err(Error::Invalid(
                "statistics max_points must be 1..=1000".into(),
            ));
        }
        if self.from_ms.zip(self.to_ms).is_some_and(|(a, b)| a >= b) {
            return Err(Error::Invalid(
                "statistics range must have from_ms < to_ms".into(),
            ));
        }
        if self.range.as_deref() == Some("custom")
            && (self.from_ms.is_none() || self.to_ms.is_none())
        {
            return Err(Error::Invalid(
                "custom statistics range requires from_ms and to_ms".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Serialize)]
pub struct StatisticsView {
    pub totals: Totals,
    pub process_scope: String,
    pub process_metrics: Snapshot,
    pub history_epoch: u64,
    pub interval_seconds: u64,
    pub retention_seconds: u64,
    pub samples: Vec<Point>,
    pub storage: Status,
}

#[derive(Clone)]
pub(super) struct Checkpoint {
    pub totals: Totals,
    pub run_id: String,
    pub seq: u64,
}
pub(super) struct Owner {
    pub base: Snapshot,
    pub offset: Snapshot,
    pub totals_epoch: u64,
    pub history_epoch: u64,
    pub since_ms: u64,
    pub run_id: String,
    pub seq: u64,
    pub latest: Option<Checkpoint>,
    pub points: VecDeque<Point>,
    pub dropped_points: u64,
    pub baseline: Snapshot,
    pub baseline_at: Instant,
    pub baseline_ms: u64,
    checkpoint_at: Instant,
    running: bool,
    generation: u64,
    next_gap: Option<String>,
}
impl Owner {
    pub fn new(metrics: &Metrics) -> Self {
        let zero = Metrics::default().snapshot();
        let now = Instant::now();
        Self {
            base: zero.clone(),
            offset: zero,
            totals_epoch: 0,
            history_epoch: 0,
            since_ms: now_ms(),
            run_id: format!("{:032x}", rand::random::<u128>()),
            seq: 0,
            latest: None,
            points: VecDeque::new(),
            dropped_points: 0,
            baseline: metrics.snapshot(),
            baseline_at: now,
            baseline_ms: now_ms(),
            checkpoint_at: now,
            running: false,
            generation: 0,
            next_gap: Some("process_start".into()),
        }
    }
    pub fn totals(&self, raw: &Snapshot) -> Totals {
        Totals {
            scope: "persistent".into(),
            epoch: self.totals_epoch,
            since_ms: self.since_ms,
            metrics: self.base.plus(&raw.delta(&self.offset)),
        }
    }
    pub fn checkpoint(&mut self, raw: &Snapshot) {
        self.seq = self.seq.saturating_add(1);
        self.latest = Some(Checkpoint {
            totals: self.totals(raw),
            run_id: self.run_id.clone(),
            seq: self.seq,
        });
        self.checkpoint_at = Instant::now();
    }
    pub fn sample(&mut self, metrics: &Metrics, force: bool) {
        let now = Instant::now();
        if !force
            && now.duration_since(self.checkpoint_at) < Duration::from_secs(5)
            && now.duration_since(self.baseline_at) < Duration::from_secs(60)
        {
            return;
        }
        let raw = metrics.snapshot();
        self.checkpoint(&raw);
        if force || now.duration_since(self.baseline_at) >= Duration::from_secs(60) {
            self.point(&raw, now, now_ms());
        }
    }
    fn point(&mut self, raw: &Snapshot, now: Instant, wall: u64) {
        let elapsed = now.saturating_duration_since(self.baseline_at);
        if elapsed.is_zero() {
            return;
        }
        let expected = self
            .baseline_ms
            .saturating_add(elapsed.as_millis().min(u64::MAX as u128) as u64);
        let gap = if raw.regressed(&self.baseline) {
            Some("counter_overflow".into())
        } else if wall.abs_diff(expected) > 5000 {
            Some("clock_jump".into())
        } else if elapsed > Duration::from_secs(90) {
            Some("sampling_delay".into())
        } else if !self.running {
            Some("dns_unavailable".into())
        } else {
            self.next_gap.take()
        };
        let mut point = Point {
            sampled_at: now,
            history_epoch: self.history_epoch,
            totals_epoch: self.totals_epoch,
            run_id: self.run_id.clone(),
            seq: self.seq,
            start_ms: self.baseline_ms,
            timestamp_ms: wall,
            elapsed_seconds: elapsed.as_secs_f64(),
            running: self.running,
            generation: self.generation,
            gap,
            requests: 0,
            cache_hits: 0,
            cache_misses: 0,
            blocked: 0,
            upstream_failures: 0,
            latency_count: 0,
            latency_sum_micros: 0,
            metrics: raw.delta(&self.baseline),
        };
        point.update_summary();
        if self.points.len() >= 120 {
            self.dropped_points = self.dropped_points.saturating_add(1);
            self.next_gap = Some("queue_overflow".into());
        } else {
            self.points.push_back(point);
        }
        self.baseline = raw.clone();
        self.baseline_at = now;
        self.baseline_ms = wall;
    }
    pub fn set_dns_state(&mut self, metrics: &Metrics, running: bool, generation: u64) {
        if self.running == running && self.generation == generation {
            return;
        }
        self.sample(metrics, true);
        self.running = running;
        self.generation = generation;
        self.next_gap = Some("dns_state_change".into());
    }
    pub fn reset_committed(&mut self, cut: Snapshot, since_ms: u64, epoch: u64) {
        self.base = Metrics::default().snapshot();
        self.offset = cut;
        self.since_ms = since_ms;
        self.totals_epoch = epoch;
        self.latest = None;
        self.next_gap = Some("totals_reset".into());
    }
    pub fn history_cleared(&mut self, cut: Snapshot, since_ms: u64, at: Instant, epoch: u64) {
        self.history_epoch = epoch;
        self.baseline = cut;
        self.baseline_ms = since_ms;
        self.baseline_at = at;
        self.points.clear();
        self.next_gap = Some("history_clear".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Counter;

    #[test]
    fn reset_cut_preserves_concurrent_increments_and_history_cut_is_independent() {
        let metrics = Metrics::default();
        let mut owner = Owner::new(&metrics);
        for _ in 0..10 {
            metrics.inc(Counter::Requests);
        }
        let cut = metrics.snapshot();
        for _ in 0..7 {
            metrics.inc(Counter::Requests);
        }
        owner.reset_committed(cut, now_ms(), 1);
        assert_eq!(
            owner.totals(&metrics.snapshot()).metrics.counters["requests"],
            7
        );
        assert_eq!(metrics.snapshot().counters["requests"], 17);
        owner.history_cleared(metrics.snapshot(), now_ms(), Instant::now(), 1);
        metrics.inc(Counter::Requests);
        owner.sample(&metrics, true);
        assert_eq!(owner.points.back().unwrap().requests, 1);
        assert_eq!(
            owner.totals(&metrics.snapshot()).metrics.counters["requests"],
            8
        );
    }

    #[test]
    fn bounded_minute_points_do_not_follow_latest_checkpoint_replacement() {
        let metrics = Metrics::default();
        let mut owner = Owner::new(&metrics);
        owner.running = true;
        for _ in 0..125 {
            metrics.inc(Counter::Requests);
            owner.sample(&metrics, true);
        }
        assert_eq!(owner.points.len(), 120);
        assert_eq!(owner.dropped_points, 5);
        assert_eq!(
            owner.latest.as_ref().unwrap().totals.metrics.counters["requests"],
            125
        );
        owner.points.clear();
        owner.sample(&metrics, true);
        assert_eq!(owner.points[0].gap.as_deref(), Some("queue_overflow"));
    }

    #[test]
    fn wall_clock_backwards_and_delayed_sampling_are_explicit_gaps() {
        let metrics = Metrics::default();
        let mut owner = Owner::new(&metrics);
        owner.running = true;
        owner.next_gap = None;
        owner.baseline_ms = now_ms() + 30_000;
        owner.sample(&metrics, true);
        assert_eq!(
            owner.points.back().unwrap().gap.as_deref(),
            Some("clock_jump")
        );
        owner.baseline_at = Instant::now() - Duration::from_secs(120);
        owner.baseline_ms = now_ms() - 120_000;
        owner.sample(&metrics, true);
        assert_eq!(
            owner.points.back().unwrap().gap.as_deref(),
            Some("sampling_delay")
        );
    }
}
