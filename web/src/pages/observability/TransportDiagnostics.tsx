import { formatDate, formatNumber, translate, type Language } from '../../i18n';
import { diagnosticLabel, protocolLabel, type QuicDiagnostics, type UpstreamDiagnostics, type UpstreamResult, type UpstreamTrace } from './diagnostics';

function Result({ value, language }: { value: UpstreamResult; language: Language }) {
  return <span>{protocolLabel(value.protocol, language)} · {diagnosticLabel(`stage_${value.stage}`, language)} · {diagnosticLabel(`outcome_${value.outcome}`, language)}{value.reason && <> · {diagnosticLabel(`upstream_reason_${value.reason}`, language)}</>}</span>;
}

export function UpstreamAttempts({ trace, language }: { trace: UpstreamTrace; language: Language }) {
  return <section><h3>{translate('reliability.attempts', language)}</h3>
    {!trace.attempts.length && <p className="muted">{translate('reliability.noAttempts', language)}</p>}
    <ol className="attempt-list">{trace.attempts.map((attempt, index) => <li key={index}>
      <p>{translate('reliability.slot', language, { slot: attempt.slot })} · #{attempt.pool_generation} · {formatNumber(attempt.elapsed_ms, language, { maximumFractionDigits: 2 })} ms</p>
      <p><Result value={attempt} language={language} /></p>
      {attempt.code && <p className="small"><span>{translate('reliability.code', language)}: </span><code>{attempt.code}</code></p>}
    </li>)}</ol>
    {trace.omitted > 0 && <p className="small muted">{translate('reliability.omitted', language, { count: trace.omitted })}</p>}
  </section>;
}

export function TransportDiagnostics({ upstreams, quic, language }: { upstreams: UpstreamDiagnostics | null; quic: QuicDiagnostics; language: Language }) {
  return <div className="settings-stack">
    {upstreams && <section className="panel"><h2>{translate('reliability.upstreams', language)}</h2>
      <p className="small muted">{translate('reliability.upstreamScope', language, { generation: upstreams.pool_generation, time: upstreams.since_ms === null ? translate('reliability.unknownTime', language) : formatDate(upstreams.since_ms, language, { dateStyle: 'short', timeStyle: 'medium' }) })} <code>{upstreams.scope}</code></p>
      {upstreams.slots.map((slot) => <details key={slot.slot}><summary>{translate('reliability.slot', language, { slot: slot.slot })}</summary>
        {!slot.counts.length && <p className="muted">{translate('reliability.noAttempts', language)}</p>}
        <dl className="diagnostic-counts">{slot.counts.map((count, index) => <div key={index}><dt><Result value={count} language={language} /></dt><dd>{formatNumber(count.count, language)}</dd></div>)}</dl>
      </details>)}
    </section>}
    <section className="panel"><h2>{translate('reliability.quic', language)}</h2><p className="small muted">{translate('reliability.processScope', language)} <code>{quic.scope}</code></p>
      <p className="small muted">{translate('reliability.quicHelp', language)}</p>
      <div className="storage-grid">{(['doq', 'doh3'] as const).map((protocol) => <section key={protocol}><h3>{protocol === 'doq' ? 'DoQ' : 'DoH / HTTP/3'}</h3>
        <dl className="diagnostic-counts">{['handshake_inflight', 'handshake_established', 'stream_inflight', 'stream_response_handed_to_transport'].map((name) => <div key={name}><dt>{diagnosticLabel(name, language)}</dt><dd>{formatNumber(quic[protocol][name], language)}</dd></div>)}</dl>
        <details><summary>{translate('views.details', language)}</summary><dl className="diagnostic-counts">{Object.entries(quic[protocol]).map(([name, count]) => <div key={name}><dt>{diagnosticLabel(name, language)}</dt><dd>{formatNumber(count, language)}</dd></div>)}</dl></details>
      </section>)}</div>
    </section>
  </div>;
}
