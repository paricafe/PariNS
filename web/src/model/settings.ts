/** UI field descriptions only; Rust owns configuration parsing and validation. */
export type FieldType = "text" | "lines" | "select" | "checkbox" | "nullable" | "number" | "endpoint";

export interface SettingField {
  path: string;
  type: FieldType;
  labelKey: string;
  helpKey?: string;
  min?: number;
  max?: number;
  options?: readonly (readonly [string, string])[];
}

export interface SettingGroup {
  id: string;
  titleKey: string;
  helpKey: string;
  enableKey: string;
  fields: readonly SettingField[];
  optional?: "dot" | "doh" | "doq";
}

export interface SettingPage {
  id: string;
  titleKey: string;
  introKey: string;
  groups: readonly SettingGroup[];
}

const field = (path: string, type: FieldType = "text", help = false, min?: number, max?: number, labelPath = path): SettingField => ({
  path, type, labelKey: `settings.${labelPath}.label`, ...(help ? { helpKey: `settings.${path}.help` } : {}),
  ...(min === undefined ? {} : { min }), ...(max === undefined ? {} : { max }),
});
const number = (path: string, min: number, max: number, help = false): SettingField => field(path, "number", help, min, max);
const select = (path: string, values: readonly string[]): SettingField => ({
  ...field(path, "select"), options: values.map((value) => [value, `settings.${path}.${value}`] as const),
});
const group = (id: string, fields: readonly SettingField[], optional?: SettingGroup["optional"]): SettingGroup => ({
  id, titleKey: `settings.group.${id}.title`, helpKey: `settings.group.${id}.help`,
  enableKey: `settings.group.${id}.enable`, fields, ...(optional ? { optional } : {}),
});
const page = (id: string, groups: readonly SettingGroup[]): SettingPage => ({
  id, titleKey: `settings.page.${id}.title`, introKey: `settings.page.${id}.intro`, groups,
});

export const settingPages = {
  dns: page("dns", [
    group("upstreams", [
      field("upstreams.servers", "lines", true), select("upstreams.mode", ["weighted", "parallel"]),
      field("upstreams.prefer_h3", "checkbox", true), field("upstreams.bootstrap", "lines", true),
      number("upstreams.max_parallel", 1, 32, true), number("upstreams.max_extra_inflight", 1, 65536, true),
      field("upstreams.ca_file", "nullable"), field("upstreams.dot_pool.enabled", "checkbox", true),
      number("upstreams.dot_pool.max_connections", 1, 256, true),
      number("upstreams.dot_pool.idle_timeout_ms", 1, 600000),
    ]),
    group("listeners", [field("listen", "text", true), number("query_timeout_ms", 1, 60000), number("tcp_io_timeout_ms", 1, 60000)]),
  ]),
  cache: page("cache", [
    group("persistence", [field("cache.persistence.enabled", "checkbox", true), number("cache.persistence.max_bytes", 1048576, 536870912)]),
    group("cache", [
      field("cache.enabled", "checkbox"), number("cache.max_entries", 1, 262144),
      number("cache.max_bytes", 512, 1073741824, true), number("cache.max_variants", 1, 256),
      number("cache.shards", 1, 64, true), number("cache.negative_percent", 0, 90, true),
      number("cache.max_ttl_secs", 1, 86400), number("cache.negative_ttl_cap_secs", 1, 86400),
    ]),
    group("prefetch", [
      field("cache.prefetch.enabled", "checkbox"), number("cache.prefetch.min_hits", 1, 1000000),
      number("cache.prefetch.remaining_percent", 1, 90), number("cache.prefetch.max_inflight", 1, 256),
      number("cache.prefetch.rate_per_sec", 1, 10000), number("cache.prefetch.backoff_secs", 1, 3600),
    ]),
    group("stale", [
      field("cache.stale.enabled", "checkbox"), number("cache.stale.retention_secs", 1, 604800),
      number("cache.stale.reply_ttl_secs", 1, 300),
    ]),
    group("ecs", [field("ecs.enabled", "checkbox"), number("ecs.ipv4_prefix", 0, 32, true), number("ecs.ipv6_prefix", 0, 128)]),
  ]),
  filters: page("filters", [
    group("filterSource", [field("filter_file", "nullable", true)]),
    group("filterRules", [
      field("filter.enabled", "checkbox"), field("filter.block_exact", "lines", true),
      field("filter.block_suffix", "lines", true), field("filter.allow_exact", "lines"),
      field("filter.allow_suffix", "lines"),
    ]),
  ]),
  security: page("security", [
    group("web", [field("web.public_host", "text", true)]),
    ...(["dot", "doh", "doq"] as const).map((protocol) => group(
      protocol,
      [...(["listen", "cert_file", "key_file"] as const).map((name) => field(
        `${protocol}.${name}`, name === "listen" ? "endpoint" : "text", false, undefined, undefined, `listener.${name}`,
      )), ...(protocol === "doh" ? [field("doh.http3", "checkbox", true)] : [])],
      protocol,
    )),
  ]),
  storage: page("storage", [
    group("queryLog", [field("query_log.enabled", "checkbox"), number("query_log.max_entries", 1, 1000000), number("query_log.max_bytes", 1048576, 536870912), number("query_log.retention_secs", 60, 2592000)]),
    group("statistics", [number("statistics.retention_secs", 3600, 31536000), number("statistics.max_samples", 60, 525600), number("statistics.reset_interval_days", 0, 3650, true)]),
    group("storageAdvanced", [number("storage.max_database_bytes", 16777216, 4294967296, true), number("storage.flush_interval_ms", 100, 5000), number("storage.cleanup_interval_secs", 10, 3600), number("storage.queue_max_entries", 128, 65536), number("storage.queue_max_bytes", 1048576, 67108864)]),
  ]),
  runtime: page("runtime", [
    group("concurrency", [number("max_inflight", 1, 65536), number("max_tcp_connections", 1, 65536), number("shutdown_grace_ms", 1, 60000)]),
    group("sourceLimits", [
      field("source_limits.enabled", "checkbox"), number("source_limits.rate_per_sec", 1, 1000000),
      number("source_limits.burst", 1, 1000000), number("source_limits.max_sources", 1, 65536),
      number("source_limits.ipv4_prefix", 0, 32), number("source_limits.ipv6_prefix", 0, 128),
      number("source_limits.max_inflight", 1, 65536), number("source_limits.max_connections", 1, 65536),
    ]),
    group("coalescing", [field("coalescing.enabled", "checkbox"), number("coalescing.max_groups", 1, 65536), number("coalescing.max_waiters", 1, 65536)]),
    group("metrics", [number("metrics.interval_secs", 0, 3600, true), field("admin_listen", "nullable", true)]),
  ]),
} as const;

export type SettingPageId = keyof typeof settingPages;
