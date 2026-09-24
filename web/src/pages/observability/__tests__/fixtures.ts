import type { StorageStatus } from '../storage';
export const storage: StorageStatus = {
  health: 'healthy', error: null, configured_revision: 7, applied_revision: 7, capacity_pending: false,
  database_bytes: 32768, journal_bytes: 0, cache_snapshot_bytes: 0, query_log_entries: 1, query_log_bytes: 512,
  history_samples: 1, pending_entries: 0, pending_bytes: 0, pending_points: 0, dropped_logs: 0, dropped_points: 0,
  last_commit_at_ms: Date.now(), persist_lag_ms: 0, log_epoch: 3, totals_epoch: 4, history_epoch: 5, clock_rollback: false,
  query_log_coverage: { earliest_time_ms: null, latest_time_ms: null, span_ms: null },
  cleanup: { last_reason: null, last_at_ms: null, removed: { age: 0, entry_limit: 0, byte_limit: 0, database_limit: 0, manual: 0 } },
  dropped_by_reason: { queue_full: 0, io_failure: 0, entry_too_large: 0, stopped: 0 },
  filesystem: { sampled_at_ms: null, total_bytes: null, available_bytes: null, error: null, stale: false, low_space: null, warning_threshold_bytes: 67108864 },
};
