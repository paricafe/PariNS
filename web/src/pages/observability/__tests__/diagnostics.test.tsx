// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { CacheDiagnosticsPanel } from '../CacheDiagnosticsPanel';
import { StorageStatusPanel } from '../StorageStatusPanel';
import { TransportDiagnostics, UpstreamAttempts } from '../TransportDiagnostics';
import { decisionLabel } from '../diagnostics';
import { storage } from './fixtures';

afterEach(cleanup);
describe('server-owned diagnostic presentation', () => {
  it('distinguishes exact cache decisions and retains unknown reason codes', () => {
    expect(decisionLabel('store', { outcome: 'superseded_only', reason: 'ttl_zero' }, 'en')).toBe('Only cleared the older answer · TTL is 0');
    expect(decisionLabel('lookup', { outcome: 'bypass', reason: 'unsupported_edns' }, 'zh-CN')).toContain('EDNS');
    expect(decisionLabel('store', { outcome: 'skipped', reason: 'future_code' }, 'en')).toContain('future_code');
    expect(decisionLabel('lookup', null, 'en')).toBe('Not recorded');
  });
  it('renders cache-generation counts without mixing persistent totals', () => {
    const value = { scope: 'cache_generation', since_ms: 1700000000000, lookup: { fresh: 12, stale: 2, miss: 4, bypass: 5, disabled: 0, reasons: { unsupported_edns: 5 } }, store: { admitted: 3, replaced: 1, skipped: 4, superseded_only: 2, reasons: { ttl_zero: 2 } } };
    render(<CacheDiagnosticsPanel value={value} language="en" />);
    expect(screen.getByText(/Since the cache was rebuilt at/)).toBeTruthy();
    expect(screen.getByText('12')).toBeTruthy();
    expect(screen.getByText('TTL is 0')).toBeTruthy();
  });
  it('leads with key storage facts, keeps nullable filesystem readings and raw errors, and moves counters into details', () => {
    const current = { ...storage, dropped_logs: 2, last_dropped_log_at_ms: 1700000000000, query_log_coverage: { earliest_time_ms: 1700000000000, latest_time_ms: 1700000026000, span_ms: 26000 }, filesystem: { ...storage.filesystem, total_bytes: null, available_bytes: null, error: 'filesystem_unavailable', stale: true, low_space: true } };
    const view = render(<StorageStatusPanel status={current} language="en" />);
    // An earlier day shows the date as well as the time.
    expect(screen.getByText(/^2 records could not be saved · last query log drop: \d+\/\d+\/\d+, /)).toBeTruthy();
    expect(screen.getByText(/last readings/)).toBeTruthy();
    expect(screen.getByText('filesystem_unavailable')).toBeTruthy();
    expect(screen.getAllByRole('alert').some((alert) => alert.textContent?.includes('Disk space is low'))).toBe(true);
    expect(screen.getByRole('button', { name: 'Detailed counts' }).getAttribute('aria-expanded')).toBe('false');
    view.rerender(<StorageStatusPanel status={current} language="zh-CN" />);
    expect(screen.getByText('磁盘空间')).toBeTruthy();
    view.rerender(<StorageStatusPanel status={{ ...current, last_dropped_log_at_ms: Date.now() }} language="en" />);
    expect(screen.getByText(/^2 records could not be saved · last query log drop: \d{1,2}:\d{2}\s?[AP]M$/)).toBeTruthy();
  });
  it('says in words that storage settings are pending, never as revision numbers', () => {
    // A fixed commit time keeps the rendered clock from containing the revision digits.
    render(<StorageStatusPanel status={{ ...storage, last_commit_at_ms: 1700000000000, configured_revision: 18, applied_revision: 17 }} language="en" />);
    expect(screen.getByText("Some storage settings haven't taken effect yet; they're being applied.")).toBeTruthy();
    expect(screen.queryByText(/18|revision|version/i)).toBeNull();
  });
  it('renders H3 failure and H2 success as separate attempts, with omitted count', () => {
    render(<UpstreamAttempts language="en" trace={{ omitted: 3, attempts: [
      { pool_generation: 2, slot: 0, protocol: 'doh3', stage: 'quic_handshake', outcome: 'failed', reason: 'deadline', code: 256, elapsed_ms: 123 },
      { pool_generation: 2, slot: 0, protocol: 'doh2', stage: 'validate', outcome: 'succeeded', reason: null, code: null, elapsed_ms: 42 },
    ] }} />);
    expect(screen.getByText(/DoH \/ HTTP\/3 · QUIC handshake · Failed · Timed out/)).toBeTruthy();
    expect(screen.getByText(/DoH \/ HTTP\/2 · Validating the response · Succeeded/)).toBeTruthy();
    expect(screen.getByText('256')).toBeTruthy();
    expect(screen.getByText('3 more requests not shown.')).toBeTruthy();
    // Slots are numbered from 1 and carry no internal generation tag.
    expect(screen.getAllByText(/^Upstream 1 · /)).toHaveLength(2);
    expect(screen.queryByText(/#2/)).toBeNull();
  });
  it('keeps admission, handoff and actual protocol counts distinct', () => {
    const protocol = { handshake_inflight: 1, handshake_established: 9, stream_inflight: 2, stream_response_handed_to_transport: 8, admission_source_rejected: 3, stream_peer_cancelled: 4 };
    render(<TransportDiagnostics language="en" recursion={null} upstreams={{ scope: 'pool_generation', pool_generation: 5, since_ms: 1700000000000, slots: [{ slot: 0, counts: [{ protocol: null, stage: 'wait', outcome: 'cancelled', reason: 'caller_cancelled', count: 7 }] }], endpoints: [{ slot: 0, label: 'tls://dns.example', address: '192.0.2.53:853' }] }} quic={{ scope: 'process', doq: protocol, doh3: protocol }} />);
    // Each upstream is named by its configured label and the address it was reached at.
    expect(screen.getByText('tls://dns.example')).toBeTruthy();
    expect(screen.getByText(/reached at 192\.0\.2\.53:853/)).toBeTruthy();
    expect(screen.queryByText('Upstream 1')).toBeNull();
    expect(screen.getByText(/Protocol not chosen yet/)).toBeTruthy();
    expect(screen.getByText(/doesn't mean the client received it/)).toBeTruthy();
    expect(screen.getAllByText('Rejected: per-client limit')).toHaveLength(2);
    expect(screen.getAllByText('Requests cancelled by client')).toHaveLength(2);
  });
  it('renders recursion counts in a fixed order, keeps unknown counters, and tolerates missing upstream diagnostics', () => {
    const quic = { scope: 'process', doq: {}, doh3: {} };
    const recursion = { scope: 'recursion_generation', resolutions: { answered: 7, bogus: 1, deadline: 1, failed: 0, negative: 2, shutdown: 0, started: 10 }, failures: { budget: 0, other: 0, referral: 2, timeout: 1 }, dnssec: { bogus: 1, indeterminate: 0, insecure: 6, secure: 3 }, ecs_scope: { absent: 4, not_sent: 5, scoped: 1 }, outbound: { inflight: 3, rate_wait_ms: 1234, rate_waits: 2, refused_after_shutdown: 0, sent: 42, future_counter: 9 } };
    const view = render(<TransportDiagnostics language="en" upstreams={null} recursion={recursion} quic={quic} />);
    expect(screen.queryByText('Upstream request diagnostics')).toBeNull();
    expect(screen.getByText('Recursive resolution diagnostics')).toBeTruthy();
    // Internal scope tags are not shown.
    expect(screen.queryByText('recursion_generation')).toBeNull();
    const resolutions = screen.getByRole('heading', { name: 'Resolutions' }).parentElement!;
    expect([...resolutions.querySelectorAll('dt')].map((term) => term.textContent)).toEqual(['Started', 'Answered', 'Name or record does not exist', 'Failed DNSSEC validation', 'Failed', 'Cancelled at the query timeout', 'Ended by shutdown']);
    expect(screen.getByText('Referral not followed').nextElementSibling?.textContent).toBe('2');
    expect(screen.getByText('Secure').nextElementSibling?.textContent).toBe('3');
    expect(screen.getByText('ECS not sent').nextElementSibling?.textContent).toBe('5');
    expect(screen.getByText('Awaiting a response').nextElementSibling?.textContent).toBe('3');
    expect(screen.getByText('Total rate-limit wait (ms)').nextElementSibling?.textContent).toBe('1,234');
    expect(screen.getByText('outbound_future_counter').nextElementSibling?.textContent).toBe('9');
    view.rerender(<TransportDiagnostics language="zh-CN" upstreams={null} recursion={null} quic={quic} />);
    expect(screen.queryByText('递归解析诊断')).toBeNull();
    expect(screen.getByText('QUIC 连接诊断')).toBeTruthy();
  });
});
