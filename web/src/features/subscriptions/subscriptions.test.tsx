// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, type ApiClient } from '../../session/client';
import { ConfigProvider, useConfig } from '../../config/context';
import { defaultSubscriptions, subscriptionDraft } from '../../model/subscriptions';
import { SubscriptionsPanel } from './SubscriptionsPanel';
import { useSubscriptions } from './useSubscriptions';
import { DomainCheck } from './DomainCheck';
import type { SubscriptionState, Operation } from './types';

let currentApi: ApiClient;
vi.mock('../../session/context', () => ({ useSession: () => ({ api: currentApi }) }));
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
const initial: SubscriptionState = { config_revision: 1, enabled: true, sources: [], generation: 2, content_revision: 1, input_rules: 20, index_rules: 10, index_bytes: 256, retained_bytes: 0, disk_bytes: 500, operation: null, recent_operation: null, unavailable_reason: null };
const operation: Operation = { id: 'op1', kind: 'prepare', source_id: 'example', fingerprint: 'hash', status: 'succeeded', started_at: 1, finished_at: 2, error: null, sha256: 'sha', rules: 20 };
const source = { id: 'example', name: 'Example', url: 'https://example.com/list', format: 'domain_list' as const, enabled: false, auto_update: true, update_interval_hours: 24 };

describe('subscription operations', () => {
  it('polls acknowledged operations and reports replaced history without replaying POST', async () => {
    let state = initial;
    const request = vi.fn(async (path: string) => {
      if (path.endsWith('/prepare')) { state = { ...initial, operation: { ...operation, status: 'running', finished_at: null } }; return { operation_id: 'op1' }; }
      return state;
    });
    const api = { request } as unknown as ApiClient;
    const hook = renderHook(() => useSubscriptions(api, 1));
    await waitFor(() => expect(hook.result.current.state).toEqual(initial));
    await act(async () => { await hook.result.current.start('prepare', subscriptionDraft(source)); });
    expect(request.mock.calls.find(([path]) => path.endsWith('/prepare'))).toBeTruthy();
    expect(hook.result.current.operation?.status).toBe('running');
    state = { ...initial, recent_operation: operation };
    await act(async () => { await hook.result.current.load(); });
    expect(hook.result.current.operation?.status).toBe('succeeded');
    state = initial;
    await act(async () => { await hook.result.current.load(); });
    expect(hook.result.current.replaced).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.endsWith('/prepare'))).toHaveLength(1);
  });

  it('reconciles a disconnected POST using GET and preserves unknown instead of claiming success', async () => {
    const request = vi.fn(async (path: string) => { if (path.endsWith('/refresh')) throw new ApiError(0, 'NETWORK', '<remote secret>'); return initial; });
    const api = { request } as unknown as ApiClient;
    const hook = renderHook(() => useSubscriptions(api, 1));
    await waitFor(() => expect(hook.result.current.state).not.toBeNull());
    await act(async () => { await hook.result.current.start('refresh', null); await hook.result.current.load(); });
    expect(hook.result.current.unknown).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.endsWith('/refresh'))).toHaveLength(1);
    expect(hook.result.current.error).toBeNull();
  });
});

describe('subscription draft and console', () => {
  function fixture(state: SubscriptionState = initial) {
    const settings = { filter_subscriptions: { ...defaultSubscriptions, sources: [source] }, cache: { rules: [] } };
    const request = vi.fn(async (path: string, _method?: string, body?: unknown) => {
      if (path === 'config') return { toml: 'original', revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: 'original', settings };
      if (path === 'config/preview') return { toml: 'edited', settings: { ...settings, filter_subscriptions: { ...settings.filter_subscriptions, ...(body as { changes: { filter_subscriptions: object } }).changes.filter_subscriptions } } };
      if (path === 'filter/subscriptions') return state;
      if (path === 'filter/subscriptions/prepare') throw new ApiError(400, 'subscription_parse', '<img src=x onerror=alert(1)>');
      throw new Error(path);
    });
    const api = { request } as unknown as ApiClient; currentApi = api;
    const wrapper = ({ children }: { children: React.ReactNode }) => <ConfigProvider api={api} active refreshSession={async () => {}}>{children}</ConfigProvider>;
    return { api, request, wrapper };
  }

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

  it('fills a disabled Natsuki draft without network, retains controls during language/theme change, and localizes safe errors', async () => {
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
    fireEvent.click(screen.getByRole('button', { name: '使用 Natsuki List 示例' }));
    expect(screen.getByDisplayValue('https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/main/natsuki-list.list')).toBeTruthy();
    expect(screen.getAllByRole('switch', { name: '启用来源' }).every((item) => item.getAttribute('aria-checked') === 'false')).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.includes('/prepare') || path.includes('/refresh'))).toHaveLength(0);
    fireEvent.click(screen.getAllByRole('button', { name: '验证并添加' })[0]);
    await screen.findByText(/规则格式不受支持/);
    fireEvent.click(screen.getByRole('button', { name: '刷新状态' }));
    await waitFor(() => expect(request.mock.calls.filter(([path]) => path === 'filter/subscriptions').length).toBeGreaterThan(1));
    expect(screen.getByText(/规则格式不受支持/)).toBeTruthy();
    expect(screen.queryByText(/onerror/)).toBeNull();
    expect(screen.getByDisplayValue('My draft')).toBeTruthy();
    view.rerender(<SubscriptionsPanel language="en" />);
    expect(screen.getByText(/Rules are malformed/)).toBeTruthy();
    delete document.documentElement.dataset.theme;
  });

  it('keeps stale drafts but does not attribute a new revision source status to them', async () => {
    const { wrapper } = fixture({ ...initial, config_revision: 2, sources: [{ id: source.id,
      fingerprint: 'replacement', active: true, ready: true, input_rules: 12, bytes: 50,
      last_success: 1, last_attempt: 1, next_update: null, failures: 0, error: null }] });
    render(<SubscriptionsPanel language="en" />, { wrapper });
    await screen.findByText(/Saved configuration changed/);
    expect(screen.getByDisplayValue(source.url)).toBeTruthy();
    expect(screen.queryByText('Active')).toBeNull();
  });

  it('shows local null witnesses separately from a subscription whose ID is local', async () => {
    let local = true;
    const request = vi.fn(async () => ({ generation: 3, decision: 'blocked', witness: { source_id: local ? null : 'local', rule: '.example.com', scope: 'suffix' } }));
    render(<DomainCheck api={{ request } as unknown as ApiClient} language="en" names={{ local: 'My online list' }} />);
    fireEvent.change(screen.getByRole('textbox', { name: 'Domain' }), { target: { value: 'example.com' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check' }));
    await screen.findByText(/Local rules/);
    local = false;
    fireEvent.click(screen.getByRole('button', { name: 'Check' }));
    await screen.findByText(/My online list/);
    expect(request.mock.calls).toHaveLength(2);
  });
});
