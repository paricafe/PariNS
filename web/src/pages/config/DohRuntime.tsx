import { createContext, useContext } from 'react';
import { translate, type Language } from '../../i18n';
import { splitListener } from '../../model';
import type { StatusView } from '../observability/metrics';

export type DohStatus = Pick<StatusView, 'running' | 'doh_listen' | 'doh_http3'>;

/** The security page's one status read, shared with the DoH group's in-service row. */
export const DohStatusContext = createContext<{ status: DohStatus | null; failed: boolean } | null>(null);

/** One row in the DoH group; the bound address comes only from status, including the port chosen for :0. */
export function DohRuntime({ status, failed, language }: { status: DohStatus | null; failed: boolean; language: Language }) {
  const address = status?.running ? status.doh_listen : null;
  return <div className="row" data-layout="wide">
    <div className="row-label"><span>{translate('storage.runningDoh', language)}</span></div>
    <div className="row-control">{status === null ? <span className="row-value muted">{translate('app.reading', language)}</span> : address
      ? <span className="row-value"><span className="transport-address">{address} · /dns-query</span><br />
        <span className="muted small">{translate(status.doh_http3 ? 'storage.runningDoh3Summary' : 'storage.runningDohSummary', language, { port: splitListener(address).port })}</span></span>
      : <span className="row-value muted">{translate('storage.dohNotRunning', language)}</span>}</div>
    {failed && <small className="row-note" role="status">{translate('storage.dohStatusUnavailable', language)}</small>}
  </div>;
}

/** Renders the in-service row where the security page provides status; elsewhere nothing. */
export function DohRuntimeRow({ language }: { language: Language }) {
  const context = useContext(DohStatusContext);
  return context ? <DohRuntime status={context.status} failed={context.failed} language={language} /> : null;
}
