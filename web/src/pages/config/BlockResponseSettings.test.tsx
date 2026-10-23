// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { BannerProvider } from '../../components/Banner';
import { ConfirmProvider } from '../../components/ConfirmProvider';
import { SettingsPage } from './SettingsPage';

const mocks = vi.hoisted(() => ({ fields: {} as Record<string, unknown>, updateField: vi.fn() }));
const settings = {
  filter_file: null,
  filter: { enabled: true, block_exact: [], block_suffix: [], allow_exact: [], allow_suffix: [], block_response: 'nodata', blocked_ttl_secs: 10 },
  filter_subscriptions: { enabled: false, max_rules: 1000, max_memory_bytes: 1048576, max_disk_bytes: 1048576, sources: [] },
};
vi.mock('../../session/context', () => ({ useSession: () => ({ state: {}, transportChanged: vi.fn() }) }));
vi.mock('../../features/subscriptions/SubscriptionsPanel', () => ({ SubscriptionsPanel: () => null }));
vi.mock('../../config/context', () => ({
  toConfigIssue: (reason: unknown) => reason,
  useConfig: () => ({
    draft: { settings, fields: mocks.fields, optional: {}, rules: null, sources: null, stale: false, hasBackup: false },
    locked: false, busy: false, dirty: Object.keys(mocks.fields).length > 0, error: null,
    updateField: mocks.updateField, setOptional: vi.fn(), setError: vi.fn(), preview: vi.fn(), ensureParsed: vi.fn(),
  }),
}));
afterEach(() => { cleanup(); mocks.fields = {}; mocks.updateField.mockReset(); });

const page = (language: 'en' | 'zh-CN' = 'en') => <MemoryRouter><BannerProvider><ConfirmProvider language={language}><SettingsPage pageId="filters" language={language} /></ConfirmProvider></BannerProvider></MemoryRouter>;

describe('block response settings', () => {
  it('edits the global response and TTL next to the local rules, with addresses hidden until Custom IP', () => {
    render(page());
    const headings = screen.getAllByRole('heading', { level: 2 }).map((heading) => heading.textContent);
    expect(headings.indexOf('Block response')).toBe(headings.indexOf('Local rules') + 1);
    expect(screen.getByText(/page\.example 192\.0\.2\.10 2001:db8::10/)).toBeTruthy();
    expect(screen.queryByLabelText('IPv4 address')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Block response: Empty answer (NODATA)' }));
    fireEvent.click(screen.getByRole('option', { name: 'Custom IP' }));
    expect(mocks.updateField).toHaveBeenCalledWith('filter.block_response', 'custom_ip');
    expect((screen.getAllByLabelText(/Blocked answer TTL/)[0] as HTMLInputElement).value).toBe('10');
  });

  it('shows both address fields for a Custom IP draft, in both languages', () => {
    mocks.fields = { 'filter.block_response': 'custom_ip', 'filter.block_ipv4': '192.0.2.1' };
    const view = render(page());
    expect((screen.getByLabelText('IPv4 address') as HTMLInputElement).value).toBe('192.0.2.1');
    expect((screen.getByLabelText('IPv6 address') as HTMLInputElement).value).toBe('');
    view.rerender(page('zh-CN'));
    expect(screen.getByRole('heading', { name: '拦截方式' })).toBeTruthy();
    expect(screen.getByRole('button', { name: '拦截方式: 指定 IP' })).toBeTruthy();
    expect((screen.getByLabelText('IPv4 地址') as HTMLInputElement).value).toBe('192.0.2.1');
  });
});
