import { useCallback, useEffect, useRef, useState } from 'react';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import type { Language } from '../../i18n';
import { DohRuntime, type DohStatus } from './DohRuntime';
import { CertificateTools, type CertificateView } from './CertificateTools';

export type SecurityStatus = DohStatus & CertificateView;

/** Owns the security page's single /status read and its one 5-second poll, shared by the DoH and certificate panels. */
export function SecurityRuntime({ language }: { language: Language }) {
  const { api } = useSession();
  const revision = useConfig().draft?.revision;
  const [status, setStatus] = useState<SecurityStatus | null>(null);
  const [failure, setFailure] = useState<ApiError | string | null>(null);
  const mounted = useRef(false);
  const request = useRef(0);
  const held = useRef(false);
  const read = useCallback(async () => {
    const owner = ++request.current;
    const current = await api.request<SecurityStatus>('status');
    if (!mounted.current || request.current !== owner) throw new StaleRequest();
    setStatus(current); setFailure(null);
    return current;
  }, [api]);
  useEffect(() => {
    mounted.current = true;
    const tick = () => {
      if (document.visibilityState === 'hidden' || held.current) return;
      void read().catch((reason) => { if (mounted.current && !(reason instanceof StaleRequest)) setFailure(reason instanceof ApiError ? reason : String(reason)); });
    };
    tick(); const timer = window.setInterval(tick, 5000);
    return () => { mounted.current = false; request.current += 1; window.clearInterval(timer); };
  }, [read, revision]);
  /** A certificate action pauses the poll and discards reads already in flight. */
  const hold = useCallback((active: boolean) => { held.current = active; if (active) request.current += 1; }, []);
  const publish = useCallback((view: CertificateView) => setStatus((current) => current && { ...current, ...view }), []);
  return <>
    <DohRuntime status={status} failed={failure !== null} language={language} />
    <CertificateTools language={language} view={status} readFailure={failure} read={read} hold={hold} publish={publish} />
  </>;
}
