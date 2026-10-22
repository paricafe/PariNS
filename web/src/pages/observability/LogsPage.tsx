import { useCallback, useEffect, useId, useRef, useState, type FormEvent, type KeyboardEvent, type MouseEvent, type PointerEvent, type ReactNode } from "react";
import { AnimatePresence, useReducedMotion } from "motion/react";
import { StaleRequest, type ApiClient } from "../../session/client";
import { formatDate, formatNumber, presentIssue, translate, type Language } from "../../i18n";
import { toConfigIssue, type ConfigIssue } from "../../config/context";
import { pathLabel, statusLabel, type CacheFilter, type LogEntry, type LogFilter, type LogListResponse } from "./logs";
import { LiveLogClient, type LiveInterval, type LiveLogState, type LiveStatus, type LiveSubscription } from "./live";
import "./observability.css";
import { Drawer, Tabs } from '../../components/beui';
import { isUnknownStorageMutation } from './storage';
import { decisionLabel, diagnosticLabel, scopeNote } from './diagnostics';
import { UpstreamAttempts } from './TransportDiagnostics';
import { CloudOff, Inbox, Info, RefreshCw, Search, Settings2 } from 'lucide-react';
import { Select } from '../../components/Popover';
import { Segmented } from '../../components/Segmented';
import { DetailPopover } from './DetailPopover';
import { InfoTip } from '../../components/InfoTip';
import { useAutoConfirm } from '../../components/useAutoConfirm';

export interface LogsPageProps {
  api: ApiClient;
  language: Language;
  onOpenSettings: () => void;
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
        <DetailRow label={translate('reliability.cacheScope', language)}>{entry.cache_scope ? <>{scopeNote(entry.cache_scope, language)} <InfoTip text={entry.cache_scope} label={translate('reliability.rawScope', language)} /></> : none}</DetailRow>
        <DetailRow label={translate("views.upstream", language)}>{entry.upstream || none}</DetailRow>
        <DetailRow label={translate('reliability.relation', language)}>{entry.upstream_relation ? diagnosticLabel(`relation_${entry.upstream_relation}`, language) : none}</DetailRow>
        {entry.failure_stage && <DetailRow label={translate('reliability.operationFailure', language)}>{diagnosticLabel(`stage_${entry.failure_stage}`, language)}{entry.failure_reason && ` · ${diagnosticLabel(`upstream_reason_${entry.failure_reason}`, language)}`}</DetailRow>}
        <DetailRow label={translate("views.rcode", language)}>{entry.rcode || translate("views.noResponse", language)}</DetailRow>
        <DetailRow label={translate("views.duration", language)}>{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</DetailRow>
      </dl></section>
    {entry.upstream_attempts && <UpstreamAttempts trace={{ attempts: entry.upstream_attempts, omitted: entry.upstream_attempts_omitted ?? 0 }} language={language} />}
    <section aria-labelledby={`${id}-dns`}><h3 id={`${id}-dns`}>{translate("views.ecsAndFlags", language)}</h3>
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
const detailId = (id: number) => `log-detail-${id}`;
const intervals = ['manual', '1', '3', '5', '10'] as const;

interface Pending { entries: LogEntry[]; count: number; overflow: boolean; more: boolean }
const noPending: Pending = { entries: [], count: 0, overflow: false, more: false };
/** The open detail card: hover cards close when the pointer leaves, pinned ones only on request. */
interface Detail { id: number; pinned: boolean; anchor: HTMLButtonElement; keyboard: boolean }

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
  const [failure, setFailure] = useState<ConfigIssue | null>(null);
  const [notice, setNotice] = useState("");
  const [confirmation, setConfirmation] = useState<{ epoch: number } | null>(null);
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
  const [detail, setDetail] = useState<Detail | null>(null);
  const client = useRef<LiveLogClient | null>(null);
  const refreshRequested = useRef(false);
  /** Newest id this page has seen at the head, shown or pending. */
  const known = useRef(0);
  const epoch = useRef<number | null>(null);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const listTop = useRef<HTMLDivElement>(null);
  const reducedMotion = useReducedMotion();

  const page = result?.page ?? null;
  const entries = page?.entries ?? [];
  const detailEntry = detail ? entries.find((entry) => entry.id === detail.id) : undefined;
  const openId = detailEntry && detail?.anchor.isConnected ? detailEntry.id : null;
  const paused = openId !== null || !atHead || isFiltered(applied);
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
      if (owner === requestId.current && !(error instanceof StaleRequest)) setFailure(toConfigIssue(error));
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
          setDetail(null);
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
    // Manual mode holds no connection; Refresh reads the list instead.
    if (!ready || hidden || suspended || interval === null) live.stop();
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
      setUnknownEpoch(null); setFailure(null); setNotice('app.logsCleared');
    }
  }, [unknownEpoch, result]);
  // A lost clear response is settled by reading the log epoch; the clear is never sent again.
  const clearCheck = useAutoConfirm(unknownEpoch !== null, async () => {
    const next = await api.request<LogListResponse>("query-log/list", "POST", { ...appliedRef.current, before_id: null, limit: PAGE_SIZE });
    return unknownEpoch !== null && next.page.log_epoch > unknownEpoch ? true : undefined;
  }, () => { void load(null, appliedRef.current); });
  // Insertion resumes when the detail card closes.
  useEffect(() => {
    if (openId === null && atHead && !isFiltered(applied) && pending.count) showPending();
  }, [openId, atHead, applied, pending.count, showPending]);
  useEffect(() => () => { if (hoverTimer.current) clearTimeout(hoverTimer.current); }, []);
  // A card whose record left the list (cap, filter, clear) closes instead of reopening later.
  useEffect(() => { if (detail && openId === null) setDetail(null); }, [detail, openId]);
  useEffect(() => {
    if (!detail) return;
    const anchor = detail.anchor;
    const owned = (node: Node | null) => !!node && (anchor.contains(node) || !!document.getElementById(detailId(detail.id))?.contains(node));
    const down = (event: globalThis.PointerEvent) => {
      if (owned(event.target as Node)) return;
      closeDetail(owned(document.activeElement) && !anchor.contains(document.activeElement));
    };
    // Escape returns focus only when the card or its button has it; elsewhere it just closes the card.
    const key = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      if (owned(document.activeElement)) { event.preventDefault(); closeDetail(true); } else closeDetail(false);
    };
    document.addEventListener('pointerdown', down);
    document.addEventListener('keydown', key);
    return () => { document.removeEventListener('pointerdown', down); document.removeEventListener('keydown', key); };
  }, [detail]);

  function cancelTimer() {
    if (hoverTimer.current) { clearTimeout(hoverTimer.current); hoverTimer.current = null; }
  }
  function closeDetail(refocus: boolean) {
    cancelTimer();
    if (refocus) detail?.anchor.focus();
    setDetail(null);
  }
  function hoverClose(event: PointerEvent) {
    if (event.pointerType !== 'mouse') return;
    cancelTimer();
    hoverTimer.current = setTimeout(() => {
      hoverTimer.current = null;
      setDetail((current) => current?.pinned ? current : null);
    }, HOVER_DELAY_MS);
  }
  function detailEvents(id: number) {
    return {
      onPointerEnter: (event: PointerEvent<HTMLButtonElement>) => {
        if (event.pointerType !== 'mouse') return;
        cancelTimer();
        const anchor = event.currentTarget;
        hoverTimer.current = setTimeout(() => {
          hoverTimer.current = null;
          // Hover never replaces a pinned card.
          setDetail((current) => current?.pinned || current?.id === id ? current : { id, pinned: false, anchor, keyboard: false });
        }, HOVER_DELAY_MS);
      },
      onPointerLeave: hoverClose,
      // Click, tap, Enter and Space pin the card; pressing a pinned card's button closes it.
      onClick: (event: MouseEvent<HTMLButtonElement>) => {
        cancelTimer();
        const anchor = event.currentTarget;
        if (detail?.id === id && detail.pinned) { setDetail(null); return; }
        setDetail({ id, pinned: true, anchor, keyboard: event.detail === 0 });
      },
    };
  }
  /** Pressing inside a hover card pins it, so selecting text never closes it. */
  function cardKey(event: KeyboardEvent<HTMLElement>) {
    // The card lives in a body portal; Tab returns to its button instead of the end of the page.
    if (event.key === 'Tab') { event.preventDefault(); closeDetail(true); }
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
    const { epoch: listed } = confirmation;
    setConfirmation(null);
    setClearing(true);
    setNotice("");
    try {
      await api.request("query-log/clear", "POST", { log_epoch: listed });
      setDetail(null);
      setNotice("app.logsCleared");
      await load(null, applied);
    } catch (error) {
      if (!(error instanceof StaleRequest)) {
        const unknown = isUnknownStorageMutation(error);
        if (unknown) setUnknownEpoch(listed);
        else setFailure(toConfigIssue(error));
      }
    } finally { setClearing(false); }
  }

  const statuses = ["", "success", "blocked", "error", "dropped"] as const;
  const queryName = (entry: LogEntry) => entry.name || translate("views.unreadableQuery", language);
  const liveMessage = liveStopped ? 'views.liveStopped'
    : liveStatus === 'reconnecting' ? 'views.liveReconnecting'
      : liveLog?.log === 'unavailable' ? 'views.liveUnavailable' : liveLog?.log === 'checking' ? 'views.liveChecking' : null;
  const disabled = (page && !page.enabled) || liveLog?.log === 'disabled';
  const detailButton = (id: number) => <button type="button" className="icon-button log-detail-button" aria-label={translate("views.queryDetails", language)}
    aria-haspopup="dialog" aria-expanded={openId === id} aria-controls={openId === id ? detailId(id) : undefined} {...detailEvents(id)}>
    <Info size={17} aria-hidden="true" /></button>;
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
        appliedRef.current = filter; setApplied(filter); setNotice(''); setDetail(null); void load(null, filter);
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
        <button type="button" onClick={() => result && setConfirmation({ epoch: result.page.log_epoch })} disabled={!page?.total || clearing || unknownEpoch !== null}
          className="button danger">{translate("ui.clearLogs", language)}</button>
      </div>
    </div>
    {liveMessage && <p role="status" className="muted small log-live-note">{translate(liveMessage, language)}</p>}
    {failure && <p role="alert" className="notice error">{presentIssue(failure, language)}</p>}
    {notice && <p role="status" className="notice">{translate(notice, language)}</p>}
    {unknownEpoch !== null && <p role="status" className="notice">{translate(clearCheck.state === 'failed' ? 'app.clearLogsUnknown' : clearCheck.state === 'unresolved' ? 'storage.stillUnknown' : 'shell.confirming', language)}</p>}
    {unknownEpoch !== null && (clearCheck.state === 'failed' || clearCheck.state === 'unresolved') && <button type="button" className="button secondary" disabled={loading} onClick={clearCheck.check}>{translate('storage.checkResult', language)}</button>}

    <div ref={listTop} className="log-list-top" />
    {pending.count > 0 && <div className="log-new-wrap"><button type="button" className="log-new-pill" onClick={revealPending}>
      {translate('views.newEntries', language, { count: `${formatNumber(pending.count, language)}${pending.more ? '+' : ''}` })}</button></div>}

    {page === null ? (failure
      ? <div className="panel empty-state" data-tone="bad"><CloudOff size={28} aria-hidden="true" /><p>{translate("app.loadFailed", language)}</p></div>
      : <div className="panel skeleton-stack" aria-hidden="true">{[0, 1, 2, 3, 4].map((index) => <div key={index} className="skeleton" style={{ height: 22 }} />)}</div>)
      : entries.length === 0 ? <div className="panel empty-state"><Inbox size={28} aria-hidden="true" /><p>{translate("views.logsEmpty", language)}</p></div>
        : <>
          <div className="log-cards">{entries.map((entry) => <article key={entry.id} className="panel log-card" data-enter={entry.id > enterAfter || undefined}>
            <div className="log-card-head"><div><span className="mono log-name">{queryName(entry)}</span><span className="muted small">{entry.qtype || "—"} · {formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "short" })}</span></div>
              {detailButton(entry.id)}</div>
            <p className="muted small log-meta"><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span> {entry.client} · {translate(pathLabel(entry.cache), language)} · {formatNumber(entry.duration_ms, language, { maximumFractionDigits: 2 })} ms</p>
          </article>)}</div>
          <div tabIndex={0} role="region" aria-label={translate("views.logsRegion", language)} className="panel log-table table-scroll">
            <table className="data-table"><caption className="sr-only">{translate("views.logsCaption", language)}</caption>
              <thead><tr>
                {(["time", "query", "clientTransport", "resultPath", "duration"] as const).map((key) => <th key={key} scope="col">{translate(`views.${key}`, language)}</th>)}
                <th scope="col"><span className="sr-only">{translate("views.details", language)}</span></th>
              </tr></thead><tbody>{entries.map((entry) => <tr key={entry.id} className="log-row" data-open={openId === entry.id || undefined} data-enter={entry.id > enterAfter || undefined}>
                <td className="num nowrap"><div className="log-cell">{formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "medium" })}</div></td>
                <td><div className="log-cell"><span className="mono log-name">{queryName(entry)}</span><span className="muted small">{entry.qtype || "—"}</span></div></td>
                <td><div className="log-cell"><span className="mono">{entry.client}</span><span className="muted small">{entry.transport}</span></div></td>
                <td><div className="log-cell"><span className="badge" data-status={entry.status}>{translate(statusLabel(entry.status), language)}</span><span className="muted small">{translate(pathLabel(entry.cache), language)}</span></div></td>
                <td className="num nowrap"><div className="log-cell">{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</div></td>
                <td className="log-detail-cell"><div className="log-cell">{detailButton(entry.id)}</div></td>
              </tr>)}</tbody></table>
          </div>
        </>}
    <AnimatePresence>{detail && detailEntry && openId !== null && <DetailPopover key={detail.id} id={detailId(detail.id)} label={translate("views.queryDetails", language)} anchor={detail.anchor} focus={detail.keyboard}
      onPointerEnter={(event) => { if (event.pointerType === 'mouse') cancelTimer(); }} onPointerLeave={hoverClose}
      onPointerDown={() => setDetail((current) => current && { ...current, pinned: true })} onKeyDown={cardKey}>
      <LogDetail entry={detailEntry} language={language} />
    </DetailPopover>}</AnimatePresence>

    {page?.next_cursor && <button type="button" onClick={() => void load(page.next_cursor, applied)} disabled={loading}
      className="button secondary load-more">{translate("ui.earlier", language)}</button>}

    <Drawer open={confirmation !== null} title={translate('app.clearLogsTitle', language)} closeLabel={translate('ui.closeDialog', language)} onClose={() => setConfirmation(null)}>
      <p id="log-clear-help" className="muted">{translate("storage.confirmLogs", language)}</p>
      <div className="button-group confirm-actions">
        <button type="button" onClick={() => setConfirmation(null)} className="button secondary">{translate("ui.cancel", language)}</button>
        <button type="button" onClick={() => void clearLogs()} className="button danger">{translate("app.clearLogs", language)}</button>
      </div>
    </Drawer>
  </div>;
}
