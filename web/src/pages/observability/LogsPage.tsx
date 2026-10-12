import { useCallback, useEffect, useRef, useState, type FormEvent, type MouseEvent } from "react";
import { ApiError, StaleRequest, type ApiClient } from "../../session/client";
import { formatDate, formatNumber, hasTranslation, translate, type Language } from "../../i18n";
import { pathLabel, statusLabel, type CacheFilter, type LogEntry, type LogFilter, type LogListResponse } from "./logs";
import { Drawer, Tabs } from '../../components/beui';
import { isUnknownStorageMutation } from './storage';
import { decisionLabel, diagnosticLabel } from './diagnostics';
import { UpstreamAttempts } from './TransportDiagnostics';
import { CloudOff, Inbox, RefreshCw, Search, Settings2 } from 'lucide-react';
import { Select } from '../../components/Popover';

export interface LogsPageProps {
  api: ApiClient;
  language: Language;
  onOpenSettings: () => void;
}

/** Failures keep the error or catalog key and are translated at render time, so a language switch needs no new request. */
type Failure = { error: unknown } | { key: string };

function localizedError(failure: Failure, language: Language): string {
  if ("key" in failure) return translate(failure.key, language);
  const { error } = failure;
  if (error instanceof ApiError) {
    const key = `api.${error.code}`;
    if (hasTranslation(key)) return translate(key, language, { detail: error.message, status: error.status });
  }
  return translate("app.loadFailed", language);
}

function DetailRow({ label, children }: { label: string; children: React.ReactNode }) {
  return <div className="log-detail-row">
    <dt>{label}</dt><dd>{children}</dd>
  </div>;
}

function LogDetail({ entry, language }: { entry: LogEntry; language: Language }) {
  const none = translate("views.none", language);
  const answer = entry.answer.map((record) => `${record.name} ${record.ttl} ${record.record_type} ${record.data}`).join("\n");
  return <div className="log-detail">
    <section aria-labelledby="log-detail-summary"><h3 id="log-detail-summary">{translate("views.query", language)}</h3>
      <dl>
        <DetailRow label={translate("views.recordId", language)}>{formatNumber(entry.id, language)}</DetailRow>
        <DetailRow label={translate("views.time", language)}>{formatDate(entry.time_ms, language, { dateStyle: "medium", timeStyle: "medium" })}</DetailRow>
        <DetailRow label={translate("views.name", language)}>{entry.name || translate("views.unreadableQuery", language)}</DetailRow>
        <DetailRow label={translate("views.qtype", language)}>{entry.qtype || "—"}</DetailRow>
        <DetailRow label={translate("views.client", language)}>{entry.client}</DetailRow>
        <DetailRow label={translate("views.transport", language)}>{entry.transport}</DetailRow>
      </dl></section>
    <section aria-labelledby="log-detail-process"><h3 id="log-detail-process">{translate("views.path", language)}</h3>
      <dl>
        <DetailRow label={translate("ui.result", language)}>{translate(statusLabel(entry.status), language)}</DetailRow>
        <DetailRow label={translate("views.path", language)}>{translate(pathLabel(entry.cache), language)}</DetailRow>
        <DetailRow label={translate('reliability.lookup', language)}>{decisionLabel('lookup', entry.cache_lookup, language)}</DetailRow>
        <DetailRow label={translate('reliability.store', language)}>{decisionLabel('store', entry.cache_store, language)}</DetailRow>
        <DetailRow label={translate('reliability.cacheScope', language)}>{entry.cache_scope ?? none}{entry.cache_scope?.startsWith('exact_ecs:') && <p className="muted small">{translate('reliability.exactEcs', language)}</p>}</DetailRow>
        <DetailRow label={translate("views.upstream", language)}>{entry.upstream || none}</DetailRow>
        <DetailRow label={translate('reliability.relation', language)}>{entry.upstream_relation ? diagnosticLabel(`relation_${entry.upstream_relation}`, language) : none}</DetailRow>
        {entry.failure_stage && <DetailRow label={translate('reliability.operationFailure', language)}>{diagnosticLabel(`stage_${entry.failure_stage}`, language)}{entry.failure_reason && ` · ${diagnosticLabel(`upstream_reason_${entry.failure_reason}`, language)}`}</DetailRow>}
        <DetailRow label={translate("views.rcode", language)}>{entry.rcode || translate("views.noResponse", language)}</DetailRow>
        <DetailRow label={translate("views.duration", language)}>{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</DetailRow>
      </dl></section>
    {entry.upstream_attempts && <UpstreamAttempts trace={{ attempts: entry.upstream_attempts, omitted: entry.upstream_attempts_omitted ?? 0 }} language={language} />}
    <section aria-labelledby="log-detail-dns"><h3 id="log-detail-dns">{translate("views.flags", language)}</h3>
      <dl>
        <DetailRow label={translate("views.incomingEcs", language)}>{entry.incoming_ecs || none}</DetailRow>
        <DetailRow label={translate("views.outgoingEcs", language)}>{entry.outgoing_ecs || none}</DetailRow>
        <DetailRow label={translate("views.flags", language)}><span className="mono small">EDNS={Number(entry.edns)} · DO={Number(entry.dnssec_ok)} · CD={Number(entry.checking_disabled)} · RD={Number(entry.recursion_desired)}</span></DetailRow>
      </dl></section>
    <section aria-labelledby="log-detail-answers"><h3 id="log-detail-answers">{translate("views.answers", language)}</h3>
      <pre className="log-answers">{answer || translate("views.noAnswers", language)}</pre>
      {entry.answer_truncated && <p className="muted small">{translate("views.answerTruncated", language)}</p>}
    </section>
  </div>;
}

export function LogsPage({ api, language, onOpenSettings }: LogsPageProps) {
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [applied, setApplied] = useState<LogFilter>({ search: "", status: null, cache: null });
  const appliedRef = useRef<LogFilter>(applied);
  const [result, setResult] = useState<LogListResponse | null>(null);
  const [updated, setUpdated] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);
  const [notice, setNotice] = useState("");
  const [selected, setSelected] = useState<LogEntry | null>(null);
  const [confirmation, setConfirmation] = useState<{ revision: number; epoch: number } | null>(null);
  const [unknownEpoch, setUnknownEpoch] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);
  const requestId = useRef(0);

  const load = useCallback(async (before: number | null, filter: LogFilter) => {
    const owner = ++requestId.current;
    setLoading(true);
    setFailure(null);
    try {
      const next = await api.request<LogListResponse>("query-log/list", "POST", { ...filter, before_id: before, limit: 50 });
      if (owner === requestId.current) {
        setResult(next); setUpdated(Date.now());
      }
    } catch (error) {
      if (owner === requestId.current && !(error instanceof StaleRequest)) setFailure({ error });
    } finally {
      if (owner === requestId.current) setLoading(false);
    }
  }, [api]);

  useEffect(() => {
    if (document.visibilityState !== "hidden") void load(null, appliedRef.current);
    const onVisible = () => { if (document.visibilityState !== "hidden") void load(null, appliedRef.current); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { requestId.current += 1; document.removeEventListener("visibilitychange", onVisible); };
  }, [load]);
  useEffect(() => {
    if (unknownEpoch !== null && result && result.page.log_epoch > unknownEpoch) {
      setUnknownEpoch(null); setNotice('app.logsCleared');
    }
  }, [unknownEpoch, result]);

  function submitFilter(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const filter = { search: search.trim(), status: statusFilter || null, cache: applied.cache };
    appliedRef.current = filter;
    setApplied(filter);
    setNotice("");
    void load(null, filter);
  }

  function openDetail(entry: LogEntry, event: MouseEvent<HTMLButtonElement>) {
    event.currentTarget.focus();
    setSelected(structuredClone(entry));
  }

  async function clearLogs() {
    if (confirmation === null || unknownEpoch !== null) return;
    const { revision, epoch } = confirmation;
    setConfirmation(null);
    setClearing(true);
    setNotice("");
    try {
      await api.request("query-log/clear", "POST", { revision, log_epoch: epoch });
      setSelected(null);
      setNotice("app.logsCleared");
      await load(null, applied);
    } catch (error) {
      if (!(error instanceof StaleRequest)) {
        const unknown = isUnknownStorageMutation(error);
        if (unknown) setUnknownEpoch(epoch);
        setFailure(unknown ? { key: "app.clearLogsUnknown" } : { error });
      }
    } finally { setClearing(false); }
  }

  const page = result?.page ?? null;
  const entries = page?.entries ?? [];
  const statuses = ["", "success", "blocked", "error", "dropped"] as const;
  const label = (entry: LogEntry) => `${translate("views.details", language)} · ${entry.name || translate("views.unreadableQuery", language)} · #${entry.id}`;
  return <div className="page-stack logs-page">
    <header className="page-head log-head large-title">
      <div><h1>{translate("ui.logs", language)}</h1></div>
      <button type="button" onClick={onOpenSettings} className="button secondary"><Settings2 size={16} aria-hidden="true" />{translate("ui.logSettings", language)}</button>
    </header>

    <div className="log-controls">
      <form onSubmit={submitFilter} className="panel log-filter">
        <label className="field"><span>{translate("ui.logSearch", language)}</span>
          <span className="search-input"><Search size={16} aria-hidden="true" /><input value={search} maxLength={256} onChange={(event) => setSearch(event.target.value)} type="search" autoComplete="off" placeholder={translate("ui.logSearchPlaceholder", language)} /></span></label>
        <div className="field"><span>{translate("ui.result", language)}</span>
          <Select label={translate("ui.result", language)} value={statusFilter} onChange={setStatusFilter}
            options={statuses.map((status) => ({ value: status, label: translate(status ? `ui.${status}` : "ui.allResults", language) }))} /></div>
        <button type="submit" className="button primary">{translate("ui.filter", language)}</button>
      </form>
      <Tabs label={translate('storage.cacheFilter', language)} value={applied.cache ?? 'all'} items={['all', 'cached', 'fresh', 'stale', 'non_cached'].map((cache) => ({ value: cache, label: translate(`storage.${cache === 'all' ? 'cacheAll' : cache}`, language) }))} onChange={(cache) => {
        const filter = { search: search.trim(), status: statusFilter || null, cache: cache === 'all' ? null : cache as CacheFilter };
        appliedRef.current = filter; setApplied(filter); setNotice(''); setSelected(null); void load(null, filter);
      }} />
    </div>
    {page && !page.enabled && <p className="notice">{translate('app.logsDisabled', language)}</p>}

    <div className="log-toolbar">
      <p role="status" className="muted small">{loading ? translate(page ? 'storage.filterPending' : "app.reading", language)
        : page && updated !== null
          ? translate("app.logSummary", language, { count: formatNumber(entries.length, language), total: formatNumber(page.total, language), time: formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })
          : translate("ui.logsUnread", language)}</p>
      <div className="button-group">
        <button type="button" onClick={() => void load(null, applied)} disabled={loading} className="button secondary"><RefreshCw size={15} aria-hidden="true" className={loading ? 'spin' : undefined} />{translate("ui.refreshLatest", language)}</button>
        <button type="button" onClick={() => result && setConfirmation({ revision: result.revision, epoch: result.page.log_epoch })} disabled={!page?.total || clearing || unknownEpoch !== null}
          className="button danger">{translate("ui.clearLogs", language)}</button>
      </div>
    </div>
    {failure && <p role="alert" className="notice error">{localizedError(failure, language)}</p>}
    {notice && <p role="status" className="notice">{translate(notice, language)}</p>}
    {unknownEpoch !== null && <button type="button" className="button secondary" disabled={loading} onClick={() => void load(null, applied)}>{translate('storage.checkResult', language)}</button>}

    {page === null ? (failure
      ? <div className="panel empty-state" data-tone="bad"><CloudOff size={28} aria-hidden="true" /><p>{translate("app.loadFailed", language)}</p></div>
      : <div className="panel skeleton-stack" aria-hidden="true">{[0, 1, 2, 3, 4].map((index) => <div key={index} className="skeleton" style={{ height: 22 }} />)}</div>)
      : entries.length === 0 ? <div className="panel empty-state"><Inbox size={28} aria-hidden="true" /><p>{translate("views.logsEmpty", language)}</p></div>
        : <>
          <div className="log-cards">{entries.map((entry) => <article key={entry.id} className="panel log-card">
            <div className="log-card-head"><div><p className="mono log-name">{entry.name || translate("views.unreadableQuery", language)}</p><p className="muted small">{entry.qtype || "—"} · {formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "short" })}</p></div>
              <button type="button" onClick={(event) => openDetail(entry, event)} aria-label={label(entry)} className="button quiet">{translate("views.details", language)}</button></div>
            <p className="muted small log-meta"><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span> {entry.client} · {translate(pathLabel(entry.cache), language)} · {formatNumber(entry.duration_ms, language, { maximumFractionDigits: 2 })} ms</p>
          </article>)}</div>
          <div tabIndex={0} role="region" aria-label={translate("views.logsRegion", language)} className="panel log-table table-scroll">
            <table className="data-table"><caption className="sr-only">{translate("views.logsCaption", language)}</caption>
              <thead><tr>
                {(["time", "query", "clientTransport", "resultPath", "duration", "details"] as const).map((key) => <th key={key} scope="col">{translate(`views.${key}`, language)}</th>)}
              </tr></thead><tbody>{entries.map((entry) => <tr key={entry.id} className="log-row">
                <td className="num nowrap">{formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "medium" })}</td>
                <td><span className="mono log-name">{entry.name || translate("views.unreadableQuery", language)}</span><span className="muted small">{entry.qtype || "—"}</span></td>
                <td><span className="mono">{entry.client}</span><span className="muted small">{entry.transport}</span></td>
                <td><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span><span className="muted small">{translate(pathLabel(entry.cache), language)}</span></td>
                <td className="num nowrap">{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</td>
                <td><button type="button" onClick={(event) => openDetail(entry, event)} aria-label={label(entry)} className="button quiet">{translate("views.details", language)}</button></td>
              </tr>)}</tbody></table>
          </div>
        </>}

    {page?.next_cursor && <button type="button" onClick={() => void load(page.next_cursor, applied)} disabled={loading}
      className="button secondary load-more">{translate("ui.earlier", language)}</button>}

    <Drawer open={selected !== null} title={translate('views.details', language)} closeLabel={translate('ui.closeDetails', language)} onClose={() => setSelected(null)}>
      {selected && <LogDetail entry={selected} language={language} />}
    </Drawer>
    <Drawer open={confirmation !== null} title={translate('app.clearLogsTitle', language)} closeLabel={translate('ui.closeDialog', language)} onClose={() => setConfirmation(null)}>
      <p id="log-clear-help" className="muted">{translate("app.clearLogsHelp", language)}</p>
      <div className="button-group confirm-actions">
        <button type="button" onClick={() => setConfirmation(null)} className="button secondary">{translate("ui.cancel", language)}</button>
        <button type="button" onClick={() => void clearLogs()} className="button danger">{translate("app.clearLogs", language)}</button>
      </div>
    </Drawer>
  </div>;
}
