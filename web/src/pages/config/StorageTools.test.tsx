// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ApiError } from '../../session/client';
import { storage } from '../observability/__tests__/fixtures';
import { StorageTools } from './StorageTools';
import { BannerProvider } from '../../components/Banner';

const mocks = vi.hoisted(() => { const request = vi.fn(); return { request, api: { request }, confirm: vi.fn() }; });
vi.mock('../../session/context', () => ({ useSession: () => ({ api: mocks.api }) }));
vi.mock('../../config/context', () => ({ useConfig: () => ({ locked: false, busy: false, dirty: true }) }));
vi.mock('../../components/ConfirmProvider', () => ({ useConfirm: () => mocks.confirm }));
const statistics = { totals: { since_ms: 100000 } };
beforeEach(() => { mocks.request.mockReset(); mocks.confirm.mockReset().mockResolvedValue(true); });
afterEach(cleanup);

describe('persistent storage operations', () => {
  it('clears with independent epochs and no client revision while a draft is unsaved', async () => {
    const current = { ...storage };
    mocks.request.mockImplementation(async (path, method) => {
      if (path === 'status') return { storage: { ...current } };
      if (path === 'stats') return statistics;
      if (method === 'POST') {
        if (path === 'query-log/clear') current.log_epoch += 1;
        if (path === 'stats/reset') current.totals_epoch += 1;
        if (path === 'stats/history/clear') current.history_epoch += 1;
        return { removed: 1 };
      }
      throw new Error(`Unexpected ${path}`);
    });
    render(<BannerProvider><StorageTools language="en" /></BannerProvider>);
    await waitFor(() => expect(screen.getByText('Space used')).toBeTruthy());
    const cases = [
      ['Clear query log', 'query-log/clear', { log_epoch: 3 }],
      ['Reset totals', 'stats/reset', { totals_epoch: 4 }],
      ['Clear trend chart', 'stats/history/clear', { history_epoch: 5 }],
    ] as const;
    for (const [label, path, body] of cases) {
      await waitFor(() => expect((screen.getByRole('button', { name: label }) as HTMLButtonElement).disabled).toBe(false));
      fireEvent.click(screen.getByRole('button', { name: label }));
      await waitFor(() => expect(mocks.request).toHaveBeenCalledWith(path, 'POST', body));
    }
    // Totals are read on mount and again only after the totals reset; no polling.
    await waitFor(() => expect(mocks.request.mock.calls.filter(([path]) => path === 'status')).toHaveLength(4));
    expect(mocks.request.mock.calls.filter(([path]) => path === 'stats')).toHaveLength(2);
  });

  it.each([[0, 'NETWORK'], [200, 'BAD_RESPONSE'], [503, 'STORAGE_UNAVAILABLE']])('confirms an unknown reset (%s %s) from later status reads without replaying', async (status, code) => {
    let epoch = 4;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { storage: { ...storage, totals_epoch: epoch } };
      if (path === 'stats') return statistics;
      if (path === 'stats/reset') throw new ApiError(status as number, code as string, 'lost response');
      throw new Error(`Unexpected ${path}`);
    });
    render(<BannerProvider><StorageTools language="en" /></BannerProvider>);
    await waitFor(() => expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Reset totals' }));
    await waitFor(() => expect(screen.getByText('Confirming the result…')).toBeTruthy());
    expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole('button', { name: 'Check result' })).toBeNull();
    epoch = 5;
    await waitFor(() => expect(screen.getAllByText('Done.').length).toBeGreaterThan(0), { timeout: 5000 });
    expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(false);
    expect(mocks.request.mock.calls.filter(([path]) => path === 'stats/reset')).toHaveLength(1);
  });

  it('offers a manual check once the automatic reads show no result', async () => {
    let epoch = 4;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { storage: { ...storage, totals_epoch: epoch } };
      if (path === 'stats') return statistics;
      if (path === 'stats/reset') throw new ApiError(0, 'NETWORK', 'lost response');
      throw new Error(`Unexpected ${path}`);
    });
    render(<BannerProvider><StorageTools language="en" /></BannerProvider>);
    await waitFor(() => expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Reset totals' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Check result' })).toBeTruthy(), { timeout: 10000 });
    expect(screen.getByText(/result isn't visible yet/)).toBeTruthy();
    epoch = 5;
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getAllByText('Done.').length).toBeGreaterThan(0));
    expect(mocks.request.mock.calls.filter(([path]) => path === 'stats/reset')).toHaveLength(1);
  }, 15000);

  it('reports storage failure and does not invent empty statistics or successful cleanup', async () => {
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { storage: { ...storage, health: 'unavailable', error: 'read-only fixture' } };
      throw new ApiError(503, 'STORAGE_UNAVAILABLE', 'read-only fixture');
    });
    render(<BannerProvider><StorageTools language="en" /></BannerProvider>);
    await waitFor(() => expect(screen.getAllByRole('alert').some((node) => node.textContent?.includes('read-only fixture'))).toBe(true));
    expect((screen.getByRole('button', { name: 'Clear query log' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByText('Done.')).toBeNull();
  });
});
