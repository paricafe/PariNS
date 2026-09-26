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
  apply(candidate: UpdateCandidate, revision: number): Promise<void>;
}
const UpdatesContext = createContext<UpdatesContextValue | null>(null);
interface Attempt { version: string; operationId: string | null; previousOperation: string | null }
const unknownResult = (reason: unknown) => reason instanceof StaleRequest || reason instanceof ApiError
  && (reason.status >= 500 || ['NETWORK', 'BAD_RESPONSE'].includes(reason.code));
const code = (reason: unknown) => reason instanceof ApiError ? reason.code : 'network';

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
    try {
      const next = await api.request<UpdatesView>('updates');
      if (owner.current !== epoch) return;
      viewRef.current = next; setView(next); setReadFailed(false); setDisconnected(false);
      const own = attempt.current;
      const observed = next.active_operation ?? next.last_operation;
      if (own && observed && (own.operationId ? observed.operation_id === own.operationId
        : observed.operation_id !== own.previousOperation && observed.version === own.version)) {
        own.operationId = observed.operation_id;
        unknownRef.current = false; setUnknown(false);
        if (terminal(observed.phase)) {
          attempt.current = null; setPending(false);
          expectUpdateRestart(false);
          if (['succeeded', 'rolled_back'].includes(observed.phase) && !configRef.current.dirty) reloadConsole();
        }
      }
      configRef.current.setUpdateLocked(Boolean(next.frozen || next.active_operation || attempt.current));
    } catch (reason) {
      if (owner.current !== epoch || reason instanceof StaleRequest) return;
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

  const apply = useCallback(async (candidate: UpdateCandidate, revision: number) => {
    const current = configRef.current;
    if (acting.current || attempt.current || !canApply(viewRef.current)) return;
    if (current.dirty || current.locked || current.busy || current.draft?.revision !== revision
      || viewRef.current?.candidate?.plan_id !== candidate.plan_id) { setError('draftChanged'); return; }
    acting.current = true; setAction('apply'); setError(null); setUnknown(false); setPending(true);
    unknownRef.current = false;
    attempt.current = { version: candidate.version, operationId: null, previousOperation: viewRef.current?.last_operation?.operation_id ?? null };
    current.setUpdateLocked(true);
    expectUpdateRestart(true);
    try {
      const accepted = await api.request<{ operation_id: string }>('updates/apply', 'POST', {
        plan_id: candidate.plan_id, expected_version: candidate.version, config_revision: revision,
      });
      if (attempt.current) attempt.current.operationId = accepted.operation_id;
    } catch (reason) {
      if (unknownResult(reason)) { unknownRef.current = true; setUnknown(true); }
      else {
        attempt.current = null; setPending(false); expectUpdateRestart(false);
        current.setUpdateLocked(Boolean(viewRef.current?.frozen || viewRef.current?.active_operation));
        setError(code(reason));
      }
    } finally { acting.current = false; setAction(null); await refresh(); }
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
