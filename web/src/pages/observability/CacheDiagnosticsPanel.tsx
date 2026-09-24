import { formatDate, formatNumber, translate, type Language } from '../../i18n';
import { diagnosticLabel, type CacheDiagnostics } from './diagnostics';

export function CacheDiagnosticsPanel({ value, language }: { value: CacheDiagnostics; language: Language }) {
  return <section className="panel" aria-label={translate('reliability.diagnostics', language)}>
    <h2>{translate('reliability.diagnostics', language)}</h2>
    <p className="small muted">{translate('reliability.diagnosticsScope', language, { time: value.since_ms === null ? translate('reliability.unknownTime', language) : formatDate(value.since_ms, language, { dateStyle: 'short', timeStyle: 'medium' }) })} <code>{value.scope}</code></p>
    <div className="storage-grid">{(['lookup', 'store'] as const).map((kind) => <section key={kind}>
      <h3>{translate(`reliability.${kind}`, language)}</h3>
      <dl className="diagnostic-counts">{Object.entries(value[kind]).filter(([name]) => name !== 'reasons').map(([name, count]) => <div key={name}><dt>{diagnosticLabel(`${kind}_${name}`, language)}</dt><dd>{formatNumber(count as number, language)}</dd></div>)}</dl>
      <details><summary>{translate('reliability.reason', language)}</summary><dl className="diagnostic-counts">{Object.entries(value[kind].reasons).map(([reason, count]) => <div key={reason}><dt>{diagnosticLabel(`reason_${reason}`, language)}</dt><dd>{formatNumber(count, language)}</dd></div>)}</dl></details>
    </section>)}</div>
  </section>;
}
