// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, type ApiClient } from '../../session/client';
import { ConfigProvider, useConfig } from '../../config/context';
import { subscriptionDraft, subscriptionId } from '../../model/subscriptions';
import { formatDate, type Language } from '../../i18n';
import { SubscriptionsPanel } from './SubscriptionsPanel';
import { useSubscriptions } from './useSubscriptions';
import { DomainCheck } from './DomainCheck';
import { pollDelay, type SubscriptionState, type Operation } from './types';

let currentApi: ApiClient;
vi.mock('../../session/context', () => ({ useSession: () => ({ api: currentApi }) }));
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
const initial: SubscriptionState = { enabled: true, sources: [], generation: 2, content_revision: 1, input_rules: 20, index_rules: 10, index_bytes: 256, retained_bytes: 0, disk_bytes: 500, operation: null, recent_operation: null, unavailable_reason: null };
const operation: Operation = { id: 'op1', kind: 'prepare', source_id: 'example', fingerprint: 'hash', status: 'succeeded', started_at: 1, finished_at: 2, error: null, sha256: 'sha', rules: 20 };
const subscriptionLimits = { max_rules: 1000000, max_memory_bytes: 134217728, max_disk_bytes: 268435456 };
const source = { id: 'example', name: 'Example', url: 'https://example.com/list', format: 'domain_list' as const, enabled: false, auto_update: true, update_interval_hours: 24 };

describe('subscription operations', () => {
  it('polls an acknowledged update without replaying POST', async () => {
    let state = initial;
    const refresh = { ...operation, kind: 'refresh' as const };
    const request = vi.fn(async (path: string) => {
      if (path.endsWith('/refresh')) { state = { ...initial, operation: { ...refresh, status: 'running', finished_at: null } }; return { operation_id: 'op1' }; }
      return state;
    });
    const api = { request } as unknown as ApiClient;
    const hook = renderHook(() => useSubscriptions(api, 1));
    await waitFor(() => expect(hook.result.current.state).toEqual(initial));
    await act(async () => { await hook.result.current.refresh('example'); });
    expect(request).toHaveBeenCalledWith('filter/subscriptions/refresh', 'POST', { source_id: 'example' });
    expect(hook.result.current.operation?.status).toBe('running');
    state = { ...initial, recent_operation: refresh };
    await act(async () => { await hook.result.current.load(); });
    expect(hook.result.current.operation?.status).toBe('succeeded');
    expect(request.mock.calls.filter(([path]) => path.endsWith('/refresh'))).toHaveLength(1);
  });

  it('reconciles a disconnected POST using GET and preserves unknown instead of claiming success', async () => {
    const request = vi.fn(async (path: string) => { if (path.endsWith('/refresh')) throw new ApiError(0, 'NETWORK', '<remote secret>'); return initial; });
    const api = { request } as unknown as ApiClient;
    const hook = renderHook(() => useSubscriptions(api, 1));
    await waitFor(() => expect(hook.result.current.state).not.toBeNull());
    await act(async () => { await hook.result.current.refresh(null); await hook.result.current.load(); });
    expect(hook.result.current.unknown).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.endsWith('/refresh'))).toHaveLength(1);
    expect(hook.result.current.error).toBeNull();
  });
});

describe('subscription polling cadence', () => {
  it('polls every 3 s only while work or an outcome is open, otherwise 30 s, and pauses while hidden', () => {
    const running = { ...initial, operation: { ...operation, status: 'running' as const, finished_at: null } };
    expect(pollDelay(initial, false, false, false)).toBe(30_000);
    expect(pollDelay(null, false, false, false)).toBe(30_000);
    expect(pollDelay(running, false, false, false)).toBe(3_000);
    expect(pollDelay(initial, true, false, false)).toBe(3_000);
    expect(pollDelay(initial, false, true, false)).toBe(3_000);
    expect(pollDelay(running, true, true, true)).toBeNull();
  });
});

describe('subscription draft and console', () => {
  function fixture(state: SubscriptionState = initial, savedSource = source) {
    let settings = { filter_subscriptions: { ...subscriptionLimits, enabled: state.enabled, sources: [savedSource] }, cache: { rules: [] } };
    let previewSettings = settings;
    let revision = 1;
    const request = vi.fn(async (path: string, method?: string, body?: unknown) => {
      if (path === 'config' && method === 'PUT') {
        settings = previewSettings;
        revision += 1;
        return { revision, restart_required: false, transport_change: null };
      }
      if (path === 'config') return { toml: 'original', revision, has_backup: false };
      if (path === 'config/parse') return { toml: 'original', settings };
      if (path === 'config/preview') {
        const candidate = body as { toml: string; changes: { filter_subscriptions: object } };
        const baseline = candidate.toml === 'edited' ? previewSettings : settings;
        previewSettings = { ...baseline, filter_subscriptions: { ...baseline.filter_subscriptions, ...candidate.changes.filter_subscriptions } };
        return { toml: 'edited', settings: previewSettings };
      }
      if (path === 'config/validate') return { restart_required: false, transport_change: null };
      if (path === 'filter/subscriptions') return state;
      if (path === 'filter/subscriptions/prepare') throw new ApiError(400, 'subscription_parse', '<img src=x onerror=alert(1)>');
      if (path === 'filter/subscriptions/refresh') return { operation_id: 'op-refresh' };
      throw new Error(path);
    });
    const api = { request } as unknown as ApiClient; currentApi = api;
    const wrapper = ({ children }: { children: React.ReactNode }) => <ConfigProvider api={api} active refreshSession={async () => {}}>{children}</ConfigProvider>;
    return { api, request, wrapper };
  }

  const scheduled: SubscriptionState = { ...initial, sources: [{ id: source.id,
    fingerprint: 'hash', active: true, ready: true, input_rules: 12, bytes: 50,
    last_success: 1, last_attempt: 1, next_update: 1900000000, failures: 0, error: null }] };

  it('changes the displayed schedule only after saving, and retains it across refresh and languages', async () => {
    const { wrapper } = fixture(scheduled, { ...source, enabled: true });
    let config!: ReturnType<typeof useConfig>;
    function Console({ language }: { language: Language }) {
      config = useConfig();
      return <SubscriptionsPanel language={language} />;
    }
    const view = render(<Console language="en" />, { wrapper });
    const next = () => screen.getByText('Next update').nextElementSibling?.textContent;
    const date = formatDate(1900000000000, 'en', { dateStyle: 'medium', timeStyle: 'short' });
    await waitFor(() => expect(next()).toBe(date));
    fireEvent.click(screen.getByRole('switch', { name: 'Automatic updates' }));
    expect(next()).toBe(date); // Unsaved disabling is still only a draft.
    await act(async () => { await config.preview(); });
    expect(next()).toBe(date); // Preview must not replace the saved baseline.
    let prepared!: Awaited<ReturnType<typeof config.prepareSave>>;
    await act(async () => { prepared = await config.prepareSave(); });
    await act(async () => { await config.commitPrepared(prepared); });
    await waitFor(() => expect(next()).toBe('Manual updates only'));
    fireEvent.click(screen.getByRole('button', { name: 'Refresh status' }));
    await act(async () => { await config.reload(); });
    expect(next()).toBe('Manual updates only');
    view.rerender(<Console language="zh-CN" />);
    expect(screen.getByText('下次更新').nextElementSibling?.textContent).toBe('仅手动更新');
    fireEvent.click(screen.getByRole('switch', { name: '自动更新' }));
    expect(screen.getByText('下次更新').nextElementSibling?.textContent).toBe('仅手动更新');
    await act(async () => { prepared = await config.prepareSave(); });
    await act(async () => { await config.commitPrepared(prepared); });
    await waitFor(() => expect(screen.getByText('下次更新').nextElementSibling?.textContent)
      .toBe(formatDate(1900000000000, 'zh-CN', { dateStyle: 'medium', timeStyle: 'short' })));
  });

  it.each([
    { global: false, enabled: true, language: 'en' as const, label: 'Next update', expected: 'Off' },
    { global: true, enabled: false, language: 'zh-CN' as const, label: '下次更新', expected: '未开启' },
  ])('hides retained dates when the saved global/source setting is disabled: $global/$enabled', async ({ global, enabled, language, label, expected }) => {
    const { wrapper } = fixture({ ...scheduled, enabled: global }, { ...source, enabled });
    let config!: ReturnType<typeof useConfig>;
    function Console() { config = useConfig(); return <SubscriptionsPanel language={language} />; }
    render(<Console />, { wrapper });
    await waitFor(() => expect(screen.getByText(label).nextElementSibling?.textContent).toBe(expected));
    act(() => {
      config.updateField('filter_subscriptions.enabled', true);
      config.setSources([subscriptionDraft({ ...source, enabled: true })]);
    });
    await act(async () => { await config.preview(); });
    expect(screen.getByText(label).nextElementSibling?.textContent).toBe(expected);
  });

  it('keeps incomplete intervals, replaces the source array, and preserves saved identity after preview', async () => {
    const { request, wrapper } = fixture();
    const hook = renderHook(useConfig, { wrapper });
    await waitFor(() => expect(hook.result.current.draft).not.toBeNull());
    act(() => hook.result.current.setSources([{ ...subscriptionDraft(source), update_interval_hours: '-' }]));
    await act(async () => { await expect(hook.result.current.preview()).rejects.toMatchObject({ path: 'filter_subscriptions.sources.0.update_interval_hours' }); });
    expect(hook.result.current.draft?.sources?.[0].update_interval_hours).toBe('-');
    act(() => hook.result.current.setSources([{ ...subscriptionDraft(source), name: 'Renamed', url: 'https://example.org/new', update_interval_hours: '12' }]));
    await act(async () => { await hook.result.current.preview(); });
    expect(request.mock.calls.find(([path]) => path === 'config/preview')?.[2]).toMatchObject({ changes: { filter_subscriptions: { sources: [{ ...source, name: 'Renamed', url: 'https://example.org/new', update_interval_hours: 12 }] } } });
    expect(hook.result.current.draft?.savedSources[0].url).toBe(source.url);
    act(() => hook.result.current.setUpdateLocked(true));
    act(() => hook.result.current.setSources([]));
    expect(hook.result.current.draft?.sources).toBeNull();
  });

  it('shows a software update freeze as status and storage failures as errors', async () => {
    const frozen = render(<SubscriptionsPanel language="en" />, { wrapper: fixture({ ...initial, unavailable_reason: 'update_in_progress' }).wrapper });
    const notice = await screen.findByText('A software update is being installed; subscription updates resume when it finishes.');
    expect(notice.getAttribute('role')).toBe('status');
    expect(notice.className).toBe('notice');
    frozen.unmount();
    render(<SubscriptionsPanel language="en" />, { wrapper: fixture({ ...initial, unavailable_reason: 'subscription_storage_unavailable' }).wrapper });
    expect((await screen.findByText("Subscription data can't be read or written. Check the service log and the server's disk.")).getAttribute('role')).toBe('alert');
  });

  it('fills a disabled Natsuki draft without network, keeps controls across language and theme changes, and derives its ID from the name', async () => {
    vi.spyOn(crypto, 'randomUUID').mockImplementation(() => { throw new Error('unavailable on HTTP'); });
    const { request, wrapper } = fixture();
    const view = render(<SubscriptionsPanel language="en" />, { wrapper });
    await screen.findByDisplayValue('Example');
    const name = screen.getByDisplayValue('Example');
    fireEvent.change(name, { target: { value: 'My draft' } }); name.focus();
    document.documentElement.dataset.theme = 'dark';
    view.rerender(<SubscriptionsPanel language="zh-CN" />);
    expect(screen.getByDisplayValue('My draft')).toBe(name);
    expect(document.activeElement).toBe(name);
    fireEvent.click(screen.getByRole('button', { name: '填入 Natsuki List 示例' }));
    expect(screen.getByDisplayValue('https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/main/natsuki-list.list')).toBeTruthy();
    expect(screen.getAllByRole('switch', { name: '开启' }).every((item) => item.getAttribute('aria-checked') === 'false')).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.includes('/prepare') || path.includes('/refresh'))).toHaveLength(0);
    // The ID follows the name until it is edited under More options.
    expect(screen.getByDisplayValue('natsuki-list')).toBeTruthy();
    expect(screen.queryByRole('button', { name: '验证并下载' })).toBeNull();
    fireEvent.change(screen.getByDisplayValue('Natsuki List'), { target: { value: 'Natsuki 主列表' } });
    expect(screen.getByDisplayValue('natsuki')).toBeTruthy();
    fireEvent.change(screen.getByDisplayValue('natsuki'), { target: { value: 'mine' } });
    fireEvent.change(screen.getByDisplayValue('Natsuki 主列表'), { target: { value: 'Renamed' } });
    expect(screen.getByDisplayValue('mine')).toBeTruthy();
    expect(screen.getByDisplayValue('My draft')).toBeTruthy();
    delete document.documentElement.dataset.theme;
  });

  it('updates a saved source without a client revision while an unrelated draft is unsaved', async () => {
    const { request, wrapper } = fixture(scheduled, { ...source, enabled: true });
    let config!: ReturnType<typeof useConfig>;
    function Console() { config = useConfig(); return <SubscriptionsPanel language="en" />; }
    render(<Console />, { wrapper });
    await screen.findByText('In use');
    act(() => config.updateField('max_inflight', '50'));
    expect(config.dirty).toBe(true);
    const update = screen.getByRole('button', { name: 'Update now' }) as HTMLButtonElement;
    expect(update.disabled).toBe(false);
    fireEvent.click(update);
    await waitFor(() => expect(request).toHaveBeenCalledWith('filter/subscriptions/refresh', 'POST', { source_id: source.id }));
    expect(screen.queryByText(/saved again/)).toBeNull();
  });

  it('shows local null witnesses separately from a subscription whose ID is local', async () => {
    let local = true;
    const request = vi.fn(async () => ({ generation: 3, decision: 'blocked', witness: { source_id: local ? null : 'local', rule: '.example.com', scope: 'suffix' } }));
    render(<DomainCheck api={{ request } as unknown as ApiClient} language="en" names={{ local: 'My online list' }} />);
    fireEvent.change(screen.getByRole('textbox', { name: 'Domain' }), { target: { value: 'example.com' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test' }));
    await screen.findByText(/Local rules/);
    local = false;
    fireEvent.click(screen.getByRole('button', { name: 'Test' }));
    await screen.findByText(/My online list/);
    expect(request.mock.calls).toHaveLength(2);
  });
});

describe('save downloads new sources first', () => {
  const list = 'https://example.org/new.list';
  function saveFixture(outcome: 'succeeded' | 'failed') {
    const saved = { filter_subscriptions: { ...subscriptionLimits, enabled: true, sources: [] as unknown[] }, cache: { rules: [] } };
    let state: SubscriptionState = initial;
    let preview = saved;
    const request = vi.fn(async (path: string, method?: string, body?: unknown) => {
      if (path === 'config' && method === 'PUT') return { revision: 2, restart_required: false, transport_change: null };
      if (path === 'config') return { toml: 'original', revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: 'original', settings: saved };
      if (path === 'config/preview') {
        const changes = (body as { changes: { filter_subscriptions: object } }).changes;
        preview = { ...saved, filter_subscriptions: { ...saved.filter_subscriptions, ...changes.filter_subscriptions } };
        return { toml: 'edited', settings: preview };
      }
      if (path === 'filter/subscriptions/prepare') {
        state = { ...initial, operation: { ...operation, id: 'op-new', status: 'running', finished_at: null } };
        return { operation_id: 'op-new' };
      }
      if (path === 'filter/subscriptions') {
        const current = state;
        if (current.operation?.status === 'running') state = { ...initial, recent_operation: { ...operation, id: 'op-new', status: outcome,
          error: outcome === 'failed' ? { code: 'subscription_parse', line: 3 } : null } };
        return current;
      }
      if (path === 'config/validate') return { restart_required: false, transport_change: null };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const api = { request } as unknown as ApiClient; currentApi = api;
    const wrapper = ({ children }: { children: React.ReactNode }) => <ConfigProvider api={api} active refreshSession={async () => {}}>{children}</ConfigProvider>;
    return { request, wrapper };
  }
  async function draftSource(wrapper: ReturnType<typeof saveFixture>['wrapper']) {
    let config!: ReturnType<typeof useConfig>;
    function Console() { config = useConfig(); return <SubscriptionsPanel language="en" />; }
    render(<Console />, { wrapper });
    fireEvent.click(await screen.findByRole('button', { name: 'Add source' }));
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'My List' } });
    fireEvent.change(screen.getByLabelText('HTTPS URL'), { target: { value: list } });
    fireEvent.click(screen.getByRole('switch', { name: 'On' }));
    expect(screen.getByText('Add an HTTPS rule list and save; PariNS downloads and checks it first, then starts using it.')).toBeTruthy();
    return () => config;
  }

  it('downloads an enabled new source, waits for it, then validates and saves', async () => {
    const { request, wrapper } = saveFixture('succeeded');
    const config = await draftSource(wrapper);
    let prepared!: Awaited<ReturnType<ReturnType<typeof config>['prepareSave']>>;
    await act(async () => { prepared = await config().prepareSave(); });
    const paths = request.mock.calls.map(([path, method]) => `${method ?? 'GET'} ${path}`);
    expect(request).toHaveBeenCalledWith('filter/subscriptions/prepare', 'POST', { source: { id: 'my-list', url: list, format: 'domain_list' } });
    expect(paths.indexOf('POST filter/subscriptions/prepare')).toBeLessThan(paths.indexOf('POST config/validate'));
    await act(async () => { await config().commitPrepared(prepared); });
    expect(request.mock.calls.filter(([path]) => path === 'filter/subscriptions/prepare')).toHaveLength(1);
    expect(request.mock.calls.some(([path, method]) => path === 'config' && method === 'PUT')).toBe(true);
  });

  it('stops before saving when the download fails and points at the source', async () => {
    const { request, wrapper } = saveFixture('failed');
    const config = await draftSource(wrapper);
    await act(async () => { await expect(config().prepareSave()).rejects.toMatchObject({ key: 'subscriptions.subscription_parse', path: 'filter_subscriptions.sources.0.url' }); });
    expect(request.mock.calls.some(([path]) => path === 'config/validate')).toBe(false);
    expect(request.mock.calls.some(([path, method]) => path === 'config' && method === 'PUT')).toBe(false);
    expect(config().draft?.toml).toBe('edited');
  });

  it('generates unique, valid IDs from names', () => {
    expect(subscriptionId('My List!', [])).toBe('my-list');
    expect(subscriptionId('My List', ['my-list', 'my-list-2'])).toBe('my-list-3');
    expect(subscriptionId('中文名称', [])).toBe('source');
    expect(subscriptionId('a'.repeat(40), ['a'.repeat(32)])).toBe(`${'a'.repeat(30)}-2`);
  });
});
