// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ApiClient } from "../../../session/client";
import { LogsPage } from "../LogsPage";
import { LiveLogClient, liveUrl, PING_MS, type LiveHandlers } from "../live";
import type { LogEntry, LogListResponse } from "../logs";
import { storage } from './fixtures';

/** Minimal server side of the live protocol; the page only sees the WebSocket API. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeSocket[] = [];
  readyState = 0;
  sent: Record<string, unknown>[] = [];
  closedBy: number | null = null;
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: ((event: { code: number }) => void) | null = null;
  constructor(readonly url: string) { FakeSocket.instances.push(this); }
  /** Like the server, every subscribe is answered with the current state first. */
  state: Record<string, unknown> = { type: 'state', log: 'ok', log_epoch: 3 };
  answerSubscribes = true;
  send(data: string) {
    const message = JSON.parse(data);
    this.sent.push(message);
    if (message.type === 'subscribe' && this.answerSubscribes) this.push(this.state);
  }
  close(code = 1000) { this.readyState = 3; this.closedBy = code; }
  open() { this.readyState = 1; this.onopen?.(); }
  push(message: unknown) {
    if ((message as { type?: string }).type === 'state') this.state = message as Record<string, unknown>;
    this.onmessage?.({ data: JSON.stringify(message) });
  }
  serverClose(code: number) { this.readyState = 3; this.onclose?.({ code }); }
  ofType(type: string) { return this.sent.filter((message) => message.type === type); }
}
const latest = () => FakeSocket.instances[FakeSocket.instances.length - 1];

const BINDING = 'binding-secret-value';
const entry: LogEntry = {
  id: 42, time_ms: Date.now(), client: "192.0.2.10", name: "example.test", qtype: "A", transport: "udp",
  status: "success", rcode: "NoError", duration_ms: 2.5, cache: "fresh", upstream: "udp://192.0.2.53:53",
  cache_lookup: { outcome: 'fresh', reason: null }, cache_store: null, cache_scope: null, upstream_relation: null,
  upstream_attempts: null, upstream_attempts_omitted: null, failure_stage: null, failure_reason: null,
  incoming_ecs: null, outgoing_ecs: null, edns: true, dnssec_ok: false, checking_disabled: false, recursion_desired: true,
  answer: [{ name: "example.test", record_type: "A", ttl: 60, data: "192.0.2.1" }], answer_truncated: false,
};
const row = (id: number, name: string): LogEntry => ({ ...entry, id, name });

beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute("open", ""); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute("open"); };
  Element.prototype.scrollIntoView = function scrollIntoView() {};
  window.scrollBy = () => {};
});
beforeEach(() => { FakeSocket.instances = []; vi.stubGlobal('WebSocket', FakeSocket); });
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); setHidden(false); });

function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => hidden ? 'hidden' : 'visible' });
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
}

function handlers(): LiveHandlers & { log: string[] } {
  const log: string[] = [];
  return { log, entries: vi.fn(), state: vi.fn(), status: (status) => { log.push(status); } };
}

describe('LiveLogClient', () => {
  it('authenticates with the first message and never puts the binding in the URL', () => {
    vi.useFakeTimers();
    const client = new LiveLogClient(() => BINDING, handlers(), { interval: 3, cursor: null, filter: { search: null, status: null, cache: null } });
    client.start({ interval: 3, cursor: 42, filter: { search: 'a', status: null, cache: null } });
    const socket = latest();
    expect(socket.url).toBe(`ws://${window.location.host}/api/query-log/live`);
    expect(socket.url).not.toContain(BINDING);
    socket.open();
    expect(socket.sent[0]).toEqual({ type: 'auth', binding: BINDING });
    expect(socket.sent[1]).toEqual({ type: 'subscribe', interval: 3, cursor: 42, filter: { search: 'a', status: null, cache: null } });
    expect(socket.sent[2]).toEqual({ type: 'refresh' });
    vi.advanceTimersByTime(PING_MS);
    expect(socket.ofType('ping')).toHaveLength(1);
    expect(liveUrl({ protocol: 'https:', host: 'dns.example:8443' })).toBe('wss://dns.example:8443/api/query-log/live');
    client.stop();
    expect(socket.closedBy).toBe(1000);
  });

  it('drops entries sent before the server answers a new subscription', () => {
    const events = handlers();
    const received: number[] = [];
    events.entries = (entries) => { received.push(...entries.map((e) => e.id)); };
    const client = new LiveLogClient(() => BINDING, events, { interval: 3, cursor: 40, filter: { search: null, status: null, cache: null } });
    client.start({ interval: 3, cursor: 40, filter: { search: null, status: null, cache: null } });
    const socket = latest();
    socket.open();
    socket.answerSubscribes = false;
    client.update({ interval: 3, cursor: 40, filter: { search: 'b.test', status: null, cache: null } });
    // Still the old filter's batch: it must not be inserted under the new filter.
    socket.push({ type: 'entries', entries: [row(43, 'a.test')], cursor: 43, more: false });
    expect(received).toEqual([]);
    socket.push({ type: 'state', log: 'ok', log_epoch: 3 });
    socket.push({ type: 'entries', entries: [row(44, 'b.test')], cursor: 44, more: false });
    expect(received).toEqual([44]);
    client.stop();
  });

  it('reconnects with bounded backoff and stops on auth or limit closes', () => {
    vi.useFakeTimers();
    const events = handlers();
    const client = new LiveLogClient(() => BINDING, events, { interval: null, cursor: null, filter: { search: null, status: null, cache: null } });
    client.start({ interval: null, cursor: null, filter: { search: null, status: null, cache: null } });
    latest().open();
    expect(latest().ofType('refresh')).toHaveLength(0); // manual mode waits for an explicit refresh
    latest().push({ type: 'entries', entries: [row(43, 'a.test')], cursor: 43, more: false });
    latest().serverClose(1001);
    expect(events.log.at(-1)).toBe('reconnecting');
    vi.advanceTimersByTime(999);
    expect(FakeSocket.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeSocket.instances).toHaveLength(2);
    // This connection fails before the server answers anything, so backoff grows.
    latest().answerSubscribes = false;
    latest().open();
    expect(latest().ofType('subscribe')[0]).toMatchObject({ cursor: 43 });
    latest().serverClose(1006);
    vi.advanceTimersByTime(1_999);
    expect(FakeSocket.instances).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(FakeSocket.instances).toHaveLength(3);
    for (let attempt = 0; attempt < 8; attempt += 1) { latest().serverClose(1006); vi.advanceTimersByTime(30_000); }
    expect(FakeSocket.instances).toHaveLength(11); // capped at 30 s, still retrying
    latest().serverClose(4401);
    expect(events.log.at(-1)).toBe('unauthorized');
    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.instances).toHaveLength(11);
  });

  it('does not connect without an in-memory binding', () => {
    const events = handlers();
    new LiveLogClient(() => null, events, { interval: 3, cursor: null, filter: { search: null, status: null, cache: null } })
      .start({ interval: 3, cursor: null, filter: { search: null, status: null, cache: null } });
    expect(FakeSocket.instances).toHaveLength(0);
    expect(events.log).toEqual(['stopped']);
  });
});

function page(entries: LogEntry[], epoch = 3, next: number | null = null): LogListResponse {
  return { revision: 7, page: { log_epoch: epoch, storage, enabled: true, total: entries.length, entries, next_cursor: next } };
}

function liveApi(list: (body: { before_id: number | null; status: string | null }) => LogListResponse) {
  return {
    currentBinding: BINDING,
    request: vi.fn(async (path: string, _method?: string, body?: unknown) => {
      if (path === 'query-log/list') return list(body as { before_id: number | null; status: string | null });
      throw new Error(`unexpected ${path}`);
    }),
  } as unknown as ApiClient & { request: ReturnType<typeof vi.fn> };
}

async function mount(list: Parameters<typeof liveApi>[0] = () => page([entry], 3, 20)) {
  const api = liveApi(list);
  render(<LogsPage api={api} language="en" onOpenSettings={() => {}} />);
  await waitFor(() => expect(screen.getAllByText('example.test').length).toBeGreaterThan(0));
  await waitFor(() => expect(FakeSocket.instances.length).toBeGreaterThan(0));
  const socket = latest();
  act(() => { socket.open(); socket.push({ type: 'state', log: 'ok', log_epoch: 3 }); });
  return { api, socket };
}
const tableControl = (name: string) => screen.getAllByRole('button', { name })[0] as HTMLButtonElement;
const tableRow = (name: string) => tableControl(name).closest('tr')!;

describe('LogsPage live updates', () => {
  it('subscribes with the default 3 s interval, resubscribes on change and inserts batches at the head', async () => {
    const { api, socket } = await mount();
    expect(socket.sent[0]).toEqual({ type: 'auth', binding: BINDING });
    expect(socket.ofType('subscribe')[0]).toEqual({ type: 'subscribe', interval: 3, cursor: 42, filter: { search: null, status: null, cache: null } });
    expect(screen.getByRole('radio', { name: '3 s' }).getAttribute('aria-checked')).toBe('true');
    fireEvent.click(screen.getByRole('radio', { name: '1 s' }));
    expect(socket.ofType('subscribe').at(-1)).toMatchObject({ interval: 1, cursor: 42 });

    act(() => socket.push({ type: 'entries', entries: [row(44, 'b.test'), row(43, 'a.test')], cursor: 44, more: false }));
    expect(tableRow('b.test').hasAttribute('data-enter')).toBe(true);
    expect(tableRow('example.test').hasAttribute('data-enter')).toBe(false);
    const names = [...document.querySelectorAll('tr.log-row .log-expand')].map((node) => node.textContent);
    expect(names).toEqual(['b.test', 'a.test', 'example.test']);
    expect(api.request).toHaveBeenCalledTimes(1);
  });

  it('updates only on Refresh in manual mode', async () => {
    const { api, socket } = await mount();
    fireEvent.click(screen.getByRole('radio', { name: 'Manual' }));
    expect(socket.ofType('subscribe').at(-1)).toMatchObject({ interval: null });
    expect(socket.ofType('refresh')).toHaveLength(1); // the catch-up sent on connect in auto mode
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    expect(socket.ofType('refresh')).toHaveLength(2);
    act(() => socket.push({ type: 'entries', entries: [row(43, 'manual.test')], cursor: 43, more: false }));
    expect(screen.getAllByText('manual.test').length).toBeGreaterThan(0);
    expect(api.request).toHaveBeenCalledTimes(1);
  });

  it('pauses while a row is expanded and shows a pill that inserts on click', async () => {
    const { socket } = await mount();
    fireEvent.pointerEnter(tableRow('example.test'), { pointerType: 'mouse' });
    await waitFor(() => expect(tableControl('example.test').getAttribute('aria-expanded')).toBe('true'));
    act(() => socket.push({ type: 'entries', entries: [row(44, 'b.test'), row(43, 'a.test')], cursor: 44, more: false }));
    expect(screen.queryByText('b.test')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '2 new queries' }));
    expect(screen.getAllByText('b.test').length).toBeGreaterThan(0);
    expect(screen.queryByRole('button', { name: /new queries/ })).toBeNull();
  });

  it('pauses under a filter and on older pages; the pill returns to the head', async () => {
    const { api, socket } = await mount((body) => body.before_id ? page([row(10, 'older.test')], 3) : page([entry], 3, 20));
    fireEvent.click(screen.getByRole('button', { name: /Older queries/ }));
    await waitFor(() => expect(screen.getAllByText('older.test').length).toBeGreaterThan(0));
    act(() => socket.push({ type: 'entries', entries: [row(43, 'a.test')], cursor: 43, more: false }));
    expect(screen.queryByText('a.test')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '1 new queries' }));
    await waitFor(() => expect(screen.getAllByText('example.test').length).toBeGreaterThan(0));
    expect(api.request).toHaveBeenLastCalledWith('query-log/list', 'POST', expect.objectContaining({ before_id: null }));

    fireEvent.click(screen.getByRole('tab', { name: 'Old-answer fallback' }));
    await waitFor(() => expect(socket.ofType('subscribe').at(-1)).toMatchObject({ filter: { search: null, status: null, cache: 'stale' } }));
    act(() => socket.push({ type: 'entries', entries: [row(45, 'stale.test')], cursor: 45, more: false }));
    expect(screen.queryByText('stale.test')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '1 new queries' }));
    expect(screen.getAllByText('stale.test').length).toBeGreaterThan(0);
  });

  it('closes while the tab is hidden and catches up as soon as it is visible', async () => {
    const { socket } = await mount();
    act(() => { setHidden(true); document.dispatchEvent(new Event('visibilitychange')); });
    expect(socket.closedBy).toBe(1000);
    expect(FakeSocket.instances).toHaveLength(1);
    act(() => { setHidden(false); document.dispatchEvent(new Event('visibilitychange')); });
    const next = latest();
    expect(next).not.toBe(socket);
    act(() => next.open());
    expect(next.sent.map((message) => message.type)).toEqual(['auth', 'subscribe', 'refresh']);
    expect(next.sent[1]).toMatchObject({ cursor: 42 });
  });

  it('resets the list when the log epoch changes', async () => {
    let epoch = 3;
    const { api, socket } = await mount(() => epoch === 3 ? page([entry], 3) : page([row(1, 'after-clear.test')], 4));
    epoch = 4;
    act(() => socket.push({ type: 'state', log: 'ok', log_epoch: 4 }));
    await waitFor(() => expect(screen.getAllByText('after-clear.test').length).toBeGreaterThan(0));
    expect(screen.queryByText('example.test')).toBeNull();
    expect(api.request).toHaveBeenCalledTimes(2);
    await waitFor(() => expect(socket.ofType('subscribe').at(-1)).toMatchObject({ cursor: 1 }));
  });

  it('stops on an auth close and lets an ordinary request report the session', async () => {
    const { api, socket } = await mount();
    act(() => socket.serverClose(4403));
    await waitFor(() => expect(api.request).toHaveBeenCalledTimes(2));
    expect(screen.getByText(/Live updates stopped/)).toBeTruthy();
    expect(FakeSocket.instances).toHaveLength(1);
  });
});

describe('LogsPage inline details', () => {
  it('expands after a hover delay, collapses on leave and keeps one row open', async () => {
    const { socket } = await mount(() => page([entry, row(41, 'second.test')]));
    act(() => socket.push({ type: 'entries', entries: [], cursor: 42, more: false }));
    fireEvent.pointerEnter(tableRow('example.test'), { pointerType: 'mouse' });
    expect(tableControl('example.test').getAttribute('aria-expanded')).toBe('false');
    await waitFor(() => expect(tableControl('example.test').getAttribute('aria-expanded')).toBe('true'));
    expect(screen.getAllByText(/example.test 60 A 192.0.2.1/).length).toBeGreaterThan(0);
    // Moving into the details keeps them open.
    fireEvent.pointerLeave(tableRow('example.test'), { pointerType: 'mouse' });
    fireEvent.pointerEnter(document.querySelector('.log-expansion-row')!, { pointerType: 'mouse' });
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(tableControl('example.test').getAttribute('aria-expanded')).toBe('true');
    fireEvent.pointerEnter(tableRow('second.test'), { pointerType: 'mouse' });
    await waitFor(() => expect(tableControl('second.test').getAttribute('aria-expanded')).toBe('true'));
    expect(tableControl('example.test').getAttribute('aria-expanded')).toBe('false');
    expect(document.querySelectorAll('.log-expansion-row')).toHaveLength(1);
    fireEvent.pointerLeave(tableRow('second.test'), { pointerType: 'mouse' });
    await waitFor(() => expect(document.querySelector('.log-expansion-row')).toBeNull());
  });

  it('expands on keyboard focus, collapses on Escape, and toggles on touch', async () => {
    await mount();
    const control = tableControl('example.test');
    fireEvent.focus(control);
    expect(control.getAttribute('aria-expanded')).toBe('true');
    expect(control.getAttribute('aria-controls')).toBe(document.querySelector('.log-expansion-row .log-expansion')!.id);
    fireEvent.keyDown(control, { key: 'Escape' });
    expect(control.getAttribute('aria-expanded')).toBe('false');

    fireEvent.pointerEnter(tableRow('example.test'), { pointerType: 'touch' });
    fireEvent.pointerDown(control, { pointerType: 'touch' });
    fireEvent.focus(control);
    fireEvent.click(control, { detail: 1 });
    expect(control.getAttribute('aria-expanded')).toBe('true');
    fireEvent.pointerDown(control, { pointerType: 'touch' });
    fireEvent.click(control, { detail: 1 });
    expect(control.getAttribute('aria-expanded')).toBe('false');
  });
});
