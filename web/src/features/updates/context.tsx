import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import { reloadConsole } from './reload';
import { canApply, pollDelay, terminal, type UpdateCandidate, type UpdatesView } from './types';

interface UpdatesContextValue {
  view: UpdatesView | null;
  action: 'check' | 'apply' | null;
  error: string | null;
  readFailed: boolean;
  unknown: boolean;
  pending: boolean;
  disconnected: boolean;
  refresh(): Promise<void>;
  check(): Promise<void>;
  apply(candidate: UpdateCandidate): Promise<void>;
}
const UpdatesContext = createContext<UpdatesContextValue | null>(null);
interface Attempt {
  version: string;
  planId: string | null;
  operationId: string | null;
  previousOperation: string | null;
  missedReads: number;
}
const unknownResult = (reason: unknown) => reason instanceof StaleRequest || reason instanceof ApiError
  && reason.code !== 'update_state_unavailable' && (reason.status >= 500 || ['NETWORK', 'BAD_RESPONSE'].includes(reason.code));
const code = (reason: unknown) => reason instanceof ApiError ? reason.code : 'network';
const restartPhases = new Set(['accepted', 'downloading', 'staged', 'preflight', 'committing', 'stopping',
  'starting', 'validating', 'rolling_back', 'awaiting_boot_recovery', 'awaiting_readiness']);

export function UpdatesProvider({ active, children }: { active: boolean; children: ReactNode }) {
  const { api, expectUpdateRestart } = useSession();
  const config = useConfig();
  const configRef = useRef(config); configRef.current = config;
  const [view, setView] = useState<UpdatesView | null>(null);
  const viewRef = useRef(view); viewRef.current = view;
  const [action, setAction] = useState<'check' | 'apply' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [readFailed, setReadFailed] = useState(false);
  const [unknown, setUnknown] = useState(false);
  const [pending, setPending] = useState(false);
  const [disconnected, setDisconnected] = useState(false);
  const attempt = useRef<Attempt | null>(null);
  const observedRestart = useRef<string | null>(null);
  const unknownRef = useRef(false);
  const reading = useRef(false);
  const acting = useRef(false);
  const owner = useRef(0);
  useEffect(() => {
    if (!active) {
      configRef.current.setUpdateLocked(false);
      attempt.current = null; unknownRef.current = false;
      setPending(false); setUnknown(false); setDisconnected(false);
    }
    return () => { configRef.current.setUpdateLocked(false); };
  }, [active]);

  const refresh = useCallback(async () => {
    if (reading.current) return;
    reading.current = true;
    const epoch = owner.current;
    const settledAttempt = acting.current ? null : attempt.current;
    try {
      const next = await api.request<UpdatesView>('updates');
      if (owner.current !== epoch) return;
      viewRef.current = next; setView(next); setReadFailed(false); setDisconnected(false);
      const own = attempt.current;
      const observed = next.active_operation ?? next.last_operation;
      const activeOperation = next.active_operation;
      const tracked = observedRestart.current;
      const manual = next.candidate?.manual_reason && next.candidate.version === activeOperation?.version;
      if (activeOperation && restartPhases.has(activeOperation.phase)
        && (next.capability.available || next.capability.reason === 'installation_pending')
        && !manual) {
        observedRestart.current = activeOperation.operation_id;
      } else if (tracked && (observed?.operation_id === tracked && terminal(observed.phase)
        || activeOperation && activeOperation.operation_id !== tracked
        || !activeOperation && !next.frozen)) {
        observedRestart.current = null;
      }
      if (own && observed && (own.operationId ? observed.operation_id === own.operationId
        : observed.operation_id !== own.previousOperation && observed.version === own.version)) {
        own.operationId = observed.operation_id; own.missedReads = 0;
        unknownRef.current = false; setUnknown(false);
        if (terminal(observed.phase)) {
          attempt.current = null; setPending(false);
          if (['succeeded', 'rolled_back'].includes(observed.phase) && !configRef.current.dirty) reloadConsole();
        }
      } else if (own && own === settledAttempt) {
        if (next.frozen || next.active_operation) own.missedReads = 0;
        else {
          own.missedReads += 1;
          if (own.missedReads >= 2 || next.candidate?.plan_id !== own.planId || !unknownRef.current) {
            attempt.current = null; setPending(false);
            unknownRef.current = false; setUnknown(false);
            setError('update_not_started');
          }
        }
      }
      if (tracked !== observedRestart.current || own !== attempt.current) {
        expectUpdateRestart(Boolean(observedRestart.current || attempt.current));
      }
      configRef.current.setUpdateLocked(Boolean(next.frozen || next.active_operation || attempt.current));
    } catch (reason) {
      if (owner.current !== epoch || reason instanceof StaleRequest) return;
      if (settledAttempt) settledAttempt.missedReads = 0;
      setReadFailed(true);
      if (attempt.current || viewRef.current?.active_operation) setDisconnected(true);
    } finally { if (owner.current === epoch) reading.current = false; }
  }, [api, expectUpdateRestart]);

  useEffect(() => {
    owner.current += 1;
    reading.current = false;
    if (!active) return;
    let stopped = false;
    let ticking = false;
    let timer: ReturnType<typeof setTimeout>;
    const tick = async () => {
      if (ticking || stopped) return;
      ticking = true;
      await refresh();
      ticking = false;
      if (!stopped) timer = setTimeout(tick, pollDelay(viewRef.current, unknownRef.current || Boolean(attempt.current), document.hidden));
    };
    void tick();
    const visible = () => { if (!document.hidden) { clearTimeout(timer); void tick(); } };
    document.addEventListener('visibilitychange', visible);
    return () => {
      stopped = true; owner.current += 1; clearTimeout(timer);
      document.removeEventListener('visibilitychange', visible);
    };
  }, [active, refresh, pending, unknown, view?.active_operation?.operation_id, view?.check.state]);

  const check = useCallback(async () => {
    if (acting.current || attempt.current) return;
    acting.current = true; setAction('check'); setError(null);
    try { await api.request('updates/check', 'POST', {}); }
    catch (reason) { if (!(reason instanceof StaleRequest)) setError(code(reason)); }
    finally { acting.current = false; setAction(null); await refresh(); }
  }, [api, refresh]);

  const apply = useCallback(async (candidate: UpdateCandidate) => {
    const current = configRef.current;
    if (acting.current || attempt.current || !canApply(viewRef.current)) return;
    if (viewRef.current?.candidate?.plan_id !== candidate.plan_id) { setError('draftChanged'); return; }
    acting.current = true; setAction('apply'); setError(null); setUnknown(false); setPending(true);
    unknownRef.current = false;
    const submitted: Attempt = { version: candidate.version, planId: candidate.plan_id, operationId: null,
      previousOperation: viewRef.current?.last_operation?.operation_id ?? null, missedReads: 0 };
    attempt.current = submitted;
    current.setUpdateLocked(true);
    expectUpdateRestart(true);
    try {
      const accepted = await api.request<{ operation_id: string }>('updates/apply', 'POST', {
        plan_id: candidate.plan_id, expected_version: candidate.version,
      });
      if (attempt.current === submitted) submitted.operationId = accepted.operation_id;
    } catch (reason) {
      if (attempt.current !== submitted || submitted.operationId) return;
      if (unknownResult(reason)) { unknownRef.current = true; setUnknown(true); }
      else {
        attempt.current = null; setPending(false); expectUpdateRestart(Boolean(observedRestart.current));
        current.setUpdateLocked(Boolean(viewRef.current?.frozen || viewRef.current?.active_operation));
        setError(code(reason));
      }
    } finally { acting.current = false; setAction(null); if (!unknownRef.current) await refresh(); }
  }, [api, expectUpdateRestart, refresh]);

  const value = useMemo(() => ({ view, action, error, readFailed, unknown, pending, disconnected, refresh, check, apply }),
    [view, action, error, readFailed, unknown, pending, disconnected, refresh, check, apply]);
  return <UpdatesContext.Provider value={value}>{children}</UpdatesContext.Provider>;
}

export function useUpdates(): UpdatesContextValue {
  const value = useContext(UpdatesContext);
  if (!value) throw new Error('UpdatesProvider is required');
  return value;
}
