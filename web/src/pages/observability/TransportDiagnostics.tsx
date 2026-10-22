import { formatDate, formatNumber, translate, type Language } from '../../i18n';
import { diagnosticLabel, protocolLabel, type QuicDiagnostics, type RecursionDiagnostics, type UpstreamDiagnostics, type UpstreamResult, type UpstreamTrace } from './diagnostics';

function Result({ value, language }: { value: UpstreamResult; language: Language }) {
  return <span>{protocolLabel(value.protocol, language)} · {diagnosticLabel(`stage_${value.stage}`, language)} · {diagnosticLabel(`outcome_${value.outcome}`, language)}{value.reason && <> · {diagnosticLabel(`upstream_reason_${value.reason}`, language)}</>}</span>;
}

export function UpstreamAttempts({ trace, language }: { trace: UpstreamTrace; language: Language }) {
  return <section><h3>{translate('reliability.attempts', language)}</h3>
    {!trace.attempts.length && <p className="muted">{translate('reliability.noAttempts', language)}</p>}
    <ol className="attempt-list">{trace.attempts.map((attempt, index) => <li key={index}>
      <p>{translate('reliability.slot', language, { slot: attempt.slot + 1 })} · {formatNumber(attempt.elapsed_ms, language, { maximumFractionDigits: 2 })} ms{attempt.connection && <> · {translate(`reliability.connection_${attempt.connection}`, language)}</>}</p>
      <p><Result value={attempt} language={language} /></p>
      {attempt.code && <p className="small"><span>{translate('reliability.code', language)}: </span><code>{attempt.code}</code></p>}
    </li>)}</ol>
    {trace.omitted > 0 && <p className="small muted">{translate('reliability.omitted', language, { count: trace.omitted })}</p>}
  </section>;
}

// Known counters first in a stable order (the server object is keyed alphabetically), then any newer ones verbatim.
const RECURSION_GROUPS = [
  ['resolutions', 'resolution', ['started', 'answered', 'negative', 'bogus', 'failed', 'deadline', 'shutdown']],
  ['failures', 'failure', ['timeout', 'referral', 'budget', 'other']],
  ['dnssec', 'dnssec', ['secure', 'insecure', 'bogus', 'indeterminate']],
  ['ecs_scope', 'ecs', ['not_sent', 'absent', 'scoped']],
  ['outbound', 'outbound', ['sent', 'inflight', 'rate_waits', 'rate_wait_ms', 'refused_after_shutdown']],
] as const;

export function RecursionDiagnosticsPanel({ value, language }: { value: RecursionDiagnostics; language: Language }) {
  return <section className="panel"><h2>{translate('reliability.recursion', language)}</h2>
    <p className="small muted">{translate('reliability.recursionScope', language)}</p>
    <div className="storage-grid">{RECURSION_GROUPS.map(([group, prefix, known]) => {
      const counts = value[group];
      const names = [...known.filter((name) => name in counts), ...Object.keys(counts).filter((name) => !(known as readonly string[]).includes(name))];
      return <section key={group}><h3>{translate(`reliability.recursion_${group}`, language)}</h3>
        <dl className="diagnostic-counts">{names.map((name) => <div key={name}><dt>{diagnosticLabel(`${prefix}_${name}`, language)}</dt><dd>{formatNumber(counts[name], language)}</dd></div>)}</dl>
      </section>;
    })}</div>
  </section>;
}

export function TransportDiagnostics({ upstreams, recursion, quic, language }: { upstreams: UpstreamDiagnostics | null; recursion: RecursionDiagnostics | null; quic: QuicDiagnostics; language: Language }) {
  return <div className="settings-stack">
    {recursion && <RecursionDiagnosticsPanel value={recursion} language={language} />}
    {upstreams && <section className="panel"><h2>{translate('reliability.upstreams', language)}</h2>
      <p className="small muted">{translate('reliability.upstreamScope', language, { time: formatDate(upstreams.since_ms, language, { dateStyle: 'short', timeStyle: 'medium' }) })}</p>
      {upstreams.slots.map((slot) => {
        const endpoint = upstreams.endpoints.find((item) => item.slot === slot.slot);
        return <details key={slot.slot}><summary>{endpoint
          ? <><span className="mono">{endpoint.label}</span>{endpoint.address !== endpoint.label && <span className="muted"> · {translate('reliability.reachedAt', language, { address: endpoint.address })}</span>}</>
          : translate('reliability.slot', language, { slot: slot.slot + 1 })}</summary>
        {!slot.counts.length && <p className="muted">{translate('reliability.noAttempts', language)}</p>}
        <dl className="diagnostic-counts">{slot.counts.map((count, index) => <div key={index}><dt><Result value={count} language={language} /></dt><dd>{formatNumber(count.count, language)}</dd></div>)}</dl>
      </details>;
      })}
    </section>}
    <section className="panel"><h2>{translate('reliability.quic', language)}</h2><p className="small muted">{translate('reliability.processScope', language)}</p>
      <p className="small muted">{translate('reliability.quicHelp', language)}</p>
      <div className="storage-grid">{(['doq', 'doh3'] as const).map((protocol) => <section key={protocol}><h3>{protocol === 'doq' ? 'DoQ' : 'DoH / HTTP/3'}</h3>
        <dl className="diagnostic-counts">{['handshake_inflight', 'handshake_established', 'stream_inflight', 'stream_response_handed_to_transport'].map((name) => <div key={name}><dt>{diagnosticLabel(name, language)}</dt><dd>{formatNumber(quic[protocol][name], language)}</dd></div>)}</dl>
        <details><summary>{translate('views.details', language)}</summary><dl className="diagnostic-counts">{Object.entries(quic[protocol]).map(([name, count]) => <div key={name}><dt>{diagnosticLabel(name, language)}</dt><dd>{formatNumber(count, language)}</dd></div>)}</dl></details>
      </section>)}</div>
    </section>
  </div>;
}
