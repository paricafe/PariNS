import { useCallback, useEffect, useRef, useState, type FormEvent, type MouseEvent } from "react";
import { ApiError, StaleRequest, type ApiClient } from "../../session/client";
import { formatDate, formatNumber, hasTranslation, translate, type Language } from "../../i18n";
import { pathLabel, statusLabel, type LogEntry, type LogFilter, type LogListResponse } from "./logs";

export interface LogsPageProps {
  api: ApiClient;
  language: Language;
  onOpenSettings: () => void;
}

function localizedError(error: unknown, language: Language): string {
  if (error instanceof ApiError) {
    const key = `api.${error.code}`;
    if (hasTranslation(key)) return translate(key, language, { detail: error.message, status: error.status });
  }
  return translate("app.loadFailed", language);
}

function DetailRow({ label, children }: { label: string; children: React.ReactNode }) {
  return <div className="grid gap-1 border-b border-zinc-100 py-2 text-sm last:border-0 dark:border-zinc-800 sm:grid-cols-[9rem_1fr]">
    <dt className="text-zinc-500 dark:text-zinc-400">{label}</dt><dd className="min-w-0 break-all">{children}</dd>
  </div>;
}

function LogDetail({ entry, language }: { entry: LogEntry; language: Language }) {
  const none = translate("views.none", language);
  const answer = entry.answer.map((record) => `${record.name} ${record.ttl} ${record.record_type} ${record.data}`).join("\n");
  return <div className="space-y-5">
    <section aria-labelledby="log-detail-summary"><h3 id="log-detail-summary" className="font-semibold">{translate("views.query", language)}</h3>
      <dl className="mt-2">
        <DetailRow label={translate("views.recordId", language)}>{formatNumber(entry.id, language)}</DetailRow>
        <DetailRow label={translate("views.time", language)}>{formatDate(entry.time_ms, language, { dateStyle: "medium", timeStyle: "medium" })}</DetailRow>
        <DetailRow label={translate("views.name", language)}>{entry.name || translate("views.unreadableQuery", language)}</DetailRow>
        <DetailRow label={translate("views.qtype", language)}>{entry.qtype || "—"}</DetailRow>
        <DetailRow label={translate("views.client", language)}>{entry.client}</DetailRow>
        <DetailRow label={translate("views.transport", language)}>{entry.transport}</DetailRow>
      </dl></section>
    <section aria-labelledby="log-detail-process"><h3 id="log-detail-process" className="font-semibold">{translate("views.path", language)}</h3>
      <dl className="mt-2">
        <DetailRow label={translate("ui.result", language)}>{translate(statusLabel(entry.status), language)}</DetailRow>
        <DetailRow label={translate("views.path", language)}>{translate(pathLabel(entry.cache), language)}</DetailRow>
        <DetailRow label={translate("views.upstream", language)}>{entry.upstream || none}</DetailRow>
        <DetailRow label={translate("views.rcode", language)}>{entry.rcode || translate("views.noResponse", language)}</DetailRow>
        <DetailRow label={translate("views.duration", language)}>{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</DetailRow>
      </dl></section>
    <section aria-labelledby="log-detail-dns"><h3 id="log-detail-dns" className="font-semibold">{translate("views.flags", language)}</h3>
      <dl className="mt-2">
        <DetailRow label={translate("views.incomingEcs", language)}>{entry.incoming_ecs || none}</DetailRow>
        <DetailRow label={translate("views.outgoingEcs", language)}>{entry.outgoing_ecs || none}</DetailRow>
        <DetailRow label={translate("views.flags", language)}><span className="font-mono text-xs">EDNS={Number(entry.edns)} · DO={Number(entry.dnssec_ok)} · CD={Number(entry.checking_disabled)} · RD={Number(entry.recursion_desired)}</span></DetailRow>
      </dl></section>
    <section aria-labelledby="log-detail-answers"><h3 id="log-detail-answers" className="font-semibold">{translate("views.answers", language)}</h3>
      <pre className="mt-2 max-h-56 overflow-auto rounded-md bg-zinc-100 p-3 text-xs dark:bg-zinc-800">{answer || translate("views.noAnswers", language)}</pre>
      {entry.answer_truncated && <p className="mt-2 text-xs text-zinc-500 dark:text-zinc-400">{translate("views.answerTruncated", language)}</p>}
    </section>
  </div>;
}

export function LogsPage({ api, language, onOpenSettings }: LogsPageProps) {
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [applied, setApplied] = useState<LogFilter>({ search: "", status: null });
  const appliedRef = useRef<LogFilter>(applied);
  const [result, setResult] = useState<LogListResponse | null>(null);
  const [updated, setUpdated] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState("");
  const [notice, setNotice] = useState("");
  const [selected, setSelected] = useState<LogEntry | null>(null);
  const [confirmRevision, setConfirmRevision] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);
  const requestId = useRef(0);
  const drawer = useRef<HTMLDialogElement>(null);
  const clearDialog = useRef<HTMLDialogElement>(null);
  const detailClose = useRef<HTMLButtonElement>(null);
  const clearCancel = useRef<HTMLButtonElement>(null);
  const opener = useRef<HTMLButtonElement | null>(null);

  const load = useCallback(async (before: number | null, filter: LogFilter) => {
    const owner = ++requestId.current;
    setLoading(true);
    setFailure("");
    try {
      const next = await api.request<LogListResponse>("query-log/list", "POST", { ...filter, before_id: before, limit: 50 });
      if (owner === requestId.current) { setResult(next); setUpdated(Date.now()); }
    } catch (error) {
      if (owner === requestId.current && !(error instanceof StaleRequest)) setFailure(localizedError(error, language));
    } finally {
      if (owner === requestId.current) setLoading(false);
    }
  }, [api, language]);

  useEffect(() => {
    if (document.visibilityState !== "hidden") void load(null, appliedRef.current);
    const onVisible = () => { if (document.visibilityState !== "hidden") void load(null, appliedRef.current); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { requestId.current += 1; document.removeEventListener("visibilitychange", onVisible); };
  }, [load]);

  useEffect(() => {
    const dialog = drawer.current;
    if (!dialog) return;
    if (selected) {
      if (!dialog.open) dialog.showModal();
      detailClose.current?.focus();
    } else if (dialog.open) dialog.close();
  }, [selected]);

  useEffect(() => {
    const dialog = clearDialog.current;
    if (!dialog) return;
    if (confirmRevision !== null) {
      if (!dialog.open) dialog.showModal();
      clearCancel.current?.focus();
    } else if (dialog.open) dialog.close();
  }, [confirmRevision]);

  function submitFilter(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const filter = { search: search.trim(), status: statusFilter || null };
    appliedRef.current = filter;
    setApplied(filter);
    setNotice("");
    void load(null, filter);
  }

  function openDetail(entry: LogEntry, event: MouseEvent<HTMLButtonElement>) {
    opener.current = event.currentTarget;
    setSelected(structuredClone(entry));
  }

  async function clearLogs() {
    const revision = confirmRevision;
    if (revision === null) return;
    setConfirmRevision(null);
    setClearing(true);
    setNotice("");
    try {
      await api.request("query-log/clear", "POST", { revision });
      setNotice(translate("app.logsCleared", language));
      await load(null, applied);
    } catch (error) {
      if (!(error instanceof StaleRequest)) setFailure(error instanceof ApiError && error.status === 0
        ? translate("app.clearLogsUnknown", language) : localizedError(error, language));
    } finally { setClearing(false); }
  }

  const page = result?.page ?? null;
  const entries = page?.entries ?? [];
  const statuses = ["", "success", "blocked", "error", "dropped"] as const;
  return <div className="space-y-5 pb-28 md:pb-8">
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div><p className="text-xs font-semibold uppercase tracking-widest text-zinc-500 dark:text-zinc-400">{translate("ui.logsEyebrow", language)}</p>
        <h1 className="mt-1 text-2xl font-semibold tracking-tight">{translate("ui.logs", language)}</h1>
        <p className="mt-2 max-w-3xl text-sm text-zinc-600 dark:text-zinc-400">{translate("ui.logIntro", language)}</p></div>
      <button type="button" onClick={onOpenSettings} className="min-h-11 rounded-md border border-zinc-400 px-3 text-sm hover:bg-zinc-100 focus-visible:outline-2 dark:border-zinc-600 dark:hover:bg-zinc-800">
        {translate("ui.logSettings", language)}
      </button>
    </div>

    <form onSubmit={submitFilter} className="grid gap-3 rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-800 dark:bg-zinc-900 sm:grid-cols-[minmax(0,1fr)_12rem_auto] sm:items-end">
      <label className="min-w-0 text-sm"><span className="mb-1 block font-medium">{translate("ui.logSearch", language)}</span>
        <input value={search} maxLength={256} onChange={(event) => setSearch(event.target.value)} type="search" autoComplete="off" placeholder={translate("ui.logSearchPlaceholder", language)}
          className="min-h-10 w-full min-w-0 rounded-md border border-zinc-400 bg-transparent px-3 outline-offset-2 dark:border-zinc-600" /></label>
      <label className="text-sm"><span className="mb-1 block font-medium">{translate("ui.result", language)}</span>
        <select value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)} className="min-h-10 w-full rounded-md border border-zinc-400 bg-transparent px-3 dark:border-zinc-600">
          {statuses.map((status) => <option key={status} value={status}>{translate(status ? `ui.${status}` : "ui.allResults", language)}</option>)}
        </select></label>
      <button type="submit" className="min-h-11 rounded-md bg-zinc-900 px-4 text-sm font-medium text-white hover:bg-zinc-700 focus-visible:outline-2 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300">{translate("ui.filter", language)}</button>
    </form>

    <div className="flex flex-wrap items-center justify-between gap-3">
      <p role="status" className="text-sm text-zinc-600 dark:text-zinc-400">{loading ? translate("app.reading", language)
        : page && updated !== null ? page.enabled
          ? translate("app.logSummary", language, { count: formatNumber(entries.length, language), total: formatNumber(page.total, language), time: formatDate(updated, language, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })
          : translate("app.logsDisabled", language)
          : translate("ui.logsUnread", language)}</p>
      <div className="flex flex-wrap gap-2">
        <button type="button" onClick={() => void load(null, applied)} disabled={loading} className="min-h-11 rounded-md border border-zinc-400 px-3 text-sm disabled:opacity-50 dark:border-zinc-600">{translate("ui.refreshLatest", language)}</button>
        <button type="button" onClick={() => result && setConfirmRevision(result.revision)} disabled={!page?.total || clearing}
          className="min-h-11 rounded-md border border-zinc-400 px-3 text-sm disabled:opacity-50 dark:border-zinc-600">{translate("ui.clearLogs", language)}</button>
      </div>
    </div>
    {failure && <p role="alert" className="rounded-md border border-red-300 p-3 text-sm text-red-800 dark:border-red-900 dark:text-red-300">{failure}</p>}
    {notice && <p role="status" className="rounded-md border border-zinc-300 p-3 text-sm dark:border-zinc-700">{notice}</p>}

    {page === null ? <p className="rounded-lg border border-zinc-200 bg-white p-8 text-center text-sm dark:border-zinc-800 dark:bg-zinc-900">{translate(failure ? "app.loadFailed" : "app.reading", language)}</p>
      : !page.enabled ? <p className="rounded-lg border border-zinc-200 bg-white p-8 text-center text-sm dark:border-zinc-800 dark:bg-zinc-900">{translate("views.logsDisabled", language)}</p>
      : entries.length === 0 ? <p className="rounded-lg border border-zinc-200 bg-white p-8 text-center text-sm dark:border-zinc-800 dark:bg-zinc-900">{translate("views.logsEmpty", language)}</p>
        : <>
          <div className="space-y-2 md:hidden">{entries.map((entry) => <article key={entry.id} className="rounded-lg border border-zinc-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900">
            <div className="flex items-start justify-between gap-3"><div className="min-w-0"><p className="break-all font-mono text-sm">{entry.name || translate("views.unreadableQuery", language)}</p><p className="text-xs text-zinc-500 dark:text-zinc-400">{entry.qtype || "—"} · {formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "short" })}</p></div>
              <button type="button" onClick={(event) => openDetail(entry, event)} aria-label={`${translate("views.details", language)} · ${entry.name || translate("views.unreadableQuery", language)} · #${entry.id}`} className="min-h-11 shrink-0 text-sm underline underline-offset-4">{translate("views.details", language)}</button></div>
            <p className="mt-2 break-all text-xs text-zinc-600 dark:text-zinc-400">{entry.client} · {translate(statusLabel(entry.status), language)} · {translate(pathLabel(entry.cache), language)} · {formatNumber(entry.duration_ms, language, { maximumFractionDigits: 2 })} ms</p>
          </article>)}</div>
          <div tabIndex={0} role="region" aria-label={translate("views.logsRegion", language)} className="hidden overflow-x-auto rounded-lg border border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900 md:block">
            <table className="w-full min-w-[52rem] text-left text-sm"><caption className="sr-only">{translate("views.logsCaption", language)}</caption>
              <thead><tr className="border-b border-zinc-200 text-xs text-zinc-500 dark:border-zinc-800 dark:text-zinc-400">
                {(["time", "query", "clientTransport", "resultPath", "duration", "details"] as const).map((key) => <th key={key} scope="col" className="px-3 py-3 font-medium">{translate(`views.${key}`, language)}</th>)}
              </tr></thead><tbody>{entries.map((entry) => <tr key={entry.id} className="border-b border-zinc-100 last:border-0 dark:border-zinc-800/70">
                <td className="whitespace-nowrap px-3 py-3 tabular-nums">{formatDate(entry.time_ms, language, { dateStyle: "short", timeStyle: "medium" })}</td>
                <td className="px-3 py-3"><span className="block max-w-52 break-all font-mono">{entry.name || translate("views.unreadableQuery", language)}</span><span className="text-xs text-zinc-500 dark:text-zinc-400">{entry.qtype || "—"}</span></td>
                <td className="px-3 py-3"><span className="block font-mono">{entry.client}</span><span className="text-xs text-zinc-500 dark:text-zinc-400">{entry.transport}</span></td>
                <td className="px-3 py-3"><span className="block">{translate(statusLabel(entry.status), language)}</span><span className="text-xs text-zinc-500 dark:text-zinc-400">{translate(pathLabel(entry.cache), language)}</span></td>
                <td className="whitespace-nowrap px-3 py-3 tabular-nums">{formatNumber(entry.duration_ms, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms</td>
                <td className="px-3 py-3"><button type="button" onClick={(event) => openDetail(entry, event)} aria-label={`${translate("views.details", language)} · ${entry.name || translate("views.unreadableQuery", language)} · #${entry.id}`} className="min-h-10 underline underline-offset-4">{translate("views.details", language)}</button></td>
              </tr>)}</tbody></table>
          </div>
        </>}

    {page?.next_cursor && <button type="button" onClick={() => void load(page.next_cursor, applied)} disabled={loading}
      className="min-h-11 rounded-md border border-zinc-400 px-4 text-sm disabled:opacity-50 dark:border-zinc-600">{translate("ui.earlier", language)}</button>}

    <dialog ref={drawer} aria-labelledby="log-detail-title" onCancel={(event) => { event.preventDefault(); setSelected(null); }}
      onClose={() => { setSelected(null); opener.current?.focus(); }}
      className="fixed inset-y-0 right-0 left-auto m-0 h-dvh max-h-dvh w-full max-w-[36rem] border-l border-zinc-300 bg-white p-0 text-zinc-900 shadow-xl backdrop:bg-black/50 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-100">
      {selected && <div className="flex h-full flex-col"><div className="flex items-center justify-between border-b border-zinc-200 p-4 dark:border-zinc-800">
        <h2 id="log-detail-title" className="text-lg font-semibold">{translate("views.details", language)}</h2>
        <button ref={detailClose} type="button" onClick={() => setSelected(null)} aria-label={translate("ui.closeDetails", language)} className="min-h-10 rounded-md border border-zinc-400 px-3 text-sm dark:border-zinc-600">{translate("ui.cancel", language)}</button>
      </div><div className="min-h-0 flex-1 overflow-y-auto p-4"><LogDetail entry={selected} language={language} /></div></div>}
    </dialog>

    <dialog ref={clearDialog} aria-labelledby="log-clear-title" aria-describedby="log-clear-help"
      onCancel={(event) => { event.preventDefault(); setConfirmRevision(null); }}
      className="w-full max-w-md rounded-xl border border-zinc-300 bg-white p-5 text-zinc-900 shadow-xl backdrop:bg-black/50 dark:border-zinc-700 dark:bg-zinc-950 dark:text-zinc-100">
      <h2 id="log-clear-title" className="text-lg font-semibold">{translate("app.clearLogsTitle", language)}</h2>
      <p id="log-clear-help" className="mt-2 text-sm text-zinc-600 dark:text-zinc-400">{translate("app.clearLogsHelp", language)}</p>
      <div className="mt-5 flex justify-end gap-2">
        <button ref={clearCancel} type="button" onClick={() => setConfirmRevision(null)} className="min-h-11 rounded-md border border-zinc-400 px-4 text-sm dark:border-zinc-600">{translate("ui.cancel", language)}</button>
        <button type="button" onClick={() => void clearLogs()} className="min-h-11 rounded-md bg-zinc-900 px-4 text-sm font-medium text-white dark:bg-zinc-100 dark:text-zinc-900">{translate("app.clearLogs", language)}</button>
      </div>
    </dialog>
  </div>;
}
