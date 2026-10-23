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
  /** Presentation only: listed under the group's “More options” row. */
  advanced?: boolean;
  /** Presentation only: help that affects security or availability stays visible instead of behind ⓘ. */
  visibleHelp?: boolean;
  /** Shown while the draft value at `path` equals `value`; otherwise the field is hidden and Save removes its value. */
  when?: { path: string; value: string };
}

export interface SettingGroup {
  id: string;
  titleKey: string;
  /** Footer text; omitted when the title and fields say enough. */
  helpKey?: string;
  enableKey: string;
  fields: readonly SettingField[];
  optional?: "dot" | "doh" | "doq";
  /** Presentation only: shown while the draft value at `path` equals `value`. */
  when?: { path: string; value: string };
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
const advanced = (item: SettingField): SettingField => ({ ...item, advanced: true });
const visible = (item: SettingField): SettingField => ({ ...item, visibleHelp: true });
const group = (id: string, fields: readonly SettingField[], optional?: SettingGroup["optional"]): SettingGroup => ({
  id, titleKey: `settings.group.${id}.title`, helpKey: `settings.group.${id}.help`,
  enableKey: `settings.group.${id}.enable`, fields, ...(optional ? { optional } : {}),
});
const shownWhen = (item: SettingField, path: string, value: string): SettingField => ({ ...item, when: { path, value } });
const when = (item: SettingGroup, path: string, value: string): SettingGroup => ({ ...item, when: { path, value } });
const quiet = ({ helpKey: _, ...rest }: SettingGroup): SettingGroup => rest;
const page = (id: string, groups: readonly SettingGroup[]): SettingPage => ({
  id, titleKey: `settings.page.${id}.title`, introKey: `settings.page.${id}.intro`, groups,
});

/** Answers to a blocked query: `[filter].block_response`, and the optional mode of a subscription source. */
export const BLOCK_RESPONSES = ["nodata", "null_ip", "custom_ip", "nxdomain", "refused"] as const;
export type BlockResponse = typeof BLOCK_RESPONSES[number];

export const settingPages = {
  dns: page("dns", [
    group("resolution", [{ ...select("resolution.mode", ["forward", "recursive"]), helpKey: "settings.resolution.mode.help" }]),
    when(group("recursion", [
      field("recursion.ipv6", "checkbox", true), number("recursion.max_outbound_per_second", 1, 10000, true),
      advanced(number("recursion.max_outbound_inflight", 1, 4096)), advanced(number("recursion.max_queries_per_request", 1, 1000, true)),
      advanced(number("recursion.max_depth", 4, 64, true)), advanced(number("recursion.cache_entries", 1024, 1048576, true)),
    ]), "resolution.mode", "recursive"),
    group("upstreams", [
      field("upstreams.servers", "lines", true), { ...select("upstreams.mode", ["weighted", "parallel"]), helpKey: "settings.upstreams.mode.help" },
      field("upstreams.prefer_h3", "checkbox", true), field("upstreams.bootstrap", "lines", true),
      number("upstreams.max_parallel", 1, 32, true), advanced(number("upstreams.max_extra_inflight", 1, 65536, true)),
      advanced(field("upstreams.ca_file", "nullable", true)), advanced(field("upstreams.dot_pool.enabled", "checkbox", true)),
      advanced(number("upstreams.dot_pool.max_connections", 1, 256, true)),
      advanced(number("upstreams.dot_pool.idle_timeout_ms", 1, 600000)),
    ]),
    quiet(group("listeners", [visible(field("listen", "text", true)), number("query_timeout_ms", 1, 60000), advanced(number("tcp_io_timeout_ms", 1, 60000))])),
  ]),
  cache: page("cache", [
    group("persistence", [field("cache.persistence.enabled", "checkbox"), number("cache.persistence.max_bytes", 1048576, 68719476736)]),
    quiet(group("cache", [
      field("cache.enabled", "checkbox"), number("cache.max_entries", 1, 16777216),
      number("cache.max_bytes", 512, 68719476736, true), advanced(number("cache.max_variants", 1, 4096, true)),
      advanced(number("cache.shards", 1, 256, true)), advanced(number("cache.negative_percent", 0, 90, true)),
      number("cache.max_ttl_secs", 1, 604800), number("cache.negative_ttl_cap_secs", 1, 604800),
    ])),
    group("prefetch", [
      field("cache.prefetch.enabled", "checkbox"), number("cache.prefetch.min_hits", 1, 1000000),
      number("cache.prefetch.remaining_percent", 1, 90), advanced(number("cache.prefetch.max_inflight", 1, 4096)),
      advanced(number("cache.prefetch.rate_per_sec", 1, 100000)), advanced(number("cache.prefetch.backoff_secs", 1, 86400)),
    ]),
    group("stale", [
      field("cache.stale.enabled", "checkbox"), number("cache.stale.retention_secs", 1, 2592000),
      number("cache.stale.reply_ttl_secs", 1, 3600),
    ]),
    group("ecs", [field("ecs.enabled", "checkbox"), number("ecs.ipv4_prefix", 0, 32, true), number("ecs.ipv6_prefix", 0, 128)]),
    group("ecsSharing", [
      field("cache.ecs_sharing.enabled", "checkbox"), number("cache.ecs_sharing.min_networks", 2, 64, true),
      advanced(number("cache.ecs_sharing.revoke_cooldown_secs", 60, 2592000, true)),
    ]),
  ]),
  filters: page("filters", [
    group("filterSource", [field("filter_file", "nullable", true)]),
    group("filterRules", [
      field("filter.enabled", "checkbox"), field("filter.block_exact", "lines"),
      field("filter.block_suffix", "lines"), field("filter.allow_exact", "lines"),
      field("filter.allow_suffix", "lines"),
    ]),
    group("blockResponse", [
      select("filter.block_response", BLOCK_RESPONSES),
      shownWhen(field("filter.block_ipv4", "nullable", true), "filter.block_response", "custom_ip"),
      shownWhen(field("filter.block_ipv6", "nullable", true), "filter.block_response", "custom_ip"),
      number("filter.blocked_ttl_secs", 1, 86400, true),
    ]),
    group("subscriptionLimits", [field("filter_subscriptions.enabled", "checkbox", true), number("filter_subscriptions.max_rules", 1, 5000000), number("filter_subscriptions.max_memory_bytes", 1, 536870912), number("filter_subscriptions.max_disk_bytes", 262144, 2147483648)]),
  ]),
  security: page("security", [
    group("web", [visible(field("web.public_host", "text", true))]),
    ...(["dot", "doh", "doq"] as const).map((protocol) => group(
      protocol,
      [...(["listen", "cert_file", "key_file"] as const).map((name) => field(
        `${protocol}.${name}`, name === "listen" ? "endpoint" : "text", false, undefined, undefined, `listener.${name}`,
      )), ...(protocol === "doh" ? [visible(field("doh.http3", "checkbox", true))] : [])],
      protocol,
    )),
  ]),
  storage: page("storage", [
    group("queryLog", [visible(field("query_log.enabled", "checkbox", true)), number("query_log.max_entries", 1, 1000000), number("query_log.max_bytes", 1048576, 536870912), number("query_log.retention_secs", 60, 2592000)]),
    group("statistics", [number("statistics.retention_secs", 3600, 31536000), number("statistics.max_samples", 60, 525600), number("statistics.reset_interval_days", 0, 3650, true)]),
    group("storageAdvanced", [number("storage.max_database_bytes", 16777216, 4294967296, true), advanced(number("storage.flush_interval_ms", 100, 5000)), advanced(number("storage.cleanup_interval_secs", 10, 3600)), advanced(number("storage.queue_max_entries", 128, 65536)), advanced(number("storage.queue_max_bytes", 1048576, 67108864))]),
  ]),
  runtime: page("runtime", [
    group("concurrency", [number("max_inflight", 1, 65536), number("max_tcp_connections", 1, 65536), number("shutdown_grace_ms", 1, 60000)]),
    group("sourceLimits", [
      field("source_limits.enabled", "checkbox"), number("source_limits.rate_per_sec", 1, 1000000),
      number("source_limits.burst", 1, 1000000), advanced(number("source_limits.max_sources", 1, 65536)),
      advanced(number("source_limits.ipv4_prefix", 0, 32)), advanced(number("source_limits.ipv6_prefix", 0, 128)),
      number("source_limits.max_inflight", 1, 65536), number("source_limits.max_connections", 1, 65536),
    ]),
    group("coalescing", [field("coalescing.enabled", "checkbox"), advanced(number("coalescing.max_groups", 1, 65536)), advanced(number("coalescing.max_waiters", 1, 65536))]),
    group("metrics", [number("metrics.interval_secs", 0, 3600, true), visible(field("admin_listen", "nullable", true))]),
  ]),
  updates: page("updates", [
    group("updates", [field("updates.auto_check", "checkbox"), number("updates.check_interval_hours", 1, 168)]),
  ]),
} as const;

export type SettingPageId = keyof typeof settingPages;
