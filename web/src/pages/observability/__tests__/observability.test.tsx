// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ApiError } from "../../../session/client";
import type { ApiClient } from "../../../session/client";
import { LogsPage } from "../LogsPage";
import { OverviewPage } from "../OverviewPage";
import { averageLatencyMillis, cacheHitRate, coalescedShare, histogramRows, isolatedPoints, trendSeries, type HistorySample, type StatusView } from "../metrics";
import type { LogEntry, LogListResponse } from "../logs";
import { storage } from './fixtures';

afterEach(() => { cleanup(); vi.restoreAllMocks(); });
beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute("open", ""); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute("open"); this.dispatchEvent(new Event("close")); };
});

function fakeApi(handler: (path: string, body?: unknown) => unknown): ApiClient {
  return { request: vi.fn(async (path: string, _method?: string, body?: unknown) => handler(path, body)) } as unknown as ApiClient;
}

const status: StatusView = {
  running: true, revision: 7, last_error: null, listen: "127.0.0.1:5353", uptime_seconds: 3600, generation: 2,
  dns_health: { state: 'running', ready: true, generation: 2, changed_at_ms: Date.now(), code: null }, resolution_mode: 'forward', diagnostics: { cache: null, upstreams: null, recursion: null, quic: { scope: 'process', doq: { handshake_inflight: 0, handshake_established: 0, stream_inflight: 0, stream_response_handed_to_transport: 0 }, doh3: { handshake_inflight: 0, handshake_established: 0, stream_inflight: 0, stream_response_handed_to_transport: 0 } } },
  metrics: {
    counters: { requests: 100, cache_hits: 30, cache_misses: 70, query_blocked: 2, response_blocked: 3,
      upstream_failures: 4, responses_noerror: 80, responses_nxdomain: 10, responses_servfail: 5, responses_refused: 3, responses_other: 2 },
    request_inflight: 1,
    request_latency: { count: 10, sum_micros: 25_000, buckets: [{ upper_bound_micros: 1_000, count: 3 }, { upper_bound_micros: null, count: 10 }] },
  },
};

const entry: LogEntry = {
  id: 42, time_ms: Date.now(), client: "192.0.2.10", name: "example.test", qtype: "A", transport: "udp",
  status: "success", rcode: "NoError", duration_ms: 2.5, cache: "fresh", upstream: "udp://192.0.2.53:53",
  cache_lookup: { outcome: 'fresh', reason: null }, cache_store: null, cache_scope: 'exact_ecs:192.0.2.0/24',
  upstream_relation: null,
  upstream_attempts: null,
  upstream_attempts_omitted: null,
  failure_stage: null,
  failure_reason: null,
  incoming_ecs: null, outgoing_ecs: "192.0.2.0/24", edns: true, dnssec_ok: false,
  checking_disabled: false, recursion_desired: true,
  answer: [{ name: "example.test", record_type: "A", ttl: 60, data: "192.0.2.1" }], answer_truncated: false,
};
const stats = { interval_seconds: 60, retention_seconds: 604800, samples: [], totals: { scope: 'persistent', epoch: 4, since_ms: Date.now(), metrics: status.metrics }, storage, process_scope: 'process', process_metrics: status.metrics, history_epoch: 5 };

describe("observability data math", () => {
  it("uses cache lookups and request timer counts as denominators", () => {
    expect(cacheHitRate(status.metrics?.counters)).toBe(30);
    expect(cacheHitRate({ cache_hits: 0, cache_misses: 0 })).toBeNull();
    // Coalesced followers are reported beside the hit rate, never folded into it.
    expect(coalescedShare({ cache_hits: 30, cache_misses: 70, flight_joined: 8 })).toBe(8);
    expect(coalescedShare({ cache_hits: 30, cache_misses: 70, flight_joined: 0 })).toBeNull();
    expect(cacheHitRate({ cache_hits: 30, cache_misses: 70, flight_joined: 8 })).toBe(30);
    expect(averageLatencyMillis(status.metrics?.request_latency)).toBe(2.5);
    expect(averageLatencyMillis({ count: 0, sum_micros: 0, buckets: [] })).toBeNull();
  });

  it("turns cumulative latency buckets into bounded bucket counts", () => {
    expect(histogramRows(status.metrics!.request_latency.buckets)).toEqual([
      { lowerMillis: 0, upperMillis: 1, count: 3 },
      { lowerMillis: 1, upperMillis: null, count: 7 },
    ]);
  });

  it("breaks trend lines on generation switches, missing intervals and stopped service", () => {
    const now = Date.now();
    const samples: HistorySample[] = [
      { timestamp_ms: now - 240_000, elapsed_seconds: 60, running: true, generation: 1, requests: 60, cache_hits: 30, blocked: 3 },
      { timestamp_ms: now - 180_000, elapsed_seconds: 60, running: true, generation: 1, requests: 120, cache_hits: 60, blocked: 6 },
      { timestamp_ms: now - 60_000, elapsed_seconds: 0, running: false, generation: 1, requests: 0, cache_hits: 0, blocked: 0 },
      { timestamp_ms: now, elapsed_seconds: 60, running: true, generation: 2, requests: 30, cache_hits: 10, blocked: 1 },
    ];
    const points = trendSeries(samples, "requests", 1, now);
    expect(points.map((point) => point.value)).toEqual([1, 2, null, 0.5]);
    expect(points.map((point) => point.breakBefore)).toEqual([true, false, true, true]);
    expect(isolatedPoints(points).map((point) => point.time)).toEqual([now]);
  });
  it('does not draw a positive-duration gap or connect across epochs', () => {
    const now = Date.now();
    const base = { timestamp_ms: now, elapsed_seconds: 60, running: true, generation: 1, requests: 60, cache_hits: 30, blocked: 0, history_epoch: 1, totals_epoch: 1, run_id: 'a' };
    const points = trendSeries([
      { ...base, timestamp_ms: now - 180000 },
      { ...base, timestamp_ms: now - 120000, gap: 'clock_jump' },
      { ...base, timestamp_ms: now - 60000 },
      { ...base, totals_epoch: 2 },
    ], 'requests', 1, now);
    expect(points.map((point) => point.value)).toEqual([1, null, 1, 1]);
    expect(points.map((point) => point.breakBefore)).toEqual([true, true, true, true]);
  });
});

describe("OverviewPage", () => {
  it('judges upstream health only from a polled window of upstream counters', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let operations = 100; let failures = 0;
      const api = fakeApi((path) => path === 'status'
        ? { ...status, metrics: { ...status.metrics, counters: { ...status.metrics!.counters, upstream_operations: operations, upstream_failures: failures } } }
        : stats);
      render(<OverviewPage api={api} language="en" onOpenDns={() => {}} />);
      // One sample has no delta, so the page does not yet claim upstreams are healthy.
      await waitFor(() => expect(screen.getByText('DNS is running')).toBeTruthy());
      operations += 10; failures += 8;
      await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
      await waitFor(() => expect(screen.getByText('Upstreams are not responding')).toBeTruthy());
      // Once the failing samples leave the 65 s window, the warning clears.
      operations += 10;
      await act(async () => { await vi.advanceTimersByTimeAsync(70_000); });
      await waitFor(() => expect(screen.getByText('Everything is running normally')).toBeTruthy());
    } finally { vi.useRealTimers(); }
  });
  it('names recursive resolution, not upstreams, when recursion is failing', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let operations = 100; let failures = 0;
      const api = fakeApi((path) => path === 'status'
        ? { ...status, resolution_mode: 'recursive', metrics: { ...status.metrics, counters: { ...status.metrics!.counters, upstream_operations: operations, upstream_failures: failures } } }
        : stats);
      render(<OverviewPage api={api} language="en" onOpenDns={() => {}} />);
      await waitFor(() => expect(screen.getByText('DNS is running')).toBeTruthy());
      operations += 10; failures += 8;
      await act(async () => { await vi.advanceTimersByTimeAsync(5_000); });
      await waitFor(() => expect(screen.getByText('Recursive resolution is failing')).toBeTruthy());
      expect(screen.queryByText('Upstreams are not responding')).toBeNull();
    } finally { vi.useRealTimers(); }
  });
  it('distinguishes a failed runtime from an ordinary stop while retaining cumulative totals', async () => {
    const failed = { ...status, running: false, dns_health: { ...status.dns_health, state: 'failed', ready: false, code: 'task_panic' } };
    const api = fakeApi((path) => path === 'status' ? failed : stats);
    render(<OverviewPage api={api} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText('Service stopped unexpectedly')).toBeTruthy());
    expect(screen.getByText('Not ready yet')).toBeTruthy();
    expect(screen.getByText('task_panic')).toBeTruthy();
    expect(screen.getByText('100')).toBeTruthy();
  });
  it('explains a startup failure with the occupied listener and the automatic retry', async () => {
    const error = 'DNS unavailable: bind DoH 0.0.0.0:443: Address in use (os error 98)';
    const failed = { ...status, running: false, last_error: error, dns_health: { ...status.dns_health, state: 'failed', ready: false, code: 'startup_failed' } };
    const view = render(<OverviewPage api={fakeApi((path) => path === 'status' ? failed : stats)} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText('DNS is not running')).toBeTruthy());
    expect(screen.getByText('0.0.0.0:443 for DoH is used by another program.')).toBeTruthy();
    expect(screen.getByText(/Retrying automatically/)).toBeTruthy();
    expect(screen.getByText('Could not start')).toBeTruthy();
    expect(screen.getByText(error)).toBeTruthy();
    view.unmount();
    // Another startup error keeps the generic explanation and the raw text.
    const other = { ...failed, last_error: 'DNS unavailable: subscription_material_missing: DNS policy unavailable' };
    render(<OverviewPage api={fakeApi((path) => path === 'status' ? other : stats)} language="zh-CN" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText('DNS 没有运行')).toBeTruthy());
    expect(screen.queryByText(/被其他程序占用/)).toBeNull();
    expect(screen.getByText(/正在自动重试/)).toBeTruthy();
  });
  it('lists listeners that are being rebound while the others keep serving', async () => {
    const degraded = { ...status, dns_health: { ...status.dns_health, state: 'degraded', ready: false, code: 'listener_unavailable' }, listeners: [
      { kind: 'dns', address: '0.0.0.0:53', state: 'running', error: null },
      { kind: 'doh', address: '0.0.0.0:443', state: 'retrying', error: 'bind DoH 0.0.0.0:443: Address in use (os error 98)' },
    ] } as StatusView;
    render(<OverviewPage api={fakeApi((path) => path === 'status' ? degraded : stats)} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getAllByText('Some listeners are unavailable').length).toBeGreaterThan(0));
    expect(screen.getByText('These listeners are unavailable')).toBeTruthy();
    expect(screen.getByText(/0.0.0.0:443 for DoH is used by another program./)).toBeTruthy();
    expect(screen.getByText('bind DoH 0.0.0.0:443: Address in use (os error 98)')).toBeTruthy();
    expect(screen.queryByText(/0.0.0.0:53 for DNS/)).toBeNull();
    expect(screen.getByText('Some listeners unavailable')).toBeTruthy();
  });
  it('shows the running resolution mode and recursion diagnostics without upstream diagnostics', async () => {
    const view = render(<OverviewPage api={fakeApi((path) => path === 'status' ? status : stats)} language="en" onOpenDns={() => {}} upstreamCount={2} />);
    await waitFor(() => expect(screen.getByText('Forward')).toBeTruthy());
    expect(screen.getByText('Upstreams')).toBeTruthy();
    expect(screen.queryByText('Recursive resolution diagnostics')).toBeNull();
    view.unmount();
    const recursion = { scope: 'recursion_generation', resolutions: { answered: 7, bogus: 1, failed: 0, negative: 2, shutdown: 0, started: 10 }, failures: { budget: 0, other: 0, referral: 0, timeout: 0 }, dnssec: { bogus: 1, indeterminate: 0, insecure: 6, secure: 3 }, ecs_scope: { absent: 4, not_sent: 5, scoped: 1 }, outbound: { inflight: 3, rate_wait_ms: 12, rate_waits: 2, refused_after_shutdown: 0, sent: 42 } };
    const recursive = { ...status, resolution_mode: 'recursive', diagnostics: { ...status.diagnostics, upstreams: null, recursion } } as StatusView;
    render(<OverviewPage api={fakeApi((path) => path === 'status' ? recursive : stats)} language="zh-CN" onOpenDns={() => {}} upstreamCount={2} />);
    await waitFor(() => expect(screen.getByText('递归（DNSSEC 验证）')).toBeTruthy());
    expect(screen.queryByText('上游')).toBeNull();
    expect(screen.getByText('递归解析诊断')).toBeTruthy();
    expect(screen.queryByText('上游请求诊断')).toBeNull();
  });
  it('shows no resolution mode while DNS is not running', async () => {
    const stopped = { ...status, running: false, resolution_mode: null, dns_health: { ...status.dns_health, state: 'stopped', ready: false } } as StatusView;
    render(<OverviewPage api={fakeApi((path) => path === 'status' ? stopped : stats)} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText('Resolution')).toBeTruthy());
    expect(screen.getByText('Resolution').nextElementSibling?.textContent).toBe('—');
  });
  it('reports coalesced followers beside the unchanged hit rate', async () => {
    const totals = { ...stats, totals: { ...stats.totals, metrics: { ...status.metrics!, counters: { ...status.metrics!.counters, flight_joined: 9 } } } };
    render(<OverviewPage api={fakeApi((path) => path === 'status' ? status : totals)} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText(/Another 9% joined an identical query already in progress/)).toBeTruthy());
    expect(screen.getByText("30.0%")).toBeTruthy();
  });
  it("renders cumulative metrics separately from time-window trend", async () => {
    const api = fakeApi((path) => path === "status" ? status : stats);
    const openDns = vi.fn();
    render(<OverviewPage api={api} language="en" onOpenDns={openDns} />);
    await waitFor(() => expect(screen.getByText("100")).toBeTruthy());
    expect(screen.getByText("30.0%")).toBeTruthy();
    expect(screen.getByText("Cache hit rate")).toBeTruthy();
    expect(screen.getByText("2.50 ms")).toBeTruthy();
    expect(screen.getAllByText("5").length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("tab", { name: "Last 7 days" }));
    expect(screen.getByRole("tab", { name: "Last 7 days" }).getAttribute("aria-selected")).toBe("true");
    expect(api.request).toHaveBeenCalledWith('stats', 'GET', undefined, undefined, { query: { range: '7d' } });
    fireEvent.click(screen.getByRole("button", { name: /Upstreams & listeners/ }));
    expect(openDns).toHaveBeenCalledOnce();
  });

  it("retains last good status and offers refresh on a later read failure", async () => {
    let reads = 0;
    const api = fakeApi((path) => {
      if (path === "stats") return stats;
      reads += 1;
      if (reads > 1) throw new Error("offline");
      return status;
    });
    render(<OverviewPage api={api} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText("100")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("unavailable right now"));
    expect(screen.getByText("100")).toBeTruthy();
  });
});

describe("LogsPage", () => {
  it('resets cursors for cache filters and ignores late pages while keeping current content', async () => {
    let finishOld!: (value: unknown) => void;
    const page = (entries: LogEntry[]) => ({ revision: 7, page: { log_epoch: 3, storage, enabled: true, total: 2, entries, next_cursor: 20 } });
    const api = fakeApi((_path, body) => {
      const input = body as { cache: string | null; before_id: number | null };
      if (input.before_id) return new Promise((resolve) => { finishOld = resolve; });
      return page([{ ...entry, name: input.cache === 'stale' ? 'stale.test' : 'fresh.test' }]);
    });
    const view = render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getAllByText('fresh.test').length).toBeGreaterThan(0));
    fireEvent.click(screen.getByRole('button', { name: /Older queries/ }));
    fireEvent.click(screen.getByRole('tab', { name: 'Old-answer fallback' }));
    await waitFor(() => expect(screen.getAllByText('stale.test').length).toBeGreaterThan(0));
    expect(api.request).toHaveBeenLastCalledWith('query-log/list', 'POST', { search: '', status: null, cache: 'stale', before_id: null, limit: 50 });
    await act(async () => { finishOld(page([{ ...entry, name: 'late.test' }])); });
    expect(screen.queryByText('late.test')).toBeNull();
    fireEvent.click(screen.getAllByRole('button', { name: /stale.test/ })[0]);
    expect(screen.getAllByText('exact_ecs:192.0.2.0/24').length).toBeGreaterThan(0);
    // The raw scope code stays verbatim, with a plain-language note beside it.
    expect(screen.getAllByText('Only for requests from exactly 192.0.2.0/24').length).toBeGreaterThan(0);
    view.unmount();
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(screen.queryByText('192.0.2.0/24')).toBeNull();
  });

  it("uses server filters/cursor, expands details inline, and clears with the listed revision", async () => {
    const requests: { path: string; body: unknown }[] = [];
    let entries = [entry];
    const api = fakeApi((path, body) => {
      requests.push({ path, body });
      if (path === "query-log/clear") { entries = []; return { removed: 1, revision: 7 }; }
      const response: LogListResponse = { revision: 7, page: { log_epoch: 3, storage, enabled: true, total: entries.length, entries, next_cursor: entries.length ? 20 : null } };
      return response;
    });
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getAllByText("example.test").length).toBeGreaterThan(0));
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: " example.test " } });
    fireEvent.click(screen.getByRole("button", { name: /^Result:/ }));
    fireEvent.click(screen.getByRole("option", { name: "Success" }));
    fireEvent.click(screen.getByRole("button", { name: "Filter" }));
    await waitFor(() => expect(requests.some((request) => request.path === "query-log/list"
      && (request.body as { search: string }).search === "example.test"
      && (request.body as { status: string }).status === "success")).toBe(true));
    fireEvent.click(screen.getByRole("button", { name: /Older queries/ }));
    await waitFor(() => expect(requests.some((request) => (request.body as { before_id: number })?.before_id === 20)).toBe(true));
    const control = screen.getAllByRole("button", { name: /example.test/ })[0];
    fireEvent.click(control);
    expect(control.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getAllByText(/example.test 60 A 192.0.2.1/).length).toBeGreaterThan(0);
    expect(screen.getAllByText("192.0.2.0/24").length).toBeGreaterThan(0);
    fireEvent.click(control);
    expect(control.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(screen.getByRole("button", { name: /Clear log/ }));
    fireEvent.click(screen.getByRole("button", { name: "Clear log" }));
    await waitFor(() => expect(requests.some((request) => request.path === "query-log/clear"
      && (request.body as { revision: number; log_epoch: number }).revision === 7 && (request.body as { log_epoch: number }).log_epoch === 3)).toBe(true));
    expect(screen.getByText("Query log cleared.")).toBeTruthy();
  });

  it("switches language without re-requesting or losing the older page and translates failures at render time", async () => {
    const api = fakeApi((_path, body) => {
      const before = (body as { before_id: number | null }).before_id;
      return { revision: 7, page: { log_epoch: 3, storage, enabled: true, total: 2, entries: [{ ...entry, name: before ? 'older.test' : 'latest.test' }], next_cursor: before ? null : 20 } };
    });
    const view = render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getAllByText('latest.test').length).toBeGreaterThan(0));
    fireEvent.click(screen.getByRole('button', { name: /Older queries/ }));
    await waitFor(() => expect(screen.getAllByText('older.test').length).toBeGreaterThan(0));
    view.rerender(<LogsPage api={api} language="zh-CN" onOpenSettings={() => {}} />);
    expect(screen.getAllByText('older.test').length).toBeGreaterThan(0);
    expect(api.request).toHaveBeenCalledTimes(2);

    cleanup();
    const failing = fakeApi(() => { throw new ApiError(0, 'NETWORK', 'offline'); });
    const failed = render(<LogsPage api={failing} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain("Can't reach PariNS"));
    failed.rerender(<LogsPage api={failing} language="zh-CN" onOpenSettings={() => {}} />);
    expect(screen.getByRole('alert').textContent).toContain('连接不上');
    expect(failing.request).toHaveBeenCalledTimes(1);
  });

  it("distinguishes disabled logs from an empty enabled page", async () => {
    const api = fakeApi(() => ({ revision: 7, page: { log_epoch: 3, storage, enabled: false, total: 1, entries: [entry], next_cursor: null } }));
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getByText(/query log is off/)).toBeTruthy());
    expect(screen.getAllByText('example.test').length).toBeGreaterThan(0);
  });

  it.each([[0, 'NETWORK'], [200, 'BAD_RESPONSE'], [503, 'STORAGE_UNAVAILABLE']])("marks clear failure (%s %s) as unknown without repeating the mutation", async (status, code) => {
    let clearCalls = 0;
    let epoch = 3;
    const api = fakeApi((path) => {
      if (path === "query-log/clear") { clearCalls += 1; throw new ApiError(status as number, code as string, "Connection lost"); }
      return { revision: 9, page: { log_epoch: epoch, storage, enabled: true, total: 1, entries: [entry], next_cursor: null } };
    });
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getByRole("button", { name: /Clear log/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Clear log/ }));
    fireEvent.click(screen.getByRole("button", { name: "Clear log" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("couldn't confirm"));
    expect(clearCalls).toBe(1);
    expect((screen.getByRole('button', { name: 'Clear log…' }) as HTMLButtonElement).disabled).toBe(true);
    epoch = 4;
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getByText('Query log cleared.')).toBeTruthy());
    expect(clearCalls).toBe(1);
  });
});
