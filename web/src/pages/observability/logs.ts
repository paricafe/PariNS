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
  enabled: boolean;
  total: number;
  entries: LogEntry[];
  next_cursor: number | null;
}

export interface LogListResponse { revision: number; page: LogPage }
export interface LogFilter { search: string; status: string | null }

const statusKeys: Record<string, string> = {
  success: "views.success", blocked: "views.blocked", error: "views.error", dropped: "views.dropped",
};
const pathKeys: Record<string, string> = {
  upstream: "views.pathUpstream", fresh: "views.pathFresh", stale: "views.pathStale",
  blocked: "views.pathBlocked", error: "views.pathError", cancelled: "views.pathCancelled",
};

export function statusLabel(status: string): string { return statusKeys[status] ?? status; }
export function pathLabel(path: string): string { return pathKeys[path] ?? path; }
