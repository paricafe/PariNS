// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ApiError } from "../../../session/client";
import type { ApiClient } from "../../../session/client";
import { LogsPage } from "../LogsPage";
import { OverviewPage } from "../OverviewPage";
import { averageLatencyMillis, cacheHitRate, histogramRows, isolatedPoints, trendSeries, type HistorySample, type StatusView } from "../metrics";
import type { LogEntry, LogListResponse } from "../logs";

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
  incoming_ecs: null, outgoing_ecs: "192.0.2.0/24", edns: true, dnssec_ok: false,
  checking_disabled: false, recursion_desired: true,
  answer: [{ name: "example.test", record_type: "A", ttl: 60, data: "192.0.2.1" }], answer_truncated: false,
};

describe("observability data math", () => {
  it("uses cache lookups and request timer counts as denominators", () => {
    expect(cacheHitRate(status.metrics?.counters)).toBe(30);
    expect(cacheHitRate({ cache_hits: 0, cache_misses: 0 })).toBeNull();
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
});

describe("OverviewPage", () => {
  it("renders cumulative metrics separately from time-window trend", async () => {
    const api = fakeApi((path) => path === "status" ? status : { interval_seconds: 60, retention_seconds: 86400, samples: [] });
    const openDns = vi.fn();
    render(<OverviewPage api={api} language="en" onOpenDns={openDns} />);
    await waitFor(() => expect(screen.getByText("100")).toBeTruthy());
    expect(screen.getByText("30.0%")).toBeTruthy();
    expect(screen.getByText("2.50 ms")).toBeTruthy();
    expect(screen.getAllByText("5").length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("button", { name: "6 hours" }));
    expect(screen.getByRole("button", { name: "6 hours" }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: /DNS settings/ }));
    expect(openDns).toHaveBeenCalledOnce();
  });

  it("retains last good status and offers refresh on a later read failure", async () => {
    let reads = 0;
    const api = fakeApi((path) => {
      if (path === "stats") return { interval_seconds: 60, retention_seconds: 86400, samples: [] };
      reads += 1;
      if (reads > 1) throw new Error("offline");
      return status;
    });
    render(<OverviewPage api={api} language="en" onOpenDns={() => {}} />);
    await waitFor(() => expect(screen.getByText("100")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Refresh status" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("temporarily unavailable"));
    expect(screen.getByText("100")).toBeTruthy();
  });
});

describe("LogsPage", () => {
  it("uses server filters/cursor, keeps a detail snapshot, and clears with the listed revision", async () => {
    const requests: { path: string; body: unknown }[] = [];
    let entries = [entry];
    const api = fakeApi((path, body) => {
      requests.push({ path, body });
      if (path === "query-log/clear") { entries = []; return { removed: 1, revision: 7 }; }
      const response: LogListResponse = { revision: 7, page: { enabled: true, total: entries.length, entries, next_cursor: entries.length ? 20 : null } };
      return response;
    });
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getAllByText("example.test").length).toBeGreaterThan(0));
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: " example.test " } });
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "success" } });
    fireEvent.click(screen.getByRole("button", { name: "Filter" }));
    await waitFor(() => expect(requests.some((request) => request.path === "query-log/list"
      && (request.body as { search: string }).search === "example.test"
      && (request.body as { status: string }).status === "success")).toBe(true));
    fireEvent.click(screen.getByRole("button", { name: /Older queries/ }));
    await waitFor(() => expect(requests.some((request) => (request.body as { before_id: number })?.before_id === 20)).toBe(true));
    fireEvent.click(screen.getAllByRole("button", { name: /Details · example.test/ })[0]);
    expect(screen.getByText(/example.test 60 A 192.0.2.1/)).toBeTruthy();
    expect(screen.getByText("192.0.2.0/24")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Close details" }));
    fireEvent.click(screen.getByRole("button", { name: /Clear log/ }));
    fireEvent.click(screen.getByRole("button", { name: "Clear log" }));
    await waitFor(() => expect(requests.some((request) => request.path === "query-log/clear"
      && (request.body as { revision: number }).revision === 7)).toBe(true));
    expect(screen.getByText("Query log cleared.")).toBeTruthy();
  });

  it("distinguishes disabled logs from an empty enabled page", async () => {
    const api = fakeApi(() => ({ revision: 7, page: { enabled: false, total: 0, entries: [], next_cursor: null } }));
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getByText(/Turn on query logging/)).toBeTruthy());
  });

  it("marks a disconnected clear as unknown without repeating the mutation", async () => {
    let clearCalls = 0;
    const api = fakeApi((path) => {
      if (path === "query-log/clear") { clearCalls += 1; throw new ApiError(0, "NETWORK", "Connection lost"); }
      return { revision: 9, page: { enabled: true, total: 1, entries: [entry], next_cursor: null } };
    });
    render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
    await waitFor(() => expect(screen.getByRole("button", { name: /Clear log/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Clear log/ }));
    fireEvent.click(screen.getByRole("button", { name: "Clear log" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("not confirmed"));
    expect(clearCalls).toBe(1);
  });
});
