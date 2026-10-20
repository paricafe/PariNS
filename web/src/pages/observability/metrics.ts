export interface HistogramBucket {
  upper_bound_micros: number | null;
  count: number;
}

export interface RequestLatency {
  buckets: HistogramBucket[];
  count: number;
  sum_micros: number;
}

import type { SnapshotReport, StorageStatus } from './storage';
import type { CacheDiagnostics, DnsHealth, QuicDiagnostics, RecursionDiagnostics, UpstreamDiagnostics } from './diagnostics';
export interface MetricsSnapshot {
  counters: Record<string, number>;
  request_inflight: number;
  request_latency: RequestLatency;
}
export interface ListenerStatus {
  kind: 'dns' | 'admin' | 'dot' | 'doh' | 'doh3' | 'doq';
  address: string;
  state: 'running' | 'retrying';
  error: string | null;
}

export interface StatusView {
  running: boolean;
  last_error: string | null;
  listen: string | null;
  uptime_seconds: number | null;
  generation: number;
  metrics: MetricsSnapshot | null;
  storage?: StorageStatus;
  cache_persistence?: SnapshotReport;
  dns_health: DnsHealth;
  /** Resolution mode of the running generation; null while DNS is not running. */
  resolution_mode: 'forward' | 'recursive' | null;
  /** Per-listener state of the running generation; absent while DNS is stopped. */
  listeners?: ListenerStatus[] | null;
  /** `upstreams` is null in recursive mode; `recursion` is null in forward mode. Both are null while DNS is not running. */
  diagnostics: { cache: CacheDiagnostics | null; upstreams: UpstreamDiagnostics | null; recursion: RecursionDiagnostics | null; quic: QuicDiagnostics };
}

export interface HistorySample {
  gap?: string | null;
  history_epoch?: number;
  totals_epoch?: number;
  run_id?: string;
  timestamp_ms: number;
  elapsed_seconds: number;
  running: boolean;
  generation: number;
  requests: number;
  cache_hits: number;
  blocked: number;
}

export interface StatsView {
  totals: { scope: string; epoch: number; since_ms: number; metrics: MetricsSnapshot };
  process_scope: string;
  process_metrics: MetricsSnapshot;
  history_epoch: number;
  storage: StorageStatus;
  interval_seconds: number;
  retention_seconds: number;
  samples: HistorySample[];
}

export type TrendKey = "requests" | "cache_hits" | "blocked";
export interface TrendPoint { time: number; value: number | null; breakBefore: boolean }

export function trendSeries(samples: readonly HistorySample[], key: TrendKey, hours: number, now: number): TrendPoint[] {
  const visible = samples.filter((sample) => sample.timestamp_ms >= now - hours * 3_600_000 && sample.timestamp_ms <= now);
  return visible.map((sample, index) => {
    const previous = visible[index - 1];
    const value = sample.running && !sample.gap && sample.elapsed_seconds > 0 ? sample[key] / sample.elapsed_seconds : null;
    return {
      time: sample.timestamp_ms,
      value: value !== null && Number.isFinite(value) ? value : null,
      breakBefore: !previous || !previous.running || Boolean(previous.gap) || Boolean(sample.gap) || sample.generation !== previous.generation
        || sample.run_id !== previous.run_id || sample.totals_epoch !== previous.totals_epoch || sample.history_epoch !== previous.history_epoch
        || sample.timestamp_ms - previous.timestamp_ms > Math.max(90_000, sample.elapsed_seconds * 1000 + 5000),
    };
  });
}

export function isolatedPoints(points: readonly TrendPoint[]): TrendPoint[] {
  return points.filter((point, index) => {
    if (point.value === null) return false;
    const previous = points[index - 1];
    const next = points[index + 1];
    const before = previous && previous.value !== null && !point.breakBefore;
    const after = next && next.value !== null && !next.breakBefore;
    return !before && !after;
  });
}

export function histogramRows(buckets: readonly HistogramBucket[]): { lowerMillis: number; upperMillis: number | null; count: number }[] {
  let previous = 0;
  let lowerMillis = 0;
  return buckets.map((bucket) => {
    const upperMillis = bucket.upper_bound_micros === null ? null : bucket.upper_bound_micros / 1000;
    const row = { lowerMillis, upperMillis, count: Math.max(0, bucket.count - previous) };
    previous = bucket.count;
    if (upperMillis !== null) lowerMillis = upperMillis;
    return row;
  });
}

/**
 * Cache lookups by how they were answered. `misses` are the lookups that needed
 * their own upstream request; the others were answered from the cache, from a
 * stale entry, or by joining an identical query already in flight.
 */
export interface CacheBreakdown { lookups: number; hits: number; stale: number; joined: number; misses: number }

export function cacheBreakdown(counters: Record<string, number> | null | undefined): CacheBreakdown | null {
  if (!counters) return null;
  const hits = counters.cache_hits ?? 0;
  const lookups = hits + (counters.cache_misses ?? 0);
  if (lookups <= 0) return null;
  const stale = counters.cache_lookup_stale ?? 0;
  const joined = counters.flight_joined ?? 0;
  // A joined query whose shared exchange failed can also be answered stale; never count past the lookups.
  return { lookups, hits, stale, joined, misses: Math.max(0, lookups - hits - stale - joined) };
}

/** Hit rate: share of cache lookups that needed no separate upstream request. */
export function cacheHitRate(counters: Record<string, number> | null | undefined): number | null {
  const breakdown = cacheBreakdown(counters);
  return breakdown ? (breakdown.lookups - breakdown.misses) / breakdown.lookups * 100 : null;
}

export function averageLatencyMillis(latency: RequestLatency | null | undefined): number | null {
  return latency?.count ? latency.sum_micros / latency.count / 1000 : null;
}
