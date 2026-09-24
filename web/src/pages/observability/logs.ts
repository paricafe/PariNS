import type { StorageStatus } from './storage';
import type { CacheDecision, UpstreamAttempt } from './diagnostics';
export interface LogAnswer {
  name: string;
  record_type: string;
  ttl: number;
  data: string;
}

export interface LogEntry {
  id: number;
  time_ms: number;
  client: string;
  name: string;
  qtype: string;
  transport: string;
  status: string;
  rcode: string | null;
  duration_ms: number;
  cache: string;
  cache_lookup: CacheDecision | null;
  cache_store: CacheDecision | null;
  cache_scope: string | null;
  upstream_relation: 'leader' | 'follower' | 'bypass' | 'prefetch_follower' | null;
  upstream_attempts: UpstreamAttempt[] | null;
  upstream_attempts_omitted: number | null;
  failure_stage: string | null;
  failure_reason: string | null;
  upstream: string | null;
  incoming_ecs: string | null;
  outgoing_ecs: string | null;
  edns: boolean;
  dnssec_ok: boolean;
  checking_disabled: boolean;
  recursion_desired: boolean;
  answer: LogAnswer[];
  answer_truncated: boolean;
}

export interface LogPage {
  log_epoch: number;
  storage: StorageStatus;
  enabled: boolean;
  total: number;
  entries: LogEntry[];
  next_cursor: number | null;
}

export interface LogListResponse { revision: number; page: LogPage }
export type CacheFilter = 'cached' | 'fresh' | 'stale' | 'non_cached' | null;
export interface LogFilter { search: string; status: string | null; cache: CacheFilter }

const statusKeys: Record<string, string> = {
  success: "views.success", blocked: "views.blocked", error: "views.error", dropped: "views.dropped",
};
const pathKeys: Record<string, string> = {
  upstream: "views.pathUpstream", fresh: "views.pathFresh", stale: "views.pathStale",
  blocked: "views.pathBlocked", error: "views.pathError", cancelled: "views.pathCancelled",
};

export function statusLabel(status: string): string { return statusKeys[status] ?? status; }
export function pathLabel(path: string): string { return pathKeys[path] ?? path; }
