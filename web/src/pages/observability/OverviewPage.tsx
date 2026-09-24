import { useEffect, useState } from "react";
import type { ApiClient } from "../../session/client";
import { formatDate, formatNumber, translate, type Language } from "../../i18n";
import { TrendChart } from "./TrendChart";
import { averageLatencyMillis, cacheHitRate, histogramRows, type StatsView, type StatusView } from "./metrics";
import "./observability.css";
import { Tabs } from '../../components/beui';
import { StorageStatusPanel } from './StorageStatusPanel';
import { CacheDiagnosticsPanel } from './CacheDiagnosticsPanel';
import { TransportDiagnostics } from './TransportDiagnostics';

export interface OverviewPageProps {
  api: ApiClient;
  language: Language;
  onOpenDns: () => void;
}

function value(value: number | null | undefined, language: Language): string {
  return value === null || value === undefined || !Number.isFinite(value) ? "—" : formatNumber(value, language);
}

function uptime(seconds: number | null, language: Language): string {
  if (seconds === null || !Number.isFinite(seconds)) return "—";
  return translate("app.uptime", language, {
    days: Math.floor(seconds / 86400), hours: Math.floor(seconds / 3600) % 24, minutes: Math.floor(seconds / 60) % 60,
  });
}

function Distribution({ rows, language }: { rows: readonly { label: string; count: number }[] | null; language: Language }) {
  if (!rows) return <p className="py-8 text-center text-sm text-zinc-500 dark:text-zinc-400">{translate("views.chartUnavailable", language)}</p>;
  const total = rows.reduce((sum, row) => sum + row.count, 0);
  return <div className="min-w-0 overflow-x-auto">
    <table className="w-full text-left text-sm">
      <thead><tr className="border-b border-zinc-200 text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
        <th scope="col" className="px-2 py-2 font-medium">{translate("views.category", language)}</th>
        <th scope="col" className="px-2 py-2 text-right font-medium">{translate("views.count", language)}</th>
        <th scope="col" className="px-2 py-2 font-medium">{translate("views.distribution", language)}</th>
      </tr></thead>
      <tbody>{rows.map((row) => {
        const share = total > 0 ? row.count / total * 100 : 0;
        return <tr key={row.label} className="border-b border-zinc-100 last:border-0 dark:border-zinc-800/70">
          <th scope="row" className="px-2 py-2 font-normal">{row.label}</th>
          <td className="px-2 py-2 text-right tabular-nums">{value(row.count, language)}</td>
          <td className="min-w-24 px-2 py-2">
            <div className="flex items-center gap-2">
              <meter className="observability-meter h-2 min-w-12 flex-1" min="0" max={total || 1} value={row.count}
                aria-label={translate("views.share", language, { label: row.label, percent: formatNumber(share, language, { maximumFractionDigits: 1 }) })} />
              <span className="min-w-10 text-right text-xs tabular-nums text-zinc-500 dark:text-zinc-400">{formatNumber(share, language, { maximumFractionDigits: 1 })}%</span>
            </div>
          </td>
        </tr>;
      })}</tbody>
    </table>
  </div>;
}

export function OverviewPage({ api, language, onOpenDns }: OverviewPageProps) {
  const [status, setStatus] = useState<StatusView | null>(null);
  const [stats, setStats] = useState<StatsView | null>(null);
  const [updated, setUpdated] = useState<number | null>(null);
  const [statusFailed, setStatusFailed] = useState(false);
  const [statsFailed, setStatsFailed] = useState(false);
  const [range, setRange] = useState('1h');
  const [customFrom, setCustomFrom] = useState('');
  const [customTo, setCustomTo] = useState('');
  const [query, setQuery] = useState<Record<string, string>>({ range: '1h' });
  const [rangeError, setRangeError] = useState(false);
  const [refreshKey, refresh] = useState(0);

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
        if (active) { setStatus(next); setUpdated(Date.now()); setStatusFailed(false); }
      } catch {
        if (active) setStatusFailed(true);
      } finally { statusBusy = false; }
    }
    async function readStats() {
      if (statsBusy) return;
      statsBusy = true;
      try {
        const next = await api.request<StatsView>("stats", 'GET', undefined, undefined, { query });
        if (active) { setStats(next); setStatsFailed(false); lastStats = Date.now(); }
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

  const running = status?.running === true;
  const counters = stats?.totals.metrics.counters;
  const latency = stats?.totals.metrics.request_latency;
  const hours = query.range === 'custom' ? (Number(query.to_ms) - Number(query.from_ms)) / 3_600_000 : query.range === '7d' ? 168 : query.range === '24h' ? 24 : 1;
  const hitRate = cacheHitRate(counters);
  const average = averageLatencyMillis(latency);
  const summary = [
    { key: "requests", label: translate("ui.requests", language), help: translate("ui.currentInstance", language), value: value(counters?.requests, language) },
    { key: "cache", label: translate("ui.hitRate", language), help: translate("ui.hitRateHelp", language), value: hitRate === null ? "—" : `${formatNumber(hitRate, language, { minimumFractionDigits: 1, maximumFractionDigits: 1 })}%` },
    { key: "latency", label: translate("ui.latency", language), help: translate("ui.latencyHelp", language), value: average === null ? "—" : `${formatNumber(average, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms` },
    { key: "blocked", label: translate("ui.blocked", language), help: translate("ui.blockedHelp", language), value: counters ? value((counters.query_blocked ?? 0) + (counters.response_blocked ?? 0), language) : "—" },
  ];
  const responses = counters ? ([
    ["responses_noerror", "app.rcodeSuccess"], ["responses_nxdomain", "app.rcodeMissing"],
    ["responses_servfail", "app.rcodeFailed"], ["responses_refused", "app.rcodeRefused"],
    ["responses_other", "app.rcodeOther"],
  ] as const).map(([key, label]) => ({ label: translate(label, language), count: counters[key] ?? 0 })) : null;
  const distribution = latency ? histogramRows(latency.buckets).map((row) => ({
    label: row.upperMillis === null ? `> ${formatNumber(row.lowerMillis, language)} ms`
      : row.lowerMillis === 0 ? `≤ ${formatNumber(row.upperMillis, language)} ms`
        : `> ${formatNumber(row.lowerMillis, language)}–${formatNumber(row.upperMillis, language)} ms`,
    count: row.count,
  })) : null;

  return <div className="space-y-6 pb-28 md:pb-8">
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div><p className="text-xs font-semibold uppercase tracking-widest text-zinc-500 dark:text-zinc-400">{translate("ui.overviewEyebrow", language)}</p>
        <h1 className="mt-1 text-2xl font-semibold tracking-tight">{translate("ui.overview", language)}</h1></div>
      <button type="button" onClick={() => refresh((key) => key + 1)} className="min-h-11 rounded-md border border-zinc-400 px-3 text-sm hover:bg-zinc-100 focus-visible:outline-2 dark:border-zinc-600 dark:hover:bg-zinc-800">
        {translate("ui.refresh", language)}
      </button>
    </div>

    <section aria-label={translate("ui.maintenance", language)} className="flex flex-wrap items-center gap-x-4 gap-y-2 rounded-lg border border-zinc-200 bg-white px-4 py-3 text-sm dark:border-zinc-800 dark:bg-zinc-900">
      <span className="font-medium">{status === null ? translate("ui.loadingStatus", language) : translate(`reliability.${status.dns_health.state === 'failed' ? 'runtimeFailed' : status.dns_health.state}`, language)}</span>
      {status && <span>{translate(`reliability.${status.dns_health.ready ? 'ready' : 'notReady'}`, language)}</span>}
      {updated !== null && <span className="text-zinc-500 dark:text-zinc-400">{translate("ui.updated", language)} {formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" })}</span>}
      {status !== null && <span className="text-zinc-500 dark:text-zinc-400">{translate("ui.revision", language)} {status.revision}</span>}
      {statusFailed && <span role="alert" className="text-red-700 dark:text-red-300">{translate("app.statusUnavailable", language)}</span>}
      <span className="text-xs text-zinc-500 dark:text-zinc-400">{translate("ui.statusInterval", language)}</span>
    </section>
    {status?.last_error && <p role="alert" className="rounded-md border border-red-300 px-4 py-3 text-sm text-red-800 dark:border-red-900 dark:text-red-300">{status.last_error}</p>}
    {status && <p className="small muted">{translate('reliability.runtimeGeneration', language, { generation: status.dns_health.generation, time: formatDate(status.dns_health.changed_at_ms, language, { dateStyle: 'short', timeStyle: 'medium' }) })}{status.dns_health.code && <> · <code>{status.dns_health.code}</code></>}</p>}
    {status?.storage && <StorageStatusPanel status={status.storage} language={language} compact />}

    <section aria-label={translate("ui.statsScope", language)}>
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">{summary.map((item) => <div key={item.key} className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900">
        <p className="text-sm text-zinc-600 dark:text-zinc-400">{item.label}</p>
        <p className="mt-2 text-3xl font-semibold tabular-nums tracking-tight">{item.value}</p>
        <p className="mt-2 text-xs text-zinc-500 dark:text-zinc-400">{item.help}</p>
      </div>)}</div>
      <p className="mt-2 text-xs text-zinc-500 dark:text-zinc-400">{translate("ui.statsHelp", language)}</p>
      {stats && <p className="small muted">{translate('storage.totalsSince', language, { time: formatDate(stats.totals.since_ms, language, { dateStyle: 'medium', timeStyle: 'short' }) })}</p>}
    </section>

    <section className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900" aria-labelledby="parins-trend-title">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div><h2 id="parins-trend-title" className="font-semibold">{translate("ui.activity", language)}</h2>
          <p className="text-xs text-zinc-500 dark:text-zinc-400">{translate("storage.trendHelp", language)}</p></div>
      </div>
      <Tabs label={translate('ui.trendRange', language)} items={['1h', '24h', '7d', 'custom'].map((value, index) => ({ value, label: translate(`storage.${['hour1', 'hour24', 'day7', 'custom'][index]}`, language) }))} value={range} onChange={(next) => { setRange(next); setRangeError(false); if (next !== 'custom') setQuery({ range: next }); }} />
      {range === 'custom' && <form className="custom-range" onSubmit={(event) => {
        event.preventDefault(); const from = new Date(customFrom).getTime(); const to = new Date(customTo).getTime();
        if (!Number.isFinite(from) || !Number.isFinite(to) || from >= to || from < 0) { setRangeError(true); return; }
        setRangeError(false); setQuery({ range: 'custom', from_ms: String(from), to_ms: String(to) });
      }}><label className="field">{translate('storage.from', language)}<input type="datetime-local" value={customFrom} onChange={(event) => setCustomFrom(event.target.value)} required /></label><label className="field">{translate('storage.to', language)}<input type="datetime-local" value={customTo} onChange={(event) => setCustomTo(event.target.value)} required /></label><button className="button secondary" type="submit">{translate('ui.filter', language)}</button></form>}
      {rangeError && <p role="alert">{translate('storage.rangeInvalid', language)}</p>}
      <div className="mt-4">{statsFailed && <p role="status" className="text-sm text-zinc-500 dark:text-zinc-400">{translate("app.statsUnavailable", language)}</p>}
        <TrendChart samples={stats?.samples ?? []} hours={hours} end={query.range === 'custom' ? Number(query.to_ms) : undefined} language={language} /></div>
      {stats?.samples.some((sample) => sample.gap) && <p className="small muted">{translate('storage.gapCount', language, { count: stats.samples.filter((sample) => sample.gap).length })}</p>}
      <div className="mt-3 flex flex-wrap gap-4 text-xs text-zinc-600 dark:text-zinc-400">
        {(["legendRequests", "legendCache", "legendBlocked"] as const).map((key) => <span key={key}>{translate(`ui.${key}`, language)}</span>)}
      </div>
    </section>

    <div className="grid gap-4 lg:grid-cols-2">
      <section className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900" aria-labelledby="parins-response-title">
        <h2 id="parins-response-title" className="font-semibold">{translate("ui.responses", language)}</h2>
        <p className="mb-2 text-xs text-zinc-500 dark:text-zinc-400">{translate("ui.responsesHelp", language)}</p>
        <Distribution rows={responses} language={language} />
      </section>
      <section className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900" aria-labelledby="parins-latency-title">
        <h2 id="parins-latency-title" className="font-semibold">{translate("ui.latencyDistribution", language)}</h2>
        <p className="mb-2 text-xs text-zinc-500 dark:text-zinc-400">{translate("ui.latencyDistributionHelp", language)}</p>
        <Distribution rows={distribution} language={language} />
      </section>
    </div>

    {status?.diagnostics.cache && <CacheDiagnosticsPanel value={status.diagnostics.cache} language={language} />}
    {status && <TransportDiagnostics upstreams={status.diagnostics.upstreams} quic={status.diagnostics.quic} language={language} />}
    <section className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900" aria-labelledby="parins-service-title">
      <div className="flex items-center justify-between gap-3"><h2 id="parins-service-title" className="font-semibold">{translate("ui.maintenance", language)}</h2>
        <button type="button" onClick={onOpenDns} className="min-h-11 text-sm underline underline-offset-4 focus-visible:outline-2">{translate("ui.openDns", language)}</button></div>
      <dl className="mt-3 grid gap-x-6 gap-y-3 text-sm sm:grid-cols-2">
        <div><dt className="text-zinc-500 dark:text-zinc-400">{translate("ui.dnsAddress", language)}</dt><dd className="mt-1 font-mono break-all">{status?.listen ?? translate("app.notConfigured", language)}</dd></div>
        <div><dt className="text-zinc-500 dark:text-zinc-400">{translate("ui.uptime", language)}</dt><dd className="mt-1 tabular-nums">{running ? uptime(status?.uptime_seconds ?? null, language) : "—"}</dd></div>
        <div><dt className="text-zinc-500 dark:text-zinc-400">{translate("ui.inflight", language)}</dt><dd className="mt-1 tabular-nums">{running ? value(status?.metrics?.request_inflight, language) : "—"}</dd></div>
        <div><dt className="text-zinc-500 dark:text-zinc-400">{translate("ui.failures", language)}</dt><dd className="mt-1 tabular-nums">{value(counters?.upstream_failures, language)}</dd></div>
      </dl>
    </section>
  </div>;
}
