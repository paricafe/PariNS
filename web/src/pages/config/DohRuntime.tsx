import { useEffect, useState } from 'react';
import { translate, type Language } from '../../i18n';
import type { ApiClient } from '../../session/client';
import { splitListener } from '../../model';

interface DohStatus { running: boolean; doh_listen: string | null; doh_http3: boolean }

/** The bound address comes only from status, including the selected port for :0. */
export function DohRuntime({ api, language }: { api: ApiClient; language: Language }) {
  const [status, setStatus] = useState<DohStatus | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    let busy = false;
    const read = async () => {
      if (busy || document.visibilityState === 'hidden') return;
      busy = true;
      try { const next = await api.request<DohStatus>('status'); if (active) { setStatus(next); setFailed(false); } }
      catch { if (active) setFailed(true); }
      finally { busy = false; }
    };
    void read(); const timer = window.setInterval(() => void read(), 5000);
    return () => { active = false; window.clearInterval(timer); };
  }, [api]);
  const address = status?.running ? status.doh_listen : null;
  return <section className="panel" aria-label={translate('storage.runningDoh', language)}>
    <h2>{translate('storage.runningDoh', language)}</h2>
    {failed && <p className="notice" role="status">{translate('storage.dohStatusUnavailable', language)}</p>}
    {status === null ? <p className="muted">{translate('app.reading', language)}</p> : address ? <>
      <p className="transport-address">{address} · /dns-query</p>
      <p>{translate(status.doh_http3 ? 'storage.runningDoh3Summary' : 'storage.runningDohSummary', language, { port: splitListener(address).port })}</p>
    </> : <p className="muted">{translate('storage.dohNotRunning', language)}</p>}
  </section>;
}
