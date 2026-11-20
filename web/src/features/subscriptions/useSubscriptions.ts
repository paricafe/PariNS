import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, StaleRequest, type ApiClient } from '../../session/client';
import { pollDelay, type SubscriptionState } from './types';

export function useSubscriptions(api: ApiClient, savedKey: unknown) {
  const [state, setState] = useState<SubscriptionState | null>(null);
  const [error, setError] = useState<ApiError | string | null>(null);
  const [readError, setReadError] = useState<ApiError | string | null>(null);
  const [pending, setPending] = useState(false);
  const [operationId, setOperationId] = useState<string | null>(null);
  const [unknown, setUnknown] = useState(false);
  const stateRef = useRef(state);
  const recovery = useRef<{ beforeRecentId: string | null; sourceId: string | null } | null>(null);
  const owner = useRef(0);
  const sending = useRef(false);
  const latestRead = useRef(0);
  useEffect(() => { owner.current += 1; return () => { owner.current += 1; }; }, [api]);
  const load = useCallback(async (manual = false) => {
    const token = owner.current;
    const read = ++latestRead.current;
    try {
      const result = await api.request<SubscriptionState>('filter/subscriptions');
      if (token !== owner.current || read !== latestRead.current) return;
      stateRef.current = result;
      setState(result); setReadError(null);
      const attempt = recovery.current;
      if (attempt) {
        const observed = [result.operation, result.recent_operation].find((item) => item
          && item.id !== attempt.beforeRecentId && item.kind === 'refresh'
          && (attempt.sourceId === null || item.source_id === attempt.sourceId));
        if (observed || manual && !result.operation) {
          recovery.current = null; setUnknown(false); setOperationId(observed?.id ?? null);
        }
      }
    } catch (reason) {
      if (token !== owner.current || read !== latestRead.current || reason instanceof StaleRequest) return;
      setReadError(reason instanceof ApiError ? reason : 'subscriptions.error');
    }
  }, [api]);
  const polling = useRef({ state, pending, unknown });
  polling.current = { state, pending, unknown };
  const reschedule = useRef(() => {});
  useEffect(() => {
    let stopped = false;
    let timer: number | undefined;
    const schedule = () => {
      window.clearTimeout(timer);
      if (stopped) return;
      const { state: current, pending: sending, unknown: unresolved } = polling.current;
      const delay = pollDelay(current, sending, unresolved, document.visibilityState === 'hidden');
      if (delay !== null) timer = window.setTimeout(tick, delay);
    };
    const tick = async () => { await load(); schedule(); };
    const visibility = () => { if (document.visibilityState === 'hidden') window.clearTimeout(timer); else void tick(); };
    reschedule.current = schedule;
    if (document.visibilityState !== 'hidden') void tick();
    document.addEventListener('visibilitychange', visibility);
    return () => {
      stopped = true; window.clearTimeout(timer); reschedule.current = () => {};
      document.removeEventListener('visibilitychange', visibility);
    };
  }, [load, savedKey]);
  const visibleDelay = pollDelay(state, pending, unknown, false);
  useEffect(() => { reschedule.current(); }, [visibleDelay]);
  const refresh = async (sourceId: string | null) => {
    if (sending.current || recovery.current) return;
    sending.current = true; setPending(true); setError(null); setUnknown(false); setOperationId(null);
    const token = owner.current;
    const beforeRecentId = stateRef.current?.recent_operation?.id ?? null;
    try {
      const body = { source_id: sourceId };
      const result = await api.request<{ operation_id: string }>('filter/subscriptions/refresh', 'POST', body);
      if (token !== owner.current) return;
      setOperationId(result.operation_id);
      await load();
    } catch (reason) {
      if (token !== owner.current || reason instanceof StaleRequest) return;
      if (reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code)) {
        recovery.current = { beforeRecentId, sourceId };
        setUnknown(true); await load();
      }
      else setError(reason instanceof ApiError ? reason : 'subscriptions.error');
    } finally { if (token === owner.current) { sending.current = false; setPending(false); } }
  };
  const operation = [state?.operation, state?.recent_operation].find((item) => item && item.id === operationId) ?? null;
  return { state, error: error ?? readError, pending, unknown, operation, load, refresh };
}
