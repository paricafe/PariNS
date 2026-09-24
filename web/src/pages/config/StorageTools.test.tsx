// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ApiError } from '../../session/client';
import { storage } from '../observability/__tests__/fixtures';
import { StorageTools } from './StorageTools';

const mocks = vi.hoisted(() => { const request = vi.fn(); return { request, api: { request }, confirm: vi.fn() }; });
vi.mock('../../session/context', () => ({ useSession: () => ({ api: mocks.api }) }));
vi.mock('../../config/context', () => ({ useConfig: () => ({ locked: false, busy: false }) }));
vi.mock('../../components/ConfirmProvider', () => ({ useConfirm: () => mocks.confirm }));
const statistics = { totals: { since_ms: 100000 } };
beforeEach(() => { mocks.request.mockReset(); mocks.confirm.mockReset().mockResolvedValue(true); });
afterEach(cleanup);

describe('persistent storage operations', () => {
  it('uses independent epochs and the live revision for all three actions', async () => {
    const current = { ...storage };
    mocks.request.mockImplementation(async (path, method) => {
      if (path === 'status') return { revision: 7, storage: { ...current } };
      if (path === 'stats') return statistics;
      if (method === 'POST') {
        if (path === 'query-log/clear') current.log_epoch += 1;
        if (path === 'stats/reset') current.totals_epoch += 1;
        if (path === 'stats/history/clear') current.history_epoch += 1;
        return { removed: 1 };
      }
      throw new Error(`Unexpected ${path}`);
    });
    render(<StorageTools language="en" />);
    await waitFor(() => expect(screen.getByText('32,768 B')).toBeTruthy());
    const cases = [
      ['Clear query records', 'query-log/clear', { revision: 7, log_epoch: 3 }],
      ['Reset totals', 'stats/reset', { revision: 7, totals_epoch: 4 }],
      ['Clear trend history', 'stats/history/clear', { revision: 7, history_epoch: 5 }],
    ] as const;
    for (const [label, path, body] of cases) {
      await waitFor(() => expect((screen.getByRole('button', { name: label }) as HTMLButtonElement).disabled).toBe(false));
      fireEvent.click(screen.getByRole('button', { name: label }));
      await waitFor(() => expect(mocks.request).toHaveBeenCalledWith(path, 'POST', body));
    }
  });

  it.each([[0, 'NETWORK'], [200, 'BAD_RESPONSE'], [503, 'STORAGE_UNAVAILABLE']])('checks an unknown reset (%s %s) by reading its epoch without replaying', async (status, code) => {
    let epoch = 4;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { revision: 7, storage: { ...storage, totals_epoch: epoch } };
      if (path === 'stats') return statistics;
      if (path === 'stats/reset') throw new ApiError(status as number, code as string, 'lost response');
      throw new Error(`Unexpected ${path}`);
    });
    render(<StorageTools language="en" />);
    await waitFor(() => expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Reset totals' }));
    await waitFor(() => expect(screen.getByText(/The result is not confirmed/)).toBeTruthy());
    expect((screen.getByRole('button', { name: 'Reset totals' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getByText(/committed result is not visible/)).toBeTruthy());
    epoch = 5;
    fireEvent.click(screen.getByRole('button', { name: 'Check result' }));
    await waitFor(() => expect(screen.getByText('The change has been saved.')).toBeTruthy());
    expect(mocks.request.mock.calls.filter(([path]) => path === 'stats/reset')).toHaveLength(1);
  });

  it('reports storage failure and does not invent empty statistics or successful cleanup', async () => {
    mocks.request.mockImplementation(async (path) => {
      if (path === 'status') return { revision: 7, storage: { ...storage, health: 'unavailable', error: 'read-only fixture' } };
      throw new ApiError(503, 'STORAGE_UNAVAILABLE', 'read-only fixture');
    });
    render(<StorageTools language="en" />);
    await waitFor(() => expect(screen.getAllByRole('alert').some((node) => node.textContent?.includes('read-only fixture'))).toBe(true));
    expect((screen.getByRole('button', { name: 'Clear query records' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByText('The change has been saved.')).toBeNull();
  });
});
