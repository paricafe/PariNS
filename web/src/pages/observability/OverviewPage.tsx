import { useEffect, useRef, useState } from "react";
import type { ApiClient } from "../../session/client";
import { formatDate, formatNumber, translate, type Language } from "../../i18n";
import { TrendChart } from "./TrendChart";
import { averageLatencyMillis, cacheHitRate, histogramRows, type ListenerStatus, type StatsView, type StatusView } from "./metrics";
import "./observability.css";
import { Odometer } from '../../components/Odometer';
import { Disclosure } from '../../components/Disclosure';
import { InfoTip } from '../../components/InfoTip';
import { Segmented } from '../../components/Segmented';
import { StorageStatusPanel } from './StorageStatusPanel';
import { CacheDiagnosticsPanel } from './CacheDiagnosticsPanel';
import { TransportDiagnostics } from './TransportDiagnostics';
import { Ban, Gauge, RefreshCw, Search, Zap } from 'lucide-react';

export interface OverviewPageProps {
  api: ApiClient;
  language: Language;
  onOpenDns: () => void;
  /** Presentation only: number of configured upstream servers, when known. */
  upstreamCount?: number | null;
}

type Tone = 'ok' | 'warn' | 'bad' | 'idle';
interface UpstreamSample { at: number; operations: number; failures: number }

function value(value: number | null | undefined, language: Language): string {
  return value === null || value === undefined || !Number.isFinite(value) ? "—" : formatNumber(value, language);
}

function uptime(seconds: number | null, language: Language): string {
  if (seconds === null || !Number.isFinite(seconds)) return "—";
  const days = Math.floor(seconds / 86400); const hours = Math.floor(seconds / 3600) % 24; const minutes = Math.floor(seconds / 60) % 60;
  if (days > 0) return translate("shell.durationDays", language, { days, hours });
  if (hours > 0) return translate("shell.durationHours", language, { hours, minutes });
  return minutes > 0 ? translate("shell.durationMinutes", language, { minutes }) : translate("shell.durationJustNow", language);
}

/**
 * Display-only signal: most upstream operations failed over roughly the last minute.
 * Null until two samples give a delta, so a fresh page does not claim upstreams are healthy.
 */
function upstreamTrouble(samples: readonly UpstreamSample[]): boolean | null {
  if (samples.length < 2) return null;
  const first = samples[0]; const last = samples[samples.length - 1];
  const operations = last.operations - first.operations; const failures = last.failures - first.failures;
  return operations >= 3 && failures / operations >= 0.5;
}

function ResultBars({ rows, language }: { rows: readonly { label: string; count: number; tone?: Tone }[] | null; language: Language }) {
  if (!rows) return <div className="skeleton-stack" aria-hidden="true">{[0, 1, 2, 3].map((index) => <div key={index} className="skeleton" style={{ height: 18 }} />)}</div>;
  const total = rows.reduce((sum, row) => sum + row.count, 0);
  return <ul className="result-bars">{rows.map((row) => {
    const share = total > 0 ? row.count / total * 100 : 0;
    const percent = formatNumber(share, language, { maximumFractionDigits: 1 });
    return <li key={row.label} data-tone={row.tone}>
      <span className="result-label">{row.label}</span>
      <span className="result-track" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(share * 10) / 10}
        aria-label={translate("views.share", language, { label: row.label, percent })}><i style={{ width: `${share}%` }} /></span>
      <span className="result-value num">{percent}%</span>
      <span className="result-count num">{value(row.count, language)}</span>
    </li>;
  })}</ul>;
}

function Distribution({ rows, language }: { rows: readonly { label: string; count: number }[] | null; language: Language }) {
  if (!rows) return <p className="muted small">{translate("views.chartUnavailable", language)}</p>;
  const total = rows.reduce((sum, row) => sum + row.count, 0);
  return <div className="table-scroll">
    <table className="data-table">
      <thead><tr>
        <th scope="col">{translate("views.category", language)}</th>
        <th scope="col" className="numeric">{translate("views.count", language)}</th>
        <th scope="col">{translate("views.distribution", language)}</th>
      </tr></thead>
      <tbody>{rows.map((row) => {
        const share = total > 0 ? row.count / total * 100 : 0;
        return <tr key={row.label}>
          <th scope="row">{row.label}</th>
          <td className="numeric">{value(row.count, language)}</td>
          <td><div className="meter-row">
            <meter className="observability-meter" min="0" max={total || 1} value={row.count}
              aria-label={translate("views.share", language, { label: row.label, percent: formatNumber(share, language, { maximumFractionDigits: 1 }) })} />
            <span className="num muted small">{formatNumber(share, language, { maximumFractionDigits: 1 })}%</span>
          </div></td>
        </tr>;
      })}</tbody>
    </table>
  </div>;
}

export function OverviewPage({ api, language, onOpenDns, upstreamCount = null }: OverviewPageProps) {
  const [status, setStatus] = useState<StatusView | null>(null);
  const [stats, setStats] = useState<StatsView | null>(null);
  const [updated, setUpdated] = useState<number | null>(null);
  const [statusFailed, setStatusFailed] = useState(false);
  const [statsFailed, setStatsFailed] = useState(false);
  const [range, setRange] = useState('1h');
  const [customFrom, setCustomFrom] = useState('');
  const [customTo, setCustomTo] = useState('');
  const [query, setQuery] = useState<Record<string, string>>({ range: '1h' });
  // The range of the samples on screen: the chart keeps its axis until the new range's samples arrive.
  const [shown, setShown] = useState(query);
  const [rangeError, setRangeError] = useState(false);
  const [refreshKey, refresh] = useState(0);
  const upstreamSamples = useRef<UpstreamSample[]>([]);
  const [troubled, setTroubled] = useState<boolean | null>(null);

  useEffect(() => {
    let active = true;
    let statusBusy = false;
    let statsBusy = false;
    let lastStats = 0;
    async function readStatus() {
      if (statusBusy) return;
      statusBusy = true;
      try {
        const next = await api.request<StatusView>("status");
        if (!active) return;
        setStatus(next); setUpdated(Date.now()); setStatusFailed(false);
        const counters = next.metrics.counters;
        const now = Date.now();
        const samples = [...upstreamSamples.current, { at: now, operations: counters.upstream_operations ?? 0, failures: counters.upstream_failures ?? 0 }]
          .filter((sample) => now - sample.at <= 65_000);
        // A restart resets process counters; start a new window.
        upstreamSamples.current = samples.some((sample, index) => index > 0 && sample.operations < samples[index - 1].operations) ? samples.slice(-1) : samples;
        setTroubled(upstreamTrouble(upstreamSamples.current));
      } catch {
        if (active) setStatusFailed(true);
      } finally { statusBusy = false; }
    }
    async function readStats() {
      if (statsBusy) return;
      statsBusy = true;
      try {
        const next = await api.request<StatsView>("stats", 'GET', undefined, undefined, { query });
        if (active) { setStats(next); setShown(query); setStatsFailed(false); lastStats = Date.now(); }
      } catch {
        if (active) setStatsFailed(true);
      } finally { statsBusy = false; }
    }
    function tick() {
      if (document.visibilityState === "hidden") return;
      void readStatus();
      if (Date.now() - lastStats >= 60_000) void readStats();
    }
    tick();
    const interval = window.setInterval(tick, 5_000);
    document.addEventListener("visibilitychange", tick);
    return () => { active = false; window.clearInterval(interval); document.removeEventListener("visibilitychange", tick); };
  }, [api, refreshKey, query]);

  const t = (key: string, params?: Record<string, string | number>) => translate(key, language, params);
  const running = status?.running === true;
  const counters = stats?.totals.metrics.counters;
  const latency = stats?.totals.metrics.request_latency;
  const hours = shown.range === 'custom' ? (Number(shown.to_ms) - Number(shown.from_ms)) / 3_600_000 : shown.range === '7d' ? 168 : shown.range === '24h' ? 24 : 1;
  const hitRate = cacheHitRate(counters);
  const average = averageLatencyMillis(latency);
  const blocked = counters ? (counters.query_blocked ?? 0) + (counters.response_blocked ?? 0) : null;
  const health = status?.dns_health.state;
  const recursive = status?.resolution_mode === 'recursive';
  const [headline, tone]: [string, Tone] = status === null ? [t('shell.headlineLoading'), 'idle']
    : health === 'failed' ? [t('shell.headlineFailed'), 'bad']
      : health === 'degraded' ? [t('shell.headlineDegraded'), 'warn']
      : health === 'unconfigured' ? [t('shell.headlineUnconfigured'), 'idle']
        : !running || health === 'stopped' ? [t('shell.headlineStopped'), 'idle']
          : health === 'starting' || !status.dns_health.ready ? [t('shell.headlineStarting'), 'warn']
            : troubled ? [t(recursive ? 'shell.headlineRecursion' : 'shell.headlineUpstream'), 'warn'] : troubled === null ? [t('shell.headlineRunning'), 'ok'] : [t('shell.headlineHealthy'), 'ok'];
  const summary = status === null ? null
    : health === 'failed' ? t('shell.summaryFailed')
      : health === 'degraded' ? t('shell.summaryDegraded')
      : !running || health === 'stopped' || health === 'unconfigured' ? t('shell.summaryStopped')
        : health === 'starting' || !status.dns_health.ready ? t('shell.summaryStarting')
          : troubled ? t(recursive ? 'shell.summaryRecursion' : 'shell.summaryUpstream')
            : counters ? hitRate === null ? t('shell.summaryQueriesOnly', { count: value(counters.requests, language) })
              : t('shell.summaryQueries', { count: value(counters.requests, language), percent: formatNumber(hitRate, language, { maximumFractionDigits: 0 }) }) : null;
  const kpis = [
    { key: "requests", icon: Search, label: t("ui.requests"), raw: counters?.requests ?? null, format: (n: number) => formatNumber(Math.round(n), language) },
    { key: "cache", icon: Zap, label: t("ui.hitRate"), tip: t('shell.hitRateHelp'), raw: hitRate, format: (n: number) => `${formatNumber(n, language, { minimumFractionDigits: 1, maximumFractionDigits: 1 })}%` },
    { key: "latency", icon: Gauge, label: t("ui.latency"), raw: average, format: (n: number) => `${formatNumber(n, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms` },
    { key: "blocked", icon: Ban, label: t("ui.blocked"), raw: blocked, format: (n: number) => formatNumber(Math.round(n), language) },
  ];
  const responses = counters ? ([
    ["responses_noerror", "app.rcodeSuccess", 'ok'], ["responses_nxdomain", "app.rcodeMissing", undefined],
    ["responses_servfail", "app.rcodeFailed", 'warn'], ["responses_refused", "app.rcodeRefused", 'bad'],
    ["responses_other", "app.rcodeOther", undefined],
  ] as const).map(([key, label, rowTone]) => ({ label: t(label), count: counters[key] ?? 0, tone: rowTone as Tone | undefined })) : null;
  const distribution = latency ? histogramRows(latency.buckets).map((row) => ({
    label: row.upperMillis === null ? `> ${formatNumber(row.lowerMillis, language)} ms`
      : row.lowerMillis === 0 ? `≤ ${formatNumber(row.upperMillis, language)} ms`
        : `> ${formatNumber(row.lowerMillis, language)}–${formatNumber(row.upperMillis, language)} ms`,
    count: row.count,
  })) : null;

  return <div className="page-stack overview">
    <header className="overview-head large-title">
      <div>
        <h1 aria-live="polite">{headline}</h1>
        {summary ? <p className="overview-summary">{summary}</p> : <div className="skeleton" style={{ width: 'min(420px, 80%)', height: 16 }} aria-hidden="true" />}
      </div>
      <button type="button" className="button secondary" onClick={() => refresh((key) => key + 1)}><RefreshCw size={16} aria-hidden="true" />{t("ui.refresh")}</button>
    </header>

    <section className="panel status-strip" data-tone={tone} aria-label={t("ui.maintenance")}>
      <span className="pulse" aria-hidden="true" />
      <div className="status-strip-main">
        {status === null ? <strong>{t("ui.loadingStatus")}</strong> : !(running && status.dns_health.ready) && <strong>{t(`reliability.${health === 'failed' ? status.dns_health.code === 'startup_failed' ? 'startupFailed' : 'runtimeFailed' : health}`)}</strong>}
        <span className={running && status?.dns_health.ready ? undefined : 'muted'}>{status && !status.dns_health.ready && <span>{t('reliability.notReady')}</span>}{status?.listen ? <>{!status.dns_health.ready && ' · '}{t('shell.statusListening', { address: status.listen })}</> : health === 'unconfigured' && <> · {t("app.notConfigured")}</>}{running && status?.uptime_seconds != null && <> · {t('shell.statusUp', { duration: uptime(status.uptime_seconds, language) })}</>}</span>
      </div>
      <dl className="status-strip-facts">
        <div><dt>{t('shell.stripInflight')}</dt><dd className="num">{running ? value(status?.metrics.request_inflight, language) : "—"}</dd></div>
        <div><dt>{t('shell.stripResolution')}</dt><dd>{status?.resolution_mode ? t(`settings.resolution.mode.${status.resolution_mode}`) : "—"}</dd></div>
        {status?.resolution_mode !== 'recursive' && <div><dt>{t('shell.stripUpstreams')}</dt><dd>{upstreamCount === null ? "—" : t('shell.upstreamCount', { count: upstreamCount })}</dd></div>}
      </dl>
    </section>
    {statusFailed && <p role="alert" className="notice error">{t("app.statusUnavailable")}{updated !== null && <> {t('shell.lastUpdated', { time: formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })}</>}</p>}
    {status?.listeners?.some((l) => l.state === 'retrying') && <ListenerFailures listeners={status.listeners.filter((l) => l.state === 'retrying')} t={t} />}
    {status?.last_error && (status.dns_health.code === 'startup_failed'
      ? <StartupFailure error={status.last_error} t={t} />
      : <p role="alert" className="notice error">{status.last_error}</p>)}

    <section aria-label={t("ui.statsScope")} className="kpi-grid">
      {kpis.map((item) => <div key={item.key} className="kpi-card glass">
        <p className="kpi-label"><item.icon size={15} aria-hidden="true" />{item.label}{"tip" in item && item.tip && <InfoTip text={item.tip} label={t('shell.about', { label: item.label })} />}</p>
        <p className="kpi-value num">{item.raw === null ? (stats || statsFailed ? "—" : <span className="skeleton" style={{ display: 'inline-block', width: 96, height: 30 }} aria-hidden="true" />) : <Odometer text={item.format(item.raw)} />}</p>
      </div>)}
    </section>

    <div className="overview-charts">
      <section className="panel" aria-labelledby="parins-trend-title">
        <div className="panel-head">
          <h2 id="parins-trend-title">{t("ui.activity")}</h2>
        </div>
        <Segmented className="trend-range" label={t('ui.trendRange')} options={['1h', '24h', '7d', 'custom'].map((range, index) => ({ value: range, label: t(`storage.${['hour1', 'hour24', 'day7', 'custom'][index]}`) }))} value={range} onChange={(next) => { setRange(next); setRangeError(false); if (next !== 'custom') setQuery({ range: next }); }} />
        {range === 'custom' && <form className="custom-range" onSubmit={(event) => {
          event.preventDefault(); const from = new Date(customFrom).getTime(); const to = new Date(customTo).getTime();
          if (!Number.isFinite(from) || !Number.isFinite(to) || from >= to || from < 0) { setRangeError(true); return; }
          setRangeError(false); setQuery({ range: 'custom', from_ms: String(from), to_ms: String(to) });
        }}><label className="field">{t('storage.from')}<input type="datetime-local" value={customFrom} onChange={(event) => setCustomFrom(event.target.value)} required /></label><label className="field">{t('storage.to')}<input type="datetime-local" value={customTo} onChange={(event) => setCustomTo(event.target.value)} required /></label><button className="button secondary" type="submit">{t('ui.filter')}</button></form>}
        {rangeError && <p role="alert" className="error small">{t('storage.rangeInvalid')}</p>}
        <div className="trend-body">{statsFailed && <p role="status" className="muted small">{t("app.statsUnavailable")}</p>}
          <TrendChart samples={stats?.samples ?? []} hours={hours} end={shown.range === 'custom' ? Number(shown.to_ms) : undefined} language={language} /></div>
        <div className="trend-legend">
          {(["legendRequests", "legendCache", "legendBlocked"] as const).map((key, index) => <span key={key} data-series={index}>{t(`ui.${key}`)}</span>)}
        </div>
      </section>
      <section className="panel" aria-labelledby="parins-response-title">
        <h2 id="parins-response-title">{t("ui.responses")}</h2>
        <p className="muted small">{t("ui.responsesHelp")}</p>
        <ResultBars rows={responses} language={language} />
      </section>
    </div>

    <Disclosure title={t('shell.advanced')}>
      <div className="advanced-stack">
        {status && <p className="small muted">{t('reliability.runtimeGeneration', { time: formatDate(status.dns_health.changed_at_ms, language, { dateStyle: 'short', timeStyle: 'medium' }) })}{status.dns_health.code && <> · <code>{status.dns_health.code}</code></>}</p>}
        <p className="small muted">{t("ui.statsHelp")}{stats && <> {t('storage.totalsSince', { time: formatDate(stats.totals.since_ms, language, { dateStyle: 'medium', timeStyle: 'short' }) })}</>}</p>
        {status && <StorageStatusPanel status={status.storage} language={language} compact />}
        <section className="panel" aria-labelledby="parins-latency-title">
          <h2 id="parins-latency-title">{t("ui.latencyDistribution")}</h2>
          <p className="muted small">{t("ui.latencyDistributionHelp")}</p>
          <Distribution rows={distribution} language={language} />
        </section>
        {status?.diagnostics.cache && <CacheDiagnosticsPanel value={status.diagnostics.cache} language={language} />}
        {status && <TransportDiagnostics upstreams={status.diagnostics.upstreams} recursion={status.diagnostics.recursion} quic={status.diagnostics.quic} language={language} />}
        <section className="panel" aria-labelledby="parins-service-title">
          <div className="panel-head"><h2 id="parins-service-title">{t('shell.serviceDetails')}</h2>
            <button type="button" className="button quiet" onClick={onOpenDns}>{t("ui.openDns")}</button></div>
          <dl className="fact-grid">
            <div><dt>{t("ui.dnsAddress")}</dt><dd className="transport-address">{status?.listen ?? t("app.notConfigured")}</dd></div>
            <div><dt>{t("ui.uptime")}</dt><dd className="num">{running ? uptime(status?.uptime_seconds ?? null, language) : "—"}</dd></div>
            <div><dt>{t("ui.inflight")}</dt><dd className="num">{running ? value(status?.metrics.request_inflight, language) : "—"}</dd></div>
            <div><dt>{t("ui.failures")}</dt><dd className="num">{value(counters?.upstream_failures, language)}</dd></div>
            <div><dt>{t("ui.updated")}</dt><dd className="num">{updated === null ? "—" : formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" })} <span className="muted small">{t("ui.statusInterval")}</span></dd></div>
          </dl>
        </section>
      </div>
    </Disclosure>
  </div>;
}

/** Protocol names are the same in every language; the metrics endpoint (`admin_listen`) has a translated name. */
const LISTENER_LABELS: Record<Exclude<ListenerStatus['kind'], 'admin'>, string> = { dns: 'DNS', dot: 'DoT', doh: 'DoH', doh3: 'DoH HTTP/3', doq: 'DoQ' };

/** Listeners of a running generation that are being rebound on their own; the rest keep serving. */
function ListenerFailures({ listeners, t }: { listeners: ListenerStatus[]; t: (key: string, params?: Record<string, string | number>) => string }) {
  return <div role="alert" className="notice error startup-failure">
    <strong>{t('shell.listenerDownTitle')}</strong>
    {listeners.map((listener) => {
      const protocol = listener.kind === 'admin' ? t('shell.listenerMetrics') : LISTENER_LABELS[listener.kind];
      const conflict = listener.error !== null && /address (?:already )?in use/i.test(listener.error);
      return <p key={`${listener.kind}-${listener.address}`}>{conflict ? t('shell.startupPortInUse', { protocol, address: listener.address }) : t('shell.listenerDown', { protocol, address: listener.address })}
        {listener.error && <><br /><code>{listener.error}</code></>}</p>;
    })}
    <p>{t('shell.listenerRetrying')}</p>
  </div>;
}

/** A saved configuration that cannot start (for example unreadable certificate material); Rust retries it automatically. */
function StartupFailure({ error, t }: { error: string; t: (key: string, params?: Record<string, string | number>) => string }) {
  const conflict = /bind (.+?) (\S+:\d+): address (?:already )?in use/i.exec(error);
  return <div role="alert" className="notice error startup-failure">
    <strong>{t('shell.startupFailedTitle')}</strong>
    {conflict && <p>{t('shell.startupPortInUse', { protocol: conflict[1] === 'admin' ? t('shell.listenerMetrics') : conflict[1], address: conflict[2] })}</p>}
    <p>{t('shell.startupRetrying')}</p>
    <code>{error}</code>
  </div>;
}
