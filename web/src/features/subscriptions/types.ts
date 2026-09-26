export interface Failure { code: string; line: number | null }
export interface Operation {
  id: string; kind: 'prepare' | 'refresh'; source_id: string | null;
  fingerprint: string | null; status: 'running' | 'succeeded' | 'failed' | 'cancelled';
  started_at: number; finished_at: number | null; error: Failure | null;
  sha256: string | null; rules: number | null;
}
export interface SourceStatus {
  id: string; fingerprint: string; ready: boolean; active: boolean;
  input_rules: number; bytes: number; last_success: number | null;
  last_attempt: number | null; next_update: number | null; failures: number; error: Failure | null;
}
export interface SubscriptionState {
  config_revision: number; enabled: boolean; sources: SourceStatus[];
  generation: number; content_revision: number; input_rules: number;
  index_rules: number; index_bytes: number; retained_bytes: number; disk_bytes: number;
  operation: Operation | null; recent_operation: Operation | null; unavailable_reason: string | null;
}
export interface CheckResult {
  generation: number; decision: 'allowed' | 'blocked' | 'unmatched';
  witness: null | { source_id: string | null; rule: string; scope: 'exact' | 'suffix' };
}
