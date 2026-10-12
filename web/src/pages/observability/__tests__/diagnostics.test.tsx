// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { CacheDiagnosticsPanel } from '../CacheDiagnosticsPanel';
import { StorageCoverage } from '../StorageCoverage';
import { TransportDiagnostics, UpstreamAttempts } from '../TransportDiagnostics';
import { decisionLabel } from '../diagnostics';
import { storage } from './fixtures';

afterEach(cleanup);
describe('server-owned diagnostic presentation', () => {
  it('distinguishes exact cache decisions and retains unknown reason codes', () => {
    expect(decisionLabel('store', { outcome: 'superseded_only', reason: 'ttl_zero' }, 'en')).toBe('Only cleared the old result · TTL is 0');
    expect(decisionLabel('lookup', { outcome: 'bypass', reason: 'unsupported_edns' }, 'zh-CN')).toContain('EDNS');
    expect(decisionLabel('store', { outcome: 'skipped', reason: 'future_code' }, 'en')).toContain('future_code');
    expect(decisionLabel('lookup', null, 'en')).toBe('Nothing recorded for this step');
  });
  it('renders cache-generation counts without mixing persistent totals', () => {
    const value = { scope: 'cache_generation', since_ms: 1700000000000, lookup: { fresh: 12, stale: 2, miss: 4, bypass: 5, disabled: 0, reasons: { unsupported_edns: 5 } }, store: { admitted: 3, replaced: 1, skipped: 4, superseded_only: 2, reasons: { ttl_zero: 2 } } };
    render(<CacheDiagnosticsPanel value={value} language="en" />);
    expect(screen.getByText(/Since the cache was rebuilt at/)).toBeTruthy();
    expect(screen.getByText('12')).toBeTruthy();
    expect(screen.getByText('TTL is 0')).toBeTruthy();
  });
  it('shows actual coverage and nullable filesystem measurements, including stale/low-space states', () => {
    const current = { ...storage, query_log_coverage: { earliest_time_ms: 1700000000000, latest_time_ms: 1700000026000, span_ms: 26000 }, filesystem: { ...storage.filesystem, total_bytes: null, available_bytes: null, error: 'filesystem_unavailable', stale: true, low_space: true } };
    const view = render(<StorageCoverage status={current} language="en" compact={false} />);
    expect(screen.getByText(/26 seconds/)).toBeTruthy();
    expect(screen.getByText(/only a maximum/)).toBeTruthy();
    expect(screen.getByText(/last readings/)).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('running low on space');
    expect(screen.getAllByText('—')).toHaveLength(2);
    view.rerender(<StorageCoverage status={current} language="zh-CN" compact={false} />);
    expect(screen.getByText('数据所在磁盘')).toBeTruthy();
  });
  it('renders H3 failure and H2 success as separate attempts, with omitted count', () => {
    render(<UpstreamAttempts language="en" trace={{ omitted: 3, attempts: [
      { pool_generation: 2, slot: 0, protocol: 'doh3', stage: 'quic_handshake', outcome: 'failed', reason: 'deadline', code: 256, elapsed_ms: 123 },
      { pool_generation: 2, slot: 0, protocol: 'doh2', stage: 'validate', outcome: 'succeeded', reason: null, code: null, elapsed_ms: 42 },
    ] }} />);
    expect(screen.getByText(/DoH \/ HTTP\/3 · QUIC handshake · Failed · Timed out/)).toBeTruthy();
    expect(screen.getByText(/DoH \/ HTTP\/2 · Checking the reply · Succeeded/)).toBeTruthy();
    expect(screen.getByText('256')).toBeTruthy();
    expect(screen.getByText('3 more requests not shown.')).toBeTruthy();
  });
  it('keeps admission, handoff and actual protocol counts distinct', () => {
    const protocol = { handshake_inflight: 1, handshake_established: 9, stream_inflight: 2, stream_response_handed_to_transport: 8, admission_source_rejected: 3, stream_peer_cancelled: 4 };
    render(<TransportDiagnostics language="en" upstreams={{ scope: 'pool_generation', pool_generation: 5, since_ms: null, slots: [{ slot: 0, counts: [{ protocol: null, stage: 'wait', outcome: 'cancelled', reason: 'caller_cancelled', count: 7 }] }] }} quic={{ scope: 'process', doq: protocol, doh3: protocol }} />);
    expect(screen.getByText(/Protocol not chosen yet/)).toBeTruthy();
    expect(screen.getByText(/doesn't guarantee the client received it/)).toBeTruthy();
    expect(screen.getAllByText('Rejected: per-client limit')).toHaveLength(2);
    expect(screen.getAllByText('Requests cancelled by client')).toHaveLength(2);
  });
});
