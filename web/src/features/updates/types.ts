export interface UpdateOperation {
  operation_id: string;
  phase: string;
  version: string;
  reason: string | null;
  downloaded_bytes: number;
  total_bytes: number | null;
}

export interface UpdateCandidate {
  version: string;
  tag: string;
  published_at: string | null;
  notes: string;
  manual_reason: string | null;
  plan_id: string | null;
  expires_at_ms: number | null;
}

export interface UpdatesView {
  current: { version: string; target: string; official_release: boolean };
  capability: { available: boolean; reason: string | null };
  check: {
    state: 'unchecked' | 'checking' | 'failed' | 'available' | 'up_to_date';
    last_check_at_ms: number | null;
    last_success_at_ms: number | null;
    next_check_at_ms: number;
    retry_at_ms: number;
    error: string | null;
  };
  candidate: UpdateCandidate | null;
  active_operation: UpdateOperation | null;
  last_operation: UpdateOperation | null;
  frozen: boolean;
}

export function terminal(phase: string): boolean {
  return ['succeeded', 'rolled_back', 'failed', 'aborted', 'manual_required'].includes(phase);
}

export function pollDelay(view: UpdatesView | null, unknown: boolean, hidden: boolean): number {
  if (hidden) return 60_000;
  return unknown || view?.active_operation || view?.check.state === 'checking' ? 2_000 : 30_000;
}

export function canApply(view: UpdatesView | null, now = Date.now()): boolean {
  return Boolean(view?.capability.available && view.candidate?.plan_id && !view.candidate.manual_reason
    && (view.candidate.expires_at_ms ?? 0) > now && !view.active_operation && !view.frozen);
}
