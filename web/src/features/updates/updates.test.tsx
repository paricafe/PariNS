// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { ApiError } from '../../session/client';
import { UpdatesProvider } from './context';
import { UpdatePanel, UpdateHint } from './UpdatePanel';
import { canApply, pollDelay, type UpdatesView } from './types';

const mocks = vi.hoisted(() => {
  const request = vi.fn();
  return { request, api: { request }, confirm: vi.fn(), reloadPage: vi.fn(), expectRestart: vi.fn(),
    config: { dirty: false, busy: false, locked: false, draft: { revision: 7, unknownApply: false, settings: { updates: { auto_check: true, check_interval_hours: 6 } } }, reload: vi.fn(), setUpdateLocked: vi.fn() } };
});
vi.mock('../../session/context', () => ({ useSession: () => ({ api: mocks.api, expectUpdateRestart: mocks.expectRestart }) }));
vi.mock('../../config/context', () => ({ useConfig: () => mocks.config }));
vi.mock('../../components/ConfirmProvider', () => ({ useConfirm: () => mocks.confirm }));
vi.mock('./reload', () => ({ reloadConsole: () => mocks.reloadPage() }));

function fixture(): UpdatesView {
  return { current: { version: '0.1.4', target: 'x86_64-unknown-linux-musl', official_release: true },
    capability: { available: true, reason: null }, check: { state: 'available', last_check_at_ms: 1000, last_success_at_ms: 1000, next_check_at_ms: Date.now() + 60_000, retry_at_ms: 0, error: null },
    candidate: { version: '0.1.5', tag: 'v0.1.5', published_at: null, notes: '<img src="https://bad.example/x" onerror="alert(1)">', manual_reason: null, plan_id: 'plan-1', expires_at_ms: Date.now() + 60_000 },
    active_operation: null, last_operation: null, frozen: false };
}
function mount(language: 'en' | 'zh-CN' = 'en') {
  return render(<MemoryRouter><UpdatesProvider active><UpdateHint language={language} /><UpdatePanel language={language} /></UpdatesProvider></MemoryRouter>);
}
beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function () { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function () { this.removeAttribute('open'); };
});
beforeEach(() => {
  vi.clearAllMocks();
  mocks.config.dirty = false; mocks.config.busy = false; mocks.config.locked = false; mocks.config.draft.unknownApply = false;
  mocks.config.draft.revision = 7;
  mocks.confirm.mockResolvedValue(true); mocks.config.reload.mockResolvedValue(undefined);
});
afterEach(cleanup);

describe('software update presentation', () => {
  it('reads cached status only, renders release text safely and switches language', async () => {
    const current = fixture(); mocks.request.mockResolvedValue(current);
    const view = mount();
    await screen.findByText('Software updates');
    await screen.findByRole('link', { name: 'View release on GitHub' });
    expect(document.querySelector('img')).toBeNull();
    expect(screen.getByText(current.candidate!.notes)).toBeTruthy();
    expect(mocks.request.mock.calls.every(([path, method]) => path === 'updates' && !method)).toBe(true);
    view.rerender(<MemoryRouter><UpdatesProvider active><UpdateHint language="zh-CN" /><UpdatePanel language="zh-CN" /></UpdatesProvider></MemoryRouter>);
    expect(screen.getByText('软件更新')).toBeTruthy();
    expect(screen.getByRole('button', { name: '立即更新' })).toBeTruthy();
  });

  it('requires resolving drafts, then a separate explicit update confirmation', async () => {
    const current = fixture(); mocks.request.mockResolvedValue(current); mocks.config.dirty = true;
    mount();
    await waitFor(() => expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Update now' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
    expect(mocks.confirm).not.toHaveBeenCalled();
    expect(mocks.request.mock.calls.some(([path]) => path === 'updates/apply')).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'Discard draft' }));
    await waitFor(() => expect(mocks.config.reload).toHaveBeenCalledTimes(1));
    expect(mocks.confirm).not.toHaveBeenCalled();
  });

  it('submits one confirmed tuple and reconciles an unknown response only by GET', async () => {
    const current = fixture();
    mocks.request.mockImplementation(async (path) => { if (path === 'updates/apply') throw new ApiError(0, 'NETWORK', 'lost'); return current; });
    mount();
    await waitFor(() => expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Update now' }));
    await waitFor(() => expect(mocks.request).toHaveBeenCalledWith('updates/apply', 'POST', { plan_id: 'plan-1', expected_version: '0.1.5', config_revision: 7 }));
    await screen.findByText(/request result is not confirmed/);
    expect(mocks.expectRestart).toHaveBeenCalledWith(true);
    expect(mocks.config.setUpdateLocked).toHaveBeenCalledWith(true);
    fireEvent.click(screen.getByRole('button', { name: 'Read update status' }));
    await waitFor(() => expect(mocks.request.mock.calls.filter(([path]) => path === 'updates/apply')).toHaveLength(1));
    expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(true);
    current.last_operation = { operation_id: 'op-1', phase: 'failed', version: '0.1.5', reason: 'preflight_failed', downloaded_bytes: 0, total_bytes: 100 };
    fireEvent.click(screen.getByRole('button', { name: 'Read update status' }));
    await screen.findByText('Update did not complete');
    await waitFor(() => expect(mocks.expectRestart).toHaveBeenLastCalledWith(false));
    expect(mocks.reloadPage).not.toHaveBeenCalled();
  });

  it('keeps a check failure distinct from stale available results and shows rate limits', async () => {
    const current = fixture(); current.check.state = 'failed'; current.check.error = 'rate_limited'; current.check.retry_at_ms = Date.now() + 90_000;
    mocks.request.mockResolvedValue(current); mount();
    await screen.findByText('The check failed. The last result is kept below.');
    expect(screen.queryByText('You have the latest stable release')).toBeNull();
    expect((screen.getByRole('button', { name: 'Check for updates' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/Check again after/)).toBeTruthy();
  });

  it('shows manual capability and real download bytes without inventing percentages', async () => {
    const current = fixture(); current.capability = { available: false, reason: 'unsupported_installation' };
    current.active_operation = { operation_id: 'other', phase: 'downloading', version: '0.1.5', reason: null, downloaded_bytes: 1024, total_bytes: null };
    mocks.request.mockResolvedValue(current); mount();
    await screen.findByText('Downloaded 1,024 B');
    expect(screen.queryByRole('progressbar')).toBeNull();
    expect(screen.getByText('This installation requires a manual update')).toBeTruthy();
    expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(true);
    expect(mocks.expectRestart).not.toHaveBeenCalled();
  });

  it('reports a transient disconnect as waiting, then reloads the console after own rollback', async () => {
    const current = fixture(); let offline = false;
    mocks.request.mockImplementation(async (path) => {
      if (path === 'updates/apply') { offline = true; return { operation_id: 'op-2' }; }
      if (offline) throw new ApiError(0, 'NETWORK', 'restart');
      return current;
    });
    mount();
    await waitFor(() => expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Update now' }));
    await screen.findByText(/Waiting for the service to reconnect/);
    expect(mocks.reloadPage).not.toHaveBeenCalled();
    offline = false;
    current.last_operation = { operation_id: 'op-2', phase: 'rolled_back', version: '0.1.5', reason: 'readiness_failed', downloaded_bytes: 100, total_bytes: 100 };
    fireEvent.click(screen.getByRole('button', { name: 'Read update status' }));
    await waitFor(() => expect(mocks.reloadPage).toHaveBeenCalledTimes(1));
    expect(mocks.request.mock.calls.filter(([path]) => path === 'updates/apply')).toHaveLength(1);
  });

  it('rejects a draft introduced while the confirmation is open', async () => {
    const current = fixture(); mocks.request.mockResolvedValue(current);
    mocks.confirm.mockImplementation(async () => { mocks.config.dirty = true; return true; });
    mount();
    await waitFor(() => expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Update now' }));
    await screen.findByText('The draft or configuration revision changed. Resolve it and confirm again.');
    expect(mocks.request.mock.calls.some(([path]) => path === 'updates/apply')).toBe(false);
  });

  it('disarms restart and releases the local editing lock after a definite rejection', async () => {
    const current = fixture();
    mocks.request.mockImplementation(async (path) => { if (path === 'updates/apply') throw new ApiError(409, 'config_changed', 'changed'); return current; });
    mount();
    await waitFor(() => expect((screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: 'Update now' }));
    await screen.findByText('Configuration changed. Check again and confirm.');
    expect(mocks.expectRestart).toHaveBeenLastCalledWith(false);
    expect(mocks.config.setUpdateLocked).toHaveBeenLastCalledWith(false);
    expect(screen.queryByText(/request result is not confirmed/)).toBeNull();
    expect(mocks.reloadPage).not.toHaveBeenCalled();
  });
});

it('polls active operations quickly and returns to low frequency on terminal state or hidden pages', () => {
  const current = fixture();
  expect(canApply(current)).toBe(true);
  expect(pollDelay(current, false, false)).toBe(30_000);
  expect(pollDelay(current, true, false)).toBe(2_000);
  expect(pollDelay(current, true, true)).toBe(60_000);
  current.candidate!.expires_at_ms = 1;
  expect(canApply(current)).toBe(false);
});
