import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, StaleRequest, type ApiClient } from '../../session/client';
import type { SubscriptionDraft } from '../../model/subscriptions';
import type { Failure, SubscriptionState } from './types';

export function useSubscriptions(api: ApiClient, revision: number | undefined) {
  const [state, setState] = useState<SubscriptionState | null>(null);
  const [error, setError] = useState<Failure | null>(null);
  const [readError, setReadError] = useState<Failure | null>(null);
  const [pending, setPending] = useState(false);
  const [operationId, setOperationId] = useState<string | null>(null);
  const [unknown, setUnknown] = useState(false);
  const owner = useRef(0);
  const sending = useRef(false);
  const latestRead = useRef(0);
  useEffect(() => { owner.current += 1; return () => { owner.current += 1; }; }, [api]);
  const load = useCallback(async () => {
    const token = owner.current;
    const read = ++latestRead.current;
    try {
      const result = await api.request<SubscriptionState>('filter/subscriptions');
      if (token !== owner.current || read !== latestRead.current) return;
      setState(result); setReadError(null);
    } catch (reason) {
      if (token !== owner.current || read !== latestRead.current || reason instanceof StaleRequest) return;
      setReadError({ code: reason instanceof ApiError ? reason.code : 'error', line: null });
    }
  }, [api]);
  useEffect(() => { void load(); const timer = window.setInterval(() => void load(), 3000); return () => window.clearInterval(timer); }, [load, revision]);
  const start = async (kind: 'prepare' | 'refresh', source: SubscriptionDraft | string | null) => {
    if (sending.current || revision === undefined) return;
    sending.current = true; setPending(true); setError(null); setUnknown(false); setOperationId(null);
    const token = owner.current;
    try {
      const body = kind === 'prepare' && source && typeof source !== 'string'
        ? { config_revision: revision, source: { id: source.id, url: source.url, format: source.format } }
        : { config_revision: revision, source_id: source };
      const result = await api.request<{ operation_id: string }>(`filter/subscriptions/${kind}`, 'POST', body);
      if (token !== owner.current) return;
      setOperationId(result.operation_id);
      await load();
    } catch (reason) {
      if (token !== owner.current || reason instanceof StaleRequest) return;
      if (reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code)) { setUnknown(true); await load(); }
      else setError({ code: reason instanceof ApiError ? reason.code : 'error', line: null });
    } finally { if (token === owner.current) { sending.current = false; setPending(false); } }
  };
  const operation = [state?.operation, state?.recent_operation].find((item) => item && item.id === operationId) ?? null;
  return { state, error: error ?? readError, pending, unknown, operation, replaced: operationId !== null && state !== null && !operation, load, start };
}
