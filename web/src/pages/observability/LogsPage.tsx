import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type FocusEvent, type FormEvent, type KeyboardEvent, type MouseEvent, type PointerEvent, type ReactNode } from "react";
import { useReducedMotion } from "motion/react";
import { ApiError, StaleRequest, type ApiClient } from "../../session/client";
import { formatDate, formatNumber, hasTranslation, translate, type Language } from "../../i18n";
import { pathLabel, statusLabel, type CacheFilter, type LogEntry, type LogFilter, type LogListResponse } from "./logs";
import { LiveLogClient, type LiveInterval, type LiveLogState, type LiveStatus, type LiveSubscription } from "./live";
import "./observability.css";
import { Drawer, Tabs } from '../../components/beui';
import { isUnknownStorageMutation } from './storage';
import { decisionLabel, diagnosticLabel } from './diagnostics';
import { UpstreamAttempts } from './TransportDiagnostics';
import { CloudOff, Inbox, RefreshCw, Search, Settings2 } from 'lucide-react';
import { Select } from '../../components/Popover';
import { Segmented } from '../../components/Segmented';

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

/** A plain-language line beside the raw cache scope code, which stays verbatim. */
function scopeNote(scope: string, language: Language): string {
  if (scope === 'no_ecs') return translate('reliability.scopeNoEcs', language);
  if (scope === 'privacy_v4') return translate('reliability.scopePrivacyV4', language);
  if (scope === 'privacy_v6') return translate('reliability.scopePrivacyV6', language);
  if (scope.startsWith('exact_ecs:')) return translate('reliability.scopeExact', language, { network: scope.slice('exact_ecs:'.length) });
  return translate('reliability.scopeNetwork', language, { network: scope });
}

function DetailRow({ label, children }: { label: string; children: ReactNode }) {
  return <div className="log-detail-row">
    <dt>{label}</dt><dd>{children}</dd>
  </div>;
}

function LogDetail({ entry, language }: { entry: LogEntry; language: Language }) {
  const id = useId();
  const none = translate("views.none", language);
  const answer = entry.answer.map((record) => `${record.name} ${record.ttl} ${record.record_type} ${record.data}`).join("\n");
  return <div className="log-detail">
    <section aria-labelledby={`${id}-summary`}><h3 id={`${id}-summary`}>{translate("views.query", language)}</h3>
      <dl>
        <DetailRow label={translate("views.recordId", language)}>{formatNumber(entry.id, language)}</DetailRow>
        <DetailRow label={translate("views.time", language)}>{formatDate(entry.time_ms, language, { dateStyle: "medium", timeStyle: "medium" })}</DetailRow>
        <DetailRow label={translate("views.name", language)}>{entry.name || translate("views.unreadableQuery", language)}</DetailRow>
        <DetailRow label={translate("views.detailQtype", language)}>{entry.qtype || "—"}</DetailRow>
        <DetailRow label={translate("views.client", language)}>{entry.client}</DetailRow>
        <DetailRow label={translate("views.transport", language)}>{entry.transport}</DetailRow>
      </dl></section>
    <section aria-labelledby={`${id}-process`}><h3 id={`${id}-process`}>{translate("views.path", language)}</h3>
      <dl>
        <DetailRow label={translate("ui.result", language)}>{translate(statusLabel(entry.status), language)}</DetailRow>
        <DetailRow label={translate("views.path", language)}>{translate(pathLabel(entry.cache), language)}</DetailRow>
        <DetailRow label={translate('reliability.lookup', language)}>{decisionLabel('lookup', entry.cache_lookup, language)}</DetailRow>
        <DetailRow label={translate('reliability.store', language)}>{decisionLabel('store', entry.cache_store, language)}</DetailRow>
        <DetailRow label={translate('reliability.cacheScope', language)}>{entry.cache_scope ? <><span className="mono">{entry.cache_scope}</span><p className="muted small">{scopeNote(entry.cache_scope, language)}</p></> : none}</DetailRow>
        <DetailRow label={translate("views.upstream", language)}>{entry.upstream || none}</DetailRow>
        <DetailRow label={translate('reliability.relation', language)}>{entry.upstream_relation ? diagnosticLabel(`relation_${entry.upstream_relation}`, language) : none}</DetailRow>
        {entry.failure_stage && <DetailRow label={translate('reliability.operationFailure', language)}>{diagnosticLabel(`stage_${entry.failure_stage}`, language)}{entry.failure_reason && ` · ${diagnosticLabel(`upstream_reason_${entry.failure_reason}`, language)}`}</DetailRow>}
        <DetailRow label={translate("views.rcode", language)}>{entry.rcode || translate("views.noResponse", language)}</DetailRow>
        <DetailRow label={translate("views.duration", language)}>{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</DetailRow>
      </dl></section>
    {entry.upstream_attempts && <UpstreamAttempts trace={{ attempts: entry.upstream_attempts, omitted: entry.upstream_attempts_omitted ?? 0 }} language={language} />}
    <section aria-labelledby={`${id}-dns`}><h3 id={`${id}-dns`}>{translate("views.flags", language)}</h3>
      <dl>
        <DetailRow label={translate("views.incomingEcs", language)}>{entry.incoming_ecs || none}</DetailRow>
        <DetailRow label={translate("views.outgoingEcs", language)}>{entry.outgoing_ecs || none}</DetailRow>
        <DetailRow label={translate("views.flags", language)}><span className="mono small">EDNS={Number(entry.edns)} · DO={Number(entry.dnssec_ok)} · CD={Number(entry.checking_disabled)} · RD={Number(entry.recursion_desired)}</span></DetailRow>
      </dl></section>
    <section aria-labelledby={`${id}-answers`}><h3 id={`${id}-answers`}>{translate("views.answers", language)}</h3>
      <pre className="log-answers">{answer || translate("views.noAnswers", language)}</pre>
      {entry.answer_truncated && <p className="muted small">{translate("views.answerTruncated", language)}</p>}
    </section>
  </div>;
}

const PAGE_SIZE = 50;
/** Live insertion keeps the head list bounded; older rows stay reachable through the cursor. */
const MAX_ROWS = 200;
const HOVER_DELAY_MS = 150;
const intervals = ['manual', '1', '3', '5', '10'] as const;

interface Pending { entries: LogEntry[]; count: number; overflow: boolean; more: boolean }
const noPending: Pending = { entries: [], count: 0, overflow: false, more: false };
interface Expanded { id: number; source: 'hover' | 'focus' | 'tap' }

const isFiltered = (filter: LogFilter) => filter.search !== '' || filter.status !== null || filter.cache !== null;
const liveFilter = (filter: LogFilter) => ({ search: filter.search || null, status: filter.status, cache: filter.cache });
const newest = (entries: readonly LogEntry[]) => entries.reduce((max, entry) => Math.max(max, entry.id), 0);

export function LogsPage({ api, language, onOpenSettings }: LogsPageProps) {
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [applied, setApplied] = useState<LogFilter>({ search: "", status: null, cache: null });
  const appliedRef = useRef<LogFilter>(applied);
  const [result, setResult] = useState<LogListResponse | null>(null);
  const [atHead, setAtHead] = useState(true);
  const [updated, setUpdated] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);
  const [notice, setNotice] = useState("");
  const [confirmation, setConfirmation] = useState<{ revision: number; epoch: number } | null>(null);
  const [unknownEpoch, setUnknownEpoch] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);
  const requestId = useRef(0);
  // Live updates: page memory only, never persisted.
  const [interval, setLiveInterval] = useState<LiveInterval>(3);
  const [hidden, setHidden] = useState(() => document.visibilityState === "hidden");
  const [suspended, setSuspended] = useState(false);
  const [liveStatus, setLiveStatus] = useState<LiveStatus>('stopped');
  const [liveStopped, setLiveStopped] = useState(false);
  const [liveLog, setLiveLog] = useState<LiveLogState | null>(null);
  const [pending, setPending] = useState<Pending>(noPending);
  const [enterAfter, setEnterAfter] = useState(Infinity);
  const [expanded, setExpanded] = useState<Expanded | null>(null);
  const client = useRef<LiveLogClient | null>(null);
  const refreshRequested = useRef(false);
  /** Newest id this page has seen at the head, shown or pending. */
  const known = useRef(0);
  const epoch = useRef<number | null>(null);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The last pointer press, so focus caused by a click or tap is not mistaken for keyboard focus. */
  const pointer = useRef<{ type: string; at: number }>({ type: '', at: -Infinity });
  const rows = useRef(new Map<string, HTMLElement>());
  const anchor = useRef<{ element: HTMLElement; top: number } | null>(null);
  const listTop = useRef<HTMLDivElement>(null);
  const reducedMotion = useReducedMotion();

  const page = result?.page ?? null;
  const entries = page?.entries ?? [];
  const expandedId = expanded && entries.some((entry) => entry.id === expanded.id) ? expanded.id : null;
  const paused = expandedId !== null || !atHead || isFiltered(applied);
  const view = useRef({ atHead, paused, pending, interval, top: 0 });
  view.current = { atHead, paused, pending, interval, top: newest(entries) };

  const subscription = useCallback((): LiveSubscription => ({
    interval: view.current.interval, cursor: known.current || null, filter: liveFilter(appliedRef.current),
  }), []);

  const load = useCallback(async (before: number | null, filter: LogFilter) => {
    const owner = ++requestId.current;
    setLoading(true);
    setFailure(null);
    try {
      const next = await api.request<LogListResponse>("query-log/list", "POST", { ...filter, before_id: before, limit: PAGE_SIZE });
      if (owner === requestId.current) {
        setResult(next); setUpdated(Date.now()); setAtHead(before === null); setEnterAfter(Infinity); setLiveLog(null);
        epoch.current = next.page.log_epoch;
        if (before === null) {
          known.current = newest(next.page.entries);
          setPending(noPending);
          client.current?.update(subscription());
        }
      }
    } catch (error) {
      if (owner === requestId.current && !(error instanceof StaleRequest)) setFailure({ error });
    } finally {
      if (owner === requestId.current) setLoading(false);
    }
  }, [api, subscription]);

  /** Prepends entries newer than the shown head, as one animated batch. */
  const insert = useCallback((incoming: readonly LogEntry[]) => {
    setEnterAfter(view.current.top);
    setResult((previous) => {
      if (!previous) return previous;
      const top = newest(previous.page.entries);
      const fresh = incoming.filter((entry) => entry.id > top).sort((a, b) => b.id - a.id);
      if (!fresh.length) return previous;
      let list = [...fresh, ...previous.page.entries];
      let cursor = previous.page.next_cursor;
      if (list.length > MAX_ROWS) { list = list.slice(0, MAX_ROWS); cursor = list[list.length - 1].id; }
      return { ...previous, page: { ...previous.page, entries: list, total: previous.page.total + fresh.length, next_cursor: cursor } };
    });
    setPending(noPending);
    setUpdated(Date.now());
  }, []);

  const showPending = useCallback(() => {
    const { atHead: head, pending: waiting } = view.current;
    if (head && !waiting.overflow) insert(waiting.entries);
    else void load(null, appliedRef.current);
  }, [insert, load]);

  // One live client per page; handlers read the latest render through refs.
  useEffect(() => {
    const live = new LiveLogClient(() => api.currentBinding, {
      entries(incoming, more) {
        const forced = refreshRequested.current;
        refreshRequested.current = false;
        const fresh = incoming.filter((entry) => entry.id > known.current).sort((a, b) => b.id - a.id);
        if (!fresh.length && !more) return;
        known.current = Math.max(known.current, newest(fresh));
        const { atHead: head, paused: hold, pending: waiting } = view.current;
        if (head && (forced || !hold)) {
          // A capped batch leaves a hole below it; reread the head instead of inserting around it.
          if (more || waiting.overflow) void load(null, appliedRef.current);
          else insert([...fresh, ...waiting.entries]);
          return;
        }
        setPending((current) => {
          const count = current.count + fresh.length;
          return { entries: [...fresh, ...current.entries].slice(0, PAGE_SIZE), count, overflow: current.overflow || more || count > PAGE_SIZE, more: current.more || more };
        });
      },
      state(state) {
        setLiveLog(state);
        if (epoch.current !== null && state.log_epoch !== epoch.current) {
          // The log was cleared elsewhere: drop everything shown and start from the new head.
          epoch.current = state.log_epoch;
          known.current = 0;
          setExpanded(null);
          setPending(noPending);
          void load(null, appliedRef.current);
        }
      },
      status(status) {
        setLiveStatus(status);
        if (status === 'unauthorized') {
          setSuspended(true); setLiveStopped(true);
          // An ordinary request lets the session layer classify the failure like any other 401.
          void load(null, appliedRef.current);
        }
        if (status === 'open') setLiveStopped(false);
      },
    }, subscription());
    client.current = live;
    return () => { client.current = null; live.stop(); };
  }, [api, insert, load, subscription]);

  const ready = result !== null;
  useEffect(() => {
    const live = client.current;
    if (!live) return;
    if (!ready || hidden || suspended) live.stop();
    else live.start(subscription());
  }, [ready, hidden, suspended, interval, subscription]);

  const loaded = useRef(false);
  loaded.current = ready;
  useEffect(() => {
    if (document.visibilityState !== "hidden") void load(null, appliedRef.current);
    const onVisible = () => {
      const isHidden = document.visibilityState === "hidden";
      setHidden(isHidden);
      // Later syncs come from the live connection, which catches up as soon as it reconnects.
      if (!isHidden && !loaded.current) void load(null, appliedRef.current);
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => { requestId.current += 1; document.removeEventListener("visibilitychange", onVisible); };
  }, [load]);
  useEffect(() => {
    if (unknownEpoch !== null && result && result.page.log_epoch > unknownEpoch) {
      setUnknownEpoch(null); setNotice('app.logsCleared');
    }
  }, [unknownEpoch, result]);
  // Insertion resumes when the hovered row closes.
  useEffect(() => {
    if (expandedId === null && atHead && !isFiltered(applied) && pending.count) showPending();
  }, [expandedId, atHead, applied, pending.count, showPending]);
  useEffect(() => () => { if (hoverTimer.current) clearTimeout(hoverTimer.current); }, []);
  // Opening or closing a row above keeps the row being opened where it was on screen.
  useLayoutEffect(() => {
    const held = anchor.current;
    anchor.current = null;
    if (!held) return;
    const delta = held.element.getBoundingClientRect().top - held.top;
    if (Math.abs(delta) >= 1) window.scrollBy(0, delta);
  }, [expanded]);

  function rowFor(id: number): HTMLElement | undefined {
    const table = rows.current.get(`row-${id}`);
    const card = rows.current.get(`card-${id}`);
    return table && table.getBoundingClientRect().height > 0 ? table : card?.getBoundingClientRect().height ? card : table ?? card;
  }
  function expand(next: Expanded | null) {
    if (hoverTimer.current) { clearTimeout(hoverTimer.current); hoverTimer.current = null; }
    setExpanded((current) => {
      if (current?.id === next?.id && current?.source === next?.source) return current;
      const element = next ? rowFor(next.id) : undefined;
      anchor.current = element ? { element, top: element.getBoundingClientRect().top } : null;
      return next;
    });
  }
  function hover(id: number | null, event: PointerEvent) {
    if (event.pointerType !== 'mouse') return;
    if (hoverTimer.current) clearTimeout(hoverTimer.current);
    hoverTimer.current = setTimeout(() => {
      hoverTimer.current = null;
      if (id !== null) expand({ id, source: 'hover' });
      else setExpanded((current) => current?.source === 'hover' ? null : current);
    }, HOVER_DELAY_MS);
  }
  function rowEvents(id: number) {
    return {
      'data-log-id': id,
      onPointerEnter: (event: PointerEvent) => hover(id, event),
      onPointerLeave: (event: PointerEvent) => hover(null, event),
      onPointerDown: (event: PointerEvent) => { pointer.current = { type: event.pointerType, at: event.timeStamp }; },
      onKeyDown: (event: KeyboardEvent) => {
        if (event.key === 'Escape' && expandedId === id) { event.preventDefault(); expand(null); }
      },
    };
  }
  function controlEvents(id: number) {
    const open = expandedId === id;
    return {
      onFocus: (event: FocusEvent) => { if (event.timeStamp - pointer.current.at > 1000) expand({ id, source: 'focus' }); },
      onBlur: (event: FocusEvent) => {
        const next = event.relatedTarget instanceof Element ? event.relatedTarget.closest('[data-log-id]') : null;
        if (expanded?.id === id && expanded.source === 'focus' && next?.getAttribute('data-log-id') !== String(id)) expand(null);
      },
      onClick: (event: MouseEvent) => {
        const mouse = event.detail > 0 && pointer.current.type === 'mouse';
        pointer.current = { type: '', at: pointer.current.at };
        // A mouse click opens at once; a tap, Enter or Space toggles.
        if (mouse) expand({ id, source: 'hover' });
        else expand(open ? null : { id, source: 'tap' });
      },
    };
  }

  function submitFilter(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const filter = { search: search.trim(), status: statusFilter || null, cache: applied.cache };
    appliedRef.current = filter;
    setApplied(filter);
    setNotice("");
    void load(null, filter);
  }

  function refreshNow() {
    if (atHead && client.current?.refresh()) { refreshRequested.current = true; return; }
    void load(null, appliedRef.current);
  }

  function chooseInterval(value: string) {
    setLiveInterval(value === 'manual' ? null : Number(value) as LiveInterval);
    setSuspended(false);
    setLiveStopped(false);
  }

  function revealPending() {
    showPending();
    listTop.current?.scrollIntoView({ block: 'start', behavior: reducedMotion ? 'auto' : 'smooth' });
  }

  async function clearLogs() {
    if (confirmation === null || unknownEpoch !== null) return;
    const { revision, epoch: listed } = confirmation;
    setConfirmation(null);
    setClearing(true);
    setNotice("");
    try {
      await api.request("query-log/clear", "POST", { revision, log_epoch: listed });
      setExpanded(null);
      setNotice("app.logsCleared");
      await load(null, applied);
    } catch (error) {
      if (!(error instanceof StaleRequest)) {
        const unknown = isUnknownStorageMutation(error);
        if (unknown) setUnknownEpoch(listed);
        setFailure(unknown ? { key: "app.clearLogsUnknown" } : { error });
      }
    } finally { setClearing(false); }
  }

  const statuses = ["", "success", "blocked", "error", "dropped"] as const;
  const queryName = (entry: LogEntry) => entry.name || translate("views.unreadableQuery", language);
  const liveMessage = liveStopped ? 'views.liveStopped'
    : liveStatus === 'reconnecting' ? 'views.liveReconnecting'
      : liveLog?.log === 'unavailable' ? 'views.liveUnavailable' : liveLog?.log === 'checking' ? 'views.liveChecking' : null;
  const disabled = (page && !page.enabled) || liveLog?.log === 'disabled';
  const detailId = (scope: string, id: number) => `log-expansion-${scope}-${id}`;
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
        appliedRef.current = filter; setApplied(filter); setNotice(''); setExpanded(null); void load(null, filter);
      }} />
    </div>
    {disabled && <p className="notice">{translate('app.logsDisabled', language)}</p>}

    <div className="log-toolbar">
      <p role="status" className="muted small">{loading ? translate(page ? 'storage.filterPending' : "app.reading", language)
        : page && updated !== null
          ? translate("app.logSummary", language, { count: formatNumber(entries.length, language), total: formatNumber(page.total, language), time: formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })
          : translate("ui.logsUnread", language)}</p>
      <div className="button-group log-live-controls">
        <Segmented className="log-interval" label={translate('views.liveRefresh', language)} value={interval === null ? 'manual' : String(interval)} onChange={chooseInterval}
          options={intervals.map((value) => ({ value, label: value === 'manual' ? translate('views.liveManual', language) : translate('views.liveSeconds', language, { seconds: value }) }))} />
        <button type="button" onClick={refreshNow} disabled={loading} className="button secondary"><RefreshCw size={15} aria-hidden="true" className={loading ? 'spin' : undefined} />{translate("ui.refreshLatest", language)}</button>
        <button type="button" onClick={() => result && setConfirmation({ revision: result.revision, epoch: result.page.log_epoch })} disabled={!page?.total || clearing || unknownEpoch !== null}
          className="button danger">{translate("ui.clearLogs", language)}</button>
      </div>
    </div>
    {liveMessage && <p role="status" className="muted small log-live-note">{translate(liveMessage, language)}</p>}
    {failure && <p role="alert" className="notice error">{localizedError(failure, language)}</p>}
    {notice && <p role="status" className="notice">{translate(notice, language)}</p>}
    {unknownEpoch !== null && <button type="button" className="button secondary" disabled={loading} onClick={() => void load(null, applied)}>{translate('storage.checkResult', language)}</button>}

    <div ref={listTop} className="log-list-top" />
    {pending.count > 0 && <div className="log-new-wrap"><button type="button" className="log-new-pill" onClick={revealPending}>
      {translate('views.newEntries', language, { count: `${formatNumber(pending.count, language)}${pending.more ? '+' : ''}` })}</button></div>}

    {page === null ? (failure
      ? <div className="panel empty-state" data-tone="bad"><CloudOff size={28} aria-hidden="true" /><p>{translate("app.loadFailed", language)}</p></div>
      : <div className="panel skeleton-stack" aria-hidden="true">{[0, 1, 2, 3, 4].map((index) => <div key={index} className="skeleton" style={{ height: 22 }} />)}</div>)
      : entries.length === 0 ? <div className="panel empty-state"><Inbox size={28} aria-hidden="true" /><p>{translate("views.logsEmpty", language)}</p></div>
        : <>
          <div className="log-cards">{entries.map((entry) => {
            const open = expandedId === entry.id;
            return <article key={entry.id} ref={(node) => { if (node) rows.current.set(`card-${entry.id}`, node); else rows.current.delete(`card-${entry.id}`); }}
              className="panel log-card" data-open={open || undefined} data-enter={entry.id > enterAfter || undefined} {...rowEvents(entry.id)}>
              <div className="log-card-head"><button type="button" className="log-expand" aria-expanded={open} aria-controls={open ? detailId('card', entry.id) : undefined} {...controlEvents(entry.id)}>
                <span className="mono log-name">{queryName(entry)}</span><span className="muted small">{entry.qtype || "—"} · {formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "short" })}</span></button></div>
              <p className="muted small log-meta"><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span> {entry.client} · {translate(pathLabel(entry.cache), language)} · {formatNumber(entry.duration_ms, language, { maximumFractionDigits: 2 })} ms</p>
              {open && <div id={detailId('card', entry.id)} className="log-expansion"><LogDetail entry={entry} language={language} /></div>}
            </article>;
          })}</div>
          <div tabIndex={0} role="region" aria-label={translate("views.logsRegion", language)} className="panel log-table table-scroll">
            <table className="data-table"><caption className="sr-only">{translate("views.logsCaption", language)}</caption>
              <thead><tr>
                {(["time", "query", "clientTransport", "resultPath", "duration"] as const).map((key) => <th key={key} scope="col">{translate(`views.${key}`, language)}</th>)}
              </tr></thead><tbody>{entries.map((entry) => {
                const open = expandedId === entry.id;
                const events = rowEvents(entry.id);
                return [<tr key={entry.id} ref={(node) => { if (node) rows.current.set(`row-${entry.id}`, node); else rows.current.delete(`row-${entry.id}`); }}
                  className="log-row" data-open={open || undefined} data-enter={entry.id > enterAfter || undefined} {...events}>
                  <td className="num nowrap"><div className="log-cell">{formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "medium" })}</div></td>
                  <td><div className="log-cell"><button type="button" className="log-expand mono log-name" aria-expanded={open} aria-controls={open ? detailId('row', entry.id) : undefined} {...controlEvents(entry.id)}>{queryName(entry)}</button><span className="muted small">{entry.qtype || "—"}</span></div></td>
                  <td><div className="log-cell"><span className="mono">{entry.client}</span><span className="muted small">{entry.transport}</span></div></td>
                  <td><div className="log-cell"><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span><span className="muted small">{translate(pathLabel(entry.cache), language)}</span></div></td>
                  <td className="num nowrap"><div className="log-cell">{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</div></td>
                </tr>, open && <tr key={`${entry.id}-detail`} className="log-expansion-row" {...events}>
                  <td colSpan={5}><div id={detailId('row', entry.id)} className="log-expansion"><LogDetail entry={entry} language={language} /></div></td>
                </tr>];
              })}</tbody></table>
          </div>
        </>}

    {page?.next_cursor && <button type="button" onClick={() => void load(page.next_cursor, applied)} disabled={loading}
      className="button secondary load-more">{translate("ui.earlier", language)}</button>}

    <Drawer open={confirmation !== null} title={translate('app.clearLogsTitle', language)} closeLabel={translate('ui.closeDialog', language)} onClose={() => setConfirmation(null)}>
      <p id="log-clear-help" className="muted">{translate("app.clearLogsHelp", language)}</p>
      <div className="button-group confirm-actions">
        <button type="button" onClick={() => setConfirmation(null)} className="button secondary">{translate("ui.cancel", language)}</button>
        <button type="button" onClick={() => void clearLogs()} className="button danger">{translate("app.clearLogs", language)}</button>
      </div>
    </Drawer>
  </div>;
}
