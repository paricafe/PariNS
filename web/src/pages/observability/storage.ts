import { ApiError } from '../../session/client';

export function isUnknownStorageMutation(error: unknown): boolean {
  return error instanceof ApiError && (error.status === 0 || error.code === 'STORAGE_UNAVAILABLE' ||
    (error.code === 'BAD_RESPONSE' && error.status >= 200 && error.status < 300));
}

export interface StorageStatus {
  health: string;
  error: string | null;
  configured_revision: number;
  applied_revision: number;
  capacity_pending: boolean;
  database_bytes: number;
  journal_bytes: number;
  cache_snapshot_bytes: number;
  query_log_entries: number;
  query_log_bytes: number;
  history_samples: number;
  pending_entries: number;
  pending_bytes: number;
  pending_points: number;
  dropped_logs: number;
  dropped_points: number;
  last_commit_at_ms: number | null;
  persist_lag_ms: number;
  log_epoch: number;
  totals_epoch: number;
  history_epoch: number;
  clock_rollback: boolean;
  query_log_coverage: { earliest_time_ms: number | null; latest_time_ms: number | null; span_ms: number | null };
  cleanup: { last_reason: string | null; last_at_ms: number | null; removed: { age: number; entry_limit: number; byte_limit: number; database_limit: number; manual: number } };
  dropped_by_reason: { queue_full: number; io_failure: number; entry_too_large: number; stopped: number };
  filesystem: { sampled_at_ms: number | null; total_bytes: number | null; available_bytes: number | null; error: string | null; stale: boolean; low_space: boolean | null; warning_threshold_bytes: number };
}

export interface SnapshotReport {
  saved: number;
  restored: number;
  skipped: number;
  bytes: number;
  reason: string | null;
}

export type StorageAction = 'logs' | 'totals' | 'history';
export const storageActions = {
  logs: { path: 'query-log/clear', epoch: 'log_epoch', label: 'clearLogs', confirm: 'confirmLogs' },
  totals: { path: 'stats/reset', epoch: 'totals_epoch', label: 'resetTotals', confirm: 'confirmTotals' },
  history: { path: 'stats/history/clear', epoch: 'history_epoch', label: 'clearHistory', confirm: 'confirmHistory' },
} as const;
