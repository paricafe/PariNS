export interface HistogramBucket {
  upper_bound_micros: number | null;
  count: number;
}

export interface RequestLatency {
  buckets: HistogramBucket[];
  count: number;
  sum_micros: number;
}

export interface StatusView {
  running: boolean;
  revision: number;
  last_error: string | null;
  listen: string | null;
  uptime_seconds: number | null;
  generation: number;
  metrics: {
    counters: Record<string, number>;
    request_inflight: number;
    request_latency: RequestLatency;
  } | null;
}

export interface HistorySample {
  timestamp_ms: number;
  elapsed_seconds: number;
  running: boolean;
  generation: number;
  requests: number;
  cache_hits: number;
  blocked: number;
}

export interface StatsView {
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
    const value = sample.running && sample.elapsed_seconds > 0 ? sample[key] / sample.elapsed_seconds : null;
    return {
      time: sample.timestamp_ms,
      value: value !== null && Number.isFinite(value) ? value : null,
      breakBefore: !previous || !previous.running || sample.generation !== previous.generation
        || sample.timestamp_ms - previous.timestamp_ms > 90_000,
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

export function cacheHitRate(counters: Record<string, number> | null | undefined): number | null {
  if (!counters) return null;
  const hits = counters.cache_hits ?? 0;
  const lookups = hits + (counters.cache_misses ?? 0);
  return lookups > 0 ? hits / lookups * 100 : null;
}

export function averageLatencyMillis(latency: RequestLatency | null | undefined): number | null {
  return latency?.count ? latency.sum_micros / latency.count / 1000 : null;
}
