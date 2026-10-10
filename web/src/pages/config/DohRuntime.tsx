import { translate, type Language } from '../../i18n';
import { splitListener } from '../../model';

export interface DohStatus { running: boolean; doh_listen: string | null; doh_http3: boolean }

/** The bound address comes only from status, including the selected port for :0. */
export function DohRuntime({ status, failed, language }: { status: DohStatus | null; failed: boolean; language: Language }) {
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
