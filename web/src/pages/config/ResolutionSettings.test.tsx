// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { BannerProvider } from '../../components/Banner';
import { ConfirmProvider } from '../../components/ConfirmProvider';
import { ModelError } from '../../model';
import { SettingsPage } from './SettingsPage';

const mocks = vi.hoisted(() => ({ fields: {} as Record<string, unknown>, error: null as unknown, updateField: vi.fn() }));
const settings = {
  listen: '127.0.0.1:53', query_timeout_ms: 2000, tcp_io_timeout_ms: 5000,
  resolution: { mode: 'forward' },
  recursion: { ipv6: false, max_outbound_per_second: 500, max_outbound_inflight: 256, max_depth: 24, cache_entries: 65536, max_queries_per_request: 200 },
  upstreams: { servers: ['udp://192.0.2.53:53'], mode: 'weighted', prefer_h3: false, bootstrap: [], max_parallel: 2, max_extra_inflight: 64, ca_file: null, dot_pool: { enabled: true, max_connections: 4, idle_timeout_ms: 30000 } },
};
vi.mock('../../session/context', () => ({ useSession: () => ({ state: {}, transportChanged: vi.fn() }) }));
vi.mock('../../config/context', () => ({
  toConfigIssue: (reason: unknown) => reason,
  useConfig: () => ({
    draft: { settings, fields: mocks.fields, optional: {}, rules: null, sources: null, stale: false, hasBackup: false },
    locked: false, busy: false, dirty: Object.keys(mocks.fields).length > 0, error: mocks.error,
    updateField: mocks.updateField, setOptional: vi.fn(), setError: vi.fn(), preview: vi.fn(), ensureParsed: vi.fn(),
  }),
}));
afterEach(() => { cleanup(); mocks.fields = {}; mocks.error = null; mocks.updateField.mockReset(); });

const page = (language: 'en' | 'zh-CN' = 'en') => <MemoryRouter><BannerProvider><ConfirmProvider language={language}><SettingsPage pageId="dns" language={language} /></ConfirmProvider></BannerProvider></MemoryRouter>;

describe('resolution mode settings', () => {
  it('keeps recursion limits out of forward mode and edits the mode through the draft', () => {
    render(page());
    expect(screen.getByRole('heading', { name: 'Resolution' })).toBeTruthy();
    expect(screen.queryByRole('heading', { name: 'Recursive resolution' })).toBeNull();
    expect(screen.getByText(/queries to authoritative servers are unencrypted/).className).toBe('group-footer');
    expect(screen.queryByText(/so upstreams are not used/)).toBeNull();
    fireEvent.click(screen.getByRole('radio', { name: 'Recursive (DNSSEC-validating)' }));
    expect(mocks.updateField).toHaveBeenCalledWith('resolution.mode', 'recursive');
  });

  it('shows recursion limits, the switching note and the unused-upstreams hint once the draft is recursive', () => {
    mocks.fields = { 'resolution.mode': 'recursive', 'recursion.max_outbound_per_second': '12' };
    render(page('zh-CN'));
    expect(screen.getByRole('heading', { name: '递归解析' })).toBeTruthy();
    expect(screen.getByText('递归模式下，发往权威服务器的查询不加密，开启 ECS 时还会带上客户端网段；过滤只使用本地规则和在线订阅。切换解析模式会重启 DNS 并清空缓存。').className).toContain('notice-footer');
    expect(screen.getByText(/不使用上游/)).toBeTruthy();
    expect((screen.getByLabelText('出站速率上限（次/秒）') as HTMLInputElement).value).toBe('12');
    // Upstream settings stay editable for a later switch back to forward.
    expect((screen.getByLabelText('地址（每行一个）') as HTMLTextAreaElement).disabled).toBe(false);
  });

  it('keeps a hidden recursion field reachable while it holds a validation error', () => {
    mocks.fields = { 'recursion.max_depth': '3' };
    mocks.error = new ModelError('settings.validation.range', 'recursion.max_depth', { min: 4, max: 64 });
    render(page());
    expect(screen.getByRole('heading', { name: 'Recursive resolution' })).toBeTruthy();
  });
});
