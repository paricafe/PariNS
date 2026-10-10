// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ApiError } from '../../session/client';
import type { CertificateStatus } from './CertificateTools';
import { SecurityRuntime } from './SecurityRuntime';

const mocks = vi.hoisted(() => { const request = vi.fn(); return { request, api: { request } }; });
vi.mock('../../session/context', () => ({ useSession: () => ({ api: mocks.api }) }));
vi.mock('../../config/context', () => ({ useConfig: () => ({ dirty: true, busy: false, locked: false, draft: { revision: 1 } }) }));
const base: CertificateStatus = { certificate_generation: 2, roles: [{ role: 'doh', leaf_sha256: 'a'.repeat(64), not_before_ms: 1700000000000, not_after_ms: 1900000000000 }], last_reload: null };
beforeEach(() => { mocks.request.mockReset(); });
afterEach(cleanup);

describe('security runtime status and certificate reload ownership', () => {
  it('uses the live revision, leaves drafts alone and shows unchanged identities bilingually', async () => {
    mocks.request.mockImplementation(async (path) => path === 'status' ? { revision: 7, certificates: base, running: true, doh_listen: '127.0.0.1:8443', doh_http3: false } : { outcome: 'unchanged', revision: 7, certificate_generation: 2, certificates: base });
    const view = render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(screen.getByText('Certificate generation 2')).toBeTruthy());
    expect(screen.getByText('127.0.0.1:8443 · /dns-query')).toBeTruthy();
    expect(screen.getByText(/You have unsaved changes/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/contents are unchanged/)).toBeTruthy());
    expect(mocks.request).toHaveBeenCalledWith('certificates/reload', 'POST', { revision: 7 });
    expect(mocks.request.mock.calls.every(([path]) => path === 'status' || path === 'certificates/reload')).toBe(true);
    view.rerender(<SecurityRuntime language="zh-CN" />);
    expect(screen.getByText('证书代次 2')).toBeTruthy();
    expect(screen.getByText('a'.repeat(64))).toBeTruthy();
  });

  it('feeds both panels from exactly one /status request per 5-second poll', async () => {
    vi.useFakeTimers();
    try {
      mocks.request.mockResolvedValue({ revision: 7, certificates: base, running: true, doh_listen: '127.0.0.1:8443', doh_http3: false });
      const statusReads = () => mocks.request.mock.calls.filter(([path]) => path === 'status').length;
      render(<SecurityRuntime language="en" />);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(screen.getByText('Certificate generation 2')).toBeTruthy();
      expect(screen.getByText('127.0.0.1:8443 · /dns-query')).toBeTruthy();
      expect(statusReads()).toBe(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
      expect(statusReads()).toBe(2);
    } finally { vi.useRealTimers(); }
  });

  it.each([[0, 'NETWORK'], [200, 'BAD_RESPONSE']])('keeps unknown %s/%s locked until a read and explicit acknowledgment, without replay', async (status, code) => {
    let current = base;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { revision: 7, certificates: current };
      throw new ApiError(status as number, code as string, 'response missing');
    });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(screen.getByText('Certificate generation 2')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/reload result is unconfirmed/)).toBeTruthy());
    expect((screen.getByRole('button', { name: 'Reload certificates' }) as HTMLButtonElement).disabled).toBe(true);
    current = { ...base, certificate_generation: 3, last_reload: { attempt_id: 8, source: 'signal', started_at_ms: 1800000000000, completed_at_ms: 1800000000010, outcome: 'applied', error_code: null } };
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getByText('Certificate generation 3')).toBeTruthy());
    expect(screen.getByText(/may include another operator/)).toBeTruthy();
    expect((screen.getByRole('button', { name: 'Reload certificates' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'I have reviewed the current state' }));
    expect((screen.getByRole('button', { name: 'Reload certificates' }) as HTMLButtonElement).disabled).toBe(false);
    expect(mocks.request.mock.calls.filter(([path]) => path === 'certificates/reload')).toHaveLength(1);
  });

  it('keeps the active summary after candidate rejection', async () => {
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { revision: 7, certificates: base };
      throw new ApiError(422, 'CERTIFICATE_NAME_MISMATCH', 'invalid SAN');
    });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(screen.getByText('Certificate generation 2')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/previous certificates remain active/)).toBeTruthy());
    expect(screen.getByRole('alert').textContent).toContain('SAN');
    expect(screen.getByText('a'.repeat(64))).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Check result' })).toBeNull();
  });

  it('does not restore certificate contents after an authenticated page unmount', async () => {
    let finish!: (value: unknown) => void;
    mocks.request.mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
    const view = render(<SecurityRuntime language="en" />);
    view.unmount();
    await act(async () => { finish({ revision: 7, certificates: base }); });
    expect(screen.queryByText('a'.repeat(64))).toBeNull();
  });
});
