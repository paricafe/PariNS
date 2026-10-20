// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ApiError } from '../../session/client';
import type { CertificateStatus } from './CertificateTools';
import { SecurityRuntime } from './SecurityRuntime';
import { DohRuntimeRow } from './DohRuntime';

const mocks = vi.hoisted(() => { const request = vi.fn(); return { request, api: { request } }; });
vi.mock('../../session/context', () => ({ useSession: () => ({ api: mocks.api }) }));
const draft = vi.hoisted(() => ({ value: { revision: 1, fields: {} as Record<string, unknown>, settings: {}, base: {} } }));
vi.mock('../../config/context', () => ({ useConfig: () => ({ dirty: true, busy: false, locked: false, draft: draft.value }) }));
const base: CertificateStatus = { certificate_generation: 2, roles: [{ role: 'doh', leaf_sha256: 'a'.repeat(64), not_before_ms: 1700000000000, not_after_ms: 1900000000000 }], last_reload: null };
beforeEach(() => { mocks.request.mockReset(); draft.value = { revision: 1, fields: { max_inflight: '5' }, settings: {}, base: {} }; });
afterEach(cleanup);

describe('security runtime status and certificate reload ownership', () => {
  const shown = () => screen.getByText(/^Expires/);
  it('reloads the saved paths without a client revision while an unrelated draft is unsaved', async () => {
    mocks.request.mockImplementation(async (path) => path === 'status' ? { certificates: { ...base, last_reload: { attempt_id: 3, source: 'signal', started_at_ms: 1800000000000, completed_at_ms: 1800000000010, outcome: 'applied', error_code: null } }, running: true, doh_listen: '127.0.0.1:8443', doh_http3: false } : { outcome: 'unchanged', certificate_generation: 2, certificates: base });
    const view = render(<SecurityRuntime language="en"><DohRuntimeRow language="en" /></SecurityRuntime>);
    await waitFor(() => expect(shown()).toBeTruthy());
    expect(screen.getByText('127.0.0.1:8443 · /dns-query')).toBeTruthy();
    expect(screen.getByText(/^Last reload: .* · System signal \(SIGHUP\) · Updated$/)).toBeTruthy();
    // The fingerprint stays behind ⓘ.
    expect(screen.queryByText('a'.repeat(64), { exact: false })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Certificate fingerprint (SHA-256)' }));
    expect(screen.getByRole('note').textContent).toContain('a'.repeat(64));
    expect(screen.queryByText(/unsaved changes/)).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/certificates haven't changed/)).toBeTruthy());
    expect(mocks.request).toHaveBeenCalledWith('certificates/reload', 'POST', {});
    view.rerender(<SecurityRuntime language="zh-CN" />);
    expect(screen.getByText('重新加载证书')).toBeTruthy();
  });

  it('mentions the draft only when it edits certificate paths', async () => {
    draft.value = { revision: 1, fields: { 'doh.cert_file': '/new.pem' }, settings: {}, base: {} };
    mocks.request.mockResolvedValue({ certificates: base });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(screen.getByText(/certificate paths have unsaved changes/)).toBeTruthy());
  });

  it('feeds both panels from exactly one /status request per 5-second poll', async () => {
    vi.useFakeTimers();
    try {
      mocks.request.mockResolvedValue({ certificates: base, running: true, doh_listen: '127.0.0.1:8443', doh_http3: false });
      const statusReads = () => mocks.request.mock.calls.filter(([path]) => path === 'status').length;
      render(<SecurityRuntime language="en"><DohRuntimeRow language="en" /></SecurityRuntime>);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(shown()).toBeTruthy();
      expect(screen.getByText('127.0.0.1:8443 · /dns-query')).toBeTruthy();
      expect(statusReads()).toBe(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
      expect(statusReads()).toBe(2);
    } finally { vi.useRealTimers(); }
  });

  it.each([[0, 'NETWORK'], [200, 'BAD_RESPONSE']])('confirms an unknown %s/%s reload from the next status read, without replay', async (status, code) => {
    let current = base;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { certificates: current };
      current = { ...base, certificate_generation: 3, last_reload: { attempt_id: 8, source: 'api', started_at_ms: 1800000000000, completed_at_ms: 1800000000010, outcome: 'applied', error_code: null } };
      throw new ApiError(status as number, code as string, 'response missing');
    });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(shown()).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText('Certificates updated.')).toBeTruthy());
    expect(screen.getByText(/· Console · Updated$/)).toBeTruthy();
    expect((screen.getByRole('button', { name: 'Reload certificates' }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByRole('button', { name: 'Check result' })).toBeNull();
    expect(mocks.request.mock.calls.filter(([path]) => path === 'certificates/reload')).toHaveLength(1);
  });

  it('reports a lost reload that did not run, and offers a manual check only when reading fails', async () => {
    let offline = false;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') { if (offline) throw new ApiError(0, 'NETWORK', 'offline'); return { certificates: base }; }
      offline = true;
      throw new ApiError(0, 'NETWORK', 'lost');
    });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(shown()).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/Confirming the result/)).toBeTruthy());
    await waitFor(() => expect(screen.getByRole('button', { name: 'Check result' })).toBeTruthy(), { timeout: 10000 });
    expect(screen.getByText(/reload couldn't be confirmed/)).toBeTruthy();
    offline = false;
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getByText(/reload didn't run/)).toBeTruthy());
    expect(mocks.request.mock.calls.filter(([path]) => path === 'certificates/reload')).toHaveLength(1);
  }, 15000);

  it('keeps the active summary after candidate rejection', async () => {
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { certificates: base };
      throw new ApiError(422, 'CERTIFICATE_NAME_MISMATCH', 'invalid SAN');
    });
    render(<SecurityRuntime language="en" />);
    await waitFor(() => expect(shown()).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: 'Reload certificates' }));
    await waitFor(() => expect(screen.getByText(/previous certificates are still in use/)).toBeTruthy());
    expect(screen.getByRole('alert').textContent).toContain("don't include the console hostname");
    expect(shown()).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Check result' })).toBeNull();
  });

  it('does not restore certificate contents after an authenticated page unmount', async () => {
    let finish!: (value: unknown) => void;
    mocks.request.mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
    const view = render(<SecurityRuntime language="en" />);
    view.unmount();
    await act(async () => { finish({ certificates: base }); });
    expect(screen.queryByText(/^Expires/)).toBeNull();
  });
});
