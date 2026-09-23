// @vitest-environment jsdom
import { act, renderHook, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { ApiError, type ApiClient } from '../session/client';
import { ConfigProvider, useConfig } from './context';

const original = 'max_inflight = 10\n';
const edited = 'max_inflight = 11\n';
const baseSettings = { max_inflight: 10, cache: { rules: [] }, web: null, dot: null, doh: null, doq: null, doh3: null };

function fixture(handler?: (path: string, method: string, body: unknown) => Promise<unknown>) {
  const request = vi.fn(async (path: string, method = 'GET', body?: unknown) => {
    if (handler) return handler(path, method, body);
    if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
    if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
    if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, max_inflight: 11 } };
    if (path === 'config/validate') return { restart_required: true };
    throw new Error(`Unexpected ${method} ${path}`);
  });
  return { request, api: { request } as unknown as ApiClient };
}

function mount(api: ApiClient, refreshSession: () => Promise<void> = async () => {}) {
  return renderHook(() => useConfig(), { wrapper: ({ children }) => <ConfigProvider api={api} active refreshSession={refreshSession}>{children}</ConfigProvider> });
}

describe('single configuration draft', () => {
  it('projects a new console access name and confirms an HTTPS to HTTP save explicitly', async () => {
    const change = { from: 'https', to: 'http', next_origin: 'http://192.0.2.20:3000', reauthenticate: true, requires_http_confirmation: true } as const;
    const { api, request } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, web: { public_host: 'dns.example.com' } }, transport_change: change };
      if (path === 'config/validate') return { restart_required: true, transport_change: change };
      if (path === 'config' && method === 'PUT') return { revision: 2, transport_change: change };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('web.public_host', 'dns.example.com'));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.prepareSave>>;
    await act(async () => { prepared = await hook.result.current.prepareSave(); });
    const previewCall = request.mock.calls.find(([path]) => path === 'config/preview');
    expect(previewCall?.[2]).toMatchObject({ changes: { web: { public_host: 'dns.example.com' } } });
    expect(prepared.transportChange).toEqual(change);
    let result!: Awaited<ReturnType<typeof hook.result.current.commitPrepared>>;
    await act(async () => { result = await hook.result.current.commitPrepared(prepared); });
    expect(result.transportChange).toEqual(change);
    expect(request.mock.calls.find(([path, method]) => path === 'config' && method === 'PUT')?.[2])
      .toMatchObject({ allow_http_downgrade: true, revision: 1 });
  });

  it('keeps raw public_host text for Rust validation and previews direct TOML edits', async () => {
    const { api, request } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: (body as { toml: string }).toml, settings: baseSettings, transport_change: null };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('web.public_host', ' dns.example.com '));
    await act(async () => { await hook.result.current.preview(); });
    expect(request.mock.calls.find(([path]) => path === 'config/preview')?.[2])
      .toMatchObject({ changes: { web: { public_host: ' dns.example.com ' } } });
    act(() => hook.result.current.setToml(`${original}\n[web]\npublic_host = "dns.example.com"\n`));
    await act(async () => { await hook.result.current.preview(); });
    expect(request.mock.calls.filter(([path]) => path === 'config/preview').at(-1)?.[2])
      .toMatchObject({ changes: {} });
  });

  it('previews rollback before sending the downgrade confirmation flag', async () => {
    const change = { from: 'https', to: 'http', next_origin: null, reauthenticate: true, requires_http_confirmation: true } as const;
    const { api, request } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: true };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/rollback/preview') return { transport_change: change };
      if (path === 'config/rollback') return { revision: 2, transport_change: change };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.previewRollback>>;
    await act(async () => { prepared = await hook.result.current.previewRollback(); });
    await act(async () => { await hook.result.current.rollback(prepared); });
    expect(request.mock.calls.find(([path]) => path === 'config/rollback')?.[2])
      .toMatchObject({ revision: 1, allow_http_downgrade: true });
  });

  it('refreshes session transport after an acknowledged same-origin save', async () => {
    let saved = false;
    const refreshSession = vi.fn(async () => {});
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return saved
        ? { toml: edited, revision: 2, has_backup: true }
        : { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, max_inflight: 11 }, transport_change: null };
      if (path === 'config/validate') return { restart_required: true, transport_change: null };
      if (path === 'config' && method === 'PUT') { saved = true; return { revision: 2, transport_change: null }; }
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api, refreshSession);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('max_inflight', '11'));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.prepareSave>>;
    await act(async () => { prepared = await hook.result.current.prepareSave(); });
    await act(async () => { await hook.result.current.commitPrepared(prepared); });
    expect(refreshSession).toHaveBeenCalledTimes(1);
    expect(hook.result.current.draft?.revision).toBe(2);
  });

  it('refreshes session transport after an acknowledged same-origin rollback', async () => {
    let restored = false;
    const refreshSession = vi.fn(async () => {});
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return restored
        ? { toml: edited, revision: 2, has_backup: true }
        : { toml: original, revision: 1, has_backup: true };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/rollback/preview') return { transport_change: null };
      if (path === 'config/rollback') { restored = true; return { revision: 2, transport_change: null }; }
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api, refreshSession);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.previewRollback>>;
    await act(async () => { prepared = await hook.result.current.previewRollback(); });
    await act(async () => { await hook.result.current.rollback(prepared); });
    expect(refreshSession).toHaveBeenCalledTimes(1);
    expect(hook.result.current.draft?.revision).toBe(2);
  });

  it('does not let a delayed preview erase newer raw input', async () => {
    let release!: (value: unknown) => void;
    const deferred = new Promise<unknown>((resolve) => { release = resolve; });
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return deferred;
      throw new Error(`Unexpected ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('max_inflight', '11'));
    let pending!: Promise<string>;
    act(() => { pending = hook.result.current.preview(); });
    act(() => hook.result.current.updateField('max_inflight', '12'));
    release({ toml: edited, settings: { ...baseSettings, max_inflight: 11 } });
    await expect(pending).rejects.toThrow('Draft changed');
    expect(hook.result.current.draft?.fields.max_inflight).toBe('12');
    expect(hook.result.current.draft?.toml).toBe(original);
  });

  it('keeps the submitted snapshot uncertain after a disconnected PUT and unchanged read', async () => {
    const refreshSession = vi.fn(async () => {});
    let remoteApplied = false;
    const { api, request } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return remoteApplied
        ? { toml: edited, revision: 2, has_backup: true }
        : { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, max_inflight: 11 } };
      if (path === 'config/validate') return { restart_required: true };
      if (path === 'config' && method === 'PUT') throw new ApiError(0, 'NETWORK', 'disconnected');
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api, refreshSession);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('max_inflight', '11'));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.prepareSave>>;
    await act(async () => { prepared = await hook.result.current.prepareSave(); });
    expect(prepared.toml).toBe(edited);
    await act(async () => { await expect(hook.result.current.commitPrepared(prepared)).rejects.toMatchObject({ code: 'NETWORK' }); });
    expect(hook.result.current.draft?.pendingApply).toEqual({ toml: edited, revision: 1 });
    act(() => hook.result.current.updateField('max_inflight', '12'));
    expect(hook.result.current.draft?.fields.max_inflight).toBeUndefined();
    let outcome = '';
    await act(async () => { outcome = await hook.result.current.resolveUnknown(); });
    expect(outcome).toBe('pending');
    expect(hook.result.current.draft?.unknownApply).toBe(true);
    expect(request.mock.calls.filter(([path, method]) => path === 'config' && method === 'PUT')).toHaveLength(1);
    expect(refreshSession).not.toHaveBeenCalled();
    remoteApplied = true;
    await act(async () => { outcome = await hook.result.current.resolveUnknown(); });
    expect(outcome).toBe('applied');
    expect(refreshSession).toHaveBeenCalledTimes(1);
    expect(request.mock.calls.filter(([path, method]) => path === 'config' && method === 'PUT')).toHaveLength(1);
  });

  it('keeps acknowledged save successful when its subsequent refresh fails', async () => {
    let gets = 0;
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') {
        gets += 1;
        if (gets > 1) throw new ApiError(0, 'NETWORK', 'refresh disconnected');
        return { toml: original, revision: 1, has_backup: false };
      }
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, max_inflight: 11 } };
      if (path === 'config/validate') return { restart_required: true };
      if (path === 'config' && method === 'PUT') return { revision: 2 };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('max_inflight', '11'));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.prepareSave>>;
    await act(async () => { prepared = await hook.result.current.prepareSave(); });
    let refreshed = true;
    await act(async () => { refreshed = (await hook.result.current.commitPrepared(prepared)).refreshed; });
    expect(refreshed).toBe(false);
    expect(hook.result.current.draft?.revision).toBe(2);
    expect(hook.result.current.draft?.original).toBe(edited);
    expect(hook.result.current.draft?.stale).toBe(false);
    expect(hook.result.current.draft?.unknownApply).toBe(false);
    expect(hook.result.current.error).toBe('app.savedRefreshFailed');
  });

  it('treats a broken successful PUT response as an unknown outcome', async () => {
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/preview') return { toml: edited, settings: { ...baseSettings, max_inflight: 11 } };
      if (path === 'config/validate') return { restart_required: true };
      if (path === 'config' && method === 'PUT') throw new ApiError(200, 'BAD_RESPONSE', 'truncated JSON');
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    act(() => hook.result.current.updateField('max_inflight', '11'));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.prepareSave>>;
    await act(async () => { prepared = await hook.result.current.prepareSave(); });
    await act(async () => { await expect(hook.result.current.commitPrepared(prepared)).rejects.toMatchObject({ code: 'BAD_RESPONSE' }); });
    expect(hook.result.current.draft?.unknownApply).toBe(true);
    expect(hook.result.current.locked).toBe(true);
  });

  it('does not report validation for a newer draft', async () => {
    let release!: (value: unknown) => void;
    const deferred = new Promise<unknown>((resolve) => { release = resolve; });
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/validate') return deferred;
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    let pending!: Promise<unknown>;
    act(() => { pending = hook.result.current.validate(); });
    act(() => hook.result.current.updateField('max_inflight', '12'));
    release({ restart_required: true });
    await expect(pending).rejects.toThrow('Draft changed');
  });

  it('freezes an uncertain rollback and never retries it during reconciliation', async () => {
    let rollbacks = 0;
    const { api } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') return { toml: original, revision: 1, has_backup: true };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/rollback/preview') return { transport_change: null };
      if (path === 'config/rollback') { rollbacks += 1; throw new ApiError(0, 'NETWORK', 'disconnected'); }
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.previewRollback>>;
    await act(async () => { prepared = await hook.result.current.previewRollback(); });
    await act(async () => { await expect(hook.result.current.rollback(prepared)).rejects.toMatchObject({ code: 'NETWORK' }); });
    expect(hook.result.current.draft?.pendingRollback).toEqual({ revision: 1 });
    expect(hook.result.current.locked).toBe(true);
    let outcome = '';
    await act(async () => { outcome = await hook.result.current.resolveUnknown(); });
    expect(outcome).toBe('pending');
    expect(rollbacks).toBe(1);
  });

  it('refreshes session transport when reconciliation confirms an acknowledged rollback', async () => {
    let reads = 0;
    const refreshSession = vi.fn(async () => {});
    const { api, request } = fixture(async (path, method, body) => {
      if (path === 'config' && method === 'GET') {
        reads += 1;
        if (reads === 1) return { toml: original, revision: 1, has_backup: true };
        if (reads === 2) throw new ApiError(0, 'NETWORK', 'refresh disconnected');
        return { toml: edited, revision: 2, has_backup: true };
      }
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings: baseSettings };
      if (path === 'config/rollback/preview') return { transport_change: null };
      if (path === 'config/rollback') return { revision: 2, transport_change: null };
      throw new Error(`Unexpected ${method} ${path}`);
    });
    const hook = mount(api, refreshSession);
    await waitFor(() => expect(hook.result.current.draft?.revision).toBe(1));
    let prepared!: Awaited<ReturnType<typeof hook.result.current.previewRollback>>;
    await act(async () => { prepared = await hook.result.current.previewRollback(); });
    await act(async () => { await hook.result.current.rollback(prepared); });
    expect(hook.result.current.draft?.unknownApply).toBe(true);
    expect(refreshSession).toHaveBeenCalledTimes(1);
    let outcome = '';
    await act(async () => { outcome = await hook.result.current.resolveUnknown(); });
    expect(outcome).toBe('applied');
    expect(refreshSession).toHaveBeenCalledTimes(2);
    expect(request.mock.calls.filter(([path]) => path === 'config/rollback')).toHaveLength(1);
  });
});
