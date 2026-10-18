import type { LogEntry } from './logs';

/** Seconds between pushes; null is manual mode (the server pushes only on refresh). */
export type LiveInterval = 1 | 3 | 5 | 10 | null;
export interface LiveFilter { search: string | null; status: string | null; cache: string | null }
export interface LiveSubscription { interval: LiveInterval; cursor: number | null; filter: LiveFilter }
export interface LiveLogState { log: 'ok' | 'disabled' | 'unavailable' | 'checking'; log_epoch: number }
/**
 * `unauthorized` (4401/4403) is terminal: the client stops and the page decides what
 * comes next. Every other close reconnects with bounded backoff.
 */
export type LiveStatus = 'connecting' | 'open' | 'reconnecting' | 'unauthorized' | 'stopped';

export interface LiveHandlers {
  entries(entries: LogEntry[], more: boolean): void;
  state(state: LiveLogState): void;
  status(status: LiveStatus): void;
}

export const PING_MS = 10_000;
const BACKOFF_MS = [1_000, 2_000, 5_000, 10_000, 30_000] as const;
const logStates = new Set(['ok', 'disabled', 'unavailable', 'checking']);

export function liveUrl(location: Pick<Location, 'protocol' | 'host'> = window.location): string {
  return `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/query-log/live`;
}

/**
 * One live query-log WebSocket. The session binding is read from the API client at
 * each connect and sent only as the first message; it never enters the URL.
 */
export class LiveLogClient {
  private socket: WebSocket | null = null;
  private ping: ReturnType<typeof setInterval> | null = null;
  private retry: ReturnType<typeof setTimeout> | null = null;
  private failures = 0;
  private unanswered = 0;
  private running = false;
  private subscription: LiveSubscription;

  constructor(private readonly binding: () => string | null | undefined, private readonly handlers: LiveHandlers, initial: LiveSubscription) {
    this.subscription = initial;
  }

  /** Connects (or keeps the open connection) with the given settings. */
  start(subscription: LiveSubscription) {
    this.subscription = subscription;
    if (this.running) { this.update(subscription); return; }
    this.running = true;
    this.failures = 0;
    this.connect();
  }

  update(subscription: LiveSubscription) {
    this.subscription = subscription;
    if (this.socket?.readyState === WebSocket.OPEN) this.subscribe();
  }

  /**
   * The server answers every subscribe with a state message before any entries for it.
   * Entries arriving before that answer belong to the previous filter or cursor, and
   * would insert unmatched rows or skip matching ones: they are dropped.
   */
  private subscribe() {
    this.unanswered += 1;
    this.send({ type: 'subscribe', ...this.subscription });
  }

  /** Asks for new entries now; returns false when no connection can carry it. */
  refresh(): boolean {
    if (this.socket?.readyState !== WebSocket.OPEN) return false;
    this.send({ type: 'refresh' });
    return true;
  }

  /** Records the newest id the page holds so a reconnect resumes after it. */
  advance(cursor: number | null) { this.subscription = { ...this.subscription, cursor }; }

  stop() {
    this.running = false;
    this.teardown();
    this.handlers.status('stopped');
  }

  private connect() {
    const binding = this.binding();
    // The session layer owns sign-in; without a binding there is nothing to authenticate with.
    if (!binding) { this.running = false; this.handlers.status('stopped'); return; }
    this.handlers.status(this.failures ? 'reconnecting' : 'connecting');
    const socket = new WebSocket(liveUrl());
    this.socket = socket;
    socket.onopen = () => {
      if (this.socket !== socket) return;
      this.send({ type: 'auth', binding });
      this.unanswered = 0;
      this.subscribe();
      // Resuming after a gap: fetch what was missed now instead of waiting for the interval.
      if (this.subscription.interval !== null) this.send({ type: 'refresh' });
      this.ping = setInterval(() => this.send({ type: 'ping' }), PING_MS);
    };
    socket.onmessage = (event) => { if (this.socket === socket) this.receive(event.data); };
    socket.onclose = (event) => { if (this.socket === socket) this.closed(event.code); };
  }

  private receive(data: unknown) {
    if (typeof data !== 'string') return;
    let message: Record<string, unknown>;
    try { message = JSON.parse(data) as Record<string, unknown>; } catch { return; }
    if (message.type === 'entries' && Array.isArray(message.entries)) {
      this.failures = 0;
      if (this.unanswered > 0) return;
      const entries = message.entries as LogEntry[];
      if (typeof message.cursor === 'number') this.advance(Math.max(message.cursor, this.subscription.cursor ?? 0));
      this.handlers.entries(entries, message.more === true);
    } else if (message.type === 'state' && logStates.has(String(message.log)) && typeof message.log_epoch === 'number') {
      this.failures = 0;
      if (this.unanswered > 0) this.unanswered -= 1;
      this.handlers.state({ log: message.log as LiveLogState['log'], log_epoch: message.log_epoch });
      this.handlers.status('open');
    }
  }

  private closed(code: number) {
    this.teardown();
    if (!this.running) return;
    if (code === 4401 || code === 4403) { this.running = false; this.handlers.status('unauthorized'); return; }
    const delay = BACKOFF_MS[Math.min(this.failures, BACKOFF_MS.length - 1)];
    this.failures += 1;
    this.handlers.status('reconnecting');
    this.retry = setTimeout(() => { this.retry = null; if (this.running) this.connect(); }, delay);
  }

  private teardown() {
    if (this.ping !== null) clearInterval(this.ping);
    if (this.retry !== null) clearTimeout(this.retry);
    this.ping = null; this.retry = null;
    const socket = this.socket;
    this.socket = null;
    if (socket && socket.readyState < WebSocket.CLOSING) socket.close(1000);
  }

  private send(message: unknown) {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message));
  }
}
