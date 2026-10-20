// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { ApiError, type ApiClient, type TransportChange } from './session/client';
import { ConfigProvider, useConfig } from './config/context';
import { ConfirmProvider } from './components/ConfirmProvider';
import { BannerProvider } from './components/Banner';
import { SaveBar } from './App';

const transportChanged = vi.fn();
vi.mock('./session/context', () => ({ useSession: () => ({ transportChanged }), SessionProvider: ({ children }: { children: unknown }) => children }));

beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute('open'); };
});
afterEach(() => { cleanup(); transportChanged.mockReset(); });

const original = 'max_inflight = 10\n';
const edited = 'max_inflight = 11\n';
const settings = { max_inflight: 10, cache: { rules: [] }, web: null, dot: null, doh: null, doq: null };

function mount(change: TransportChange | null, restart = true) {
  let saved = false;
  const request = vi.fn(async (path: string, method = 'GET', body?: unknown) => {
    if (path === 'config' && method === 'GET') return saved ? { toml: edited, revision: 2, has_backup: true } : { toml: original, revision: 1, has_backup: false };
    if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings };
    if (path === 'config/preview') return { toml: edited, settings: { ...settings, max_inflight: 11 }, transport_change: change };
    if (path === 'config/validate') return { valid: true, restart_required: restart, transport_change: change };
    if (path === 'config' && method === 'PUT') { saved = true; return { revision: 2, restart_required: restart, transport_change: change }; }
    throw new Error(`Unexpected ${method} ${path}`);
  });
  const api = { request } as unknown as ApiClient;
  function Harness() {
    const config = useConfig();
    return <><button type="button" disabled={!config.draft} onClick={() => config.updateField('max_inflight', '11')}>Edit</button><SaveBar language="en" /></>;
  }
  render(<BannerProvider><ConfigProvider api={api} active refreshSession={async () => {}}><ConfirmProvider language="en"><Harness /></ConfirmProvider></ConfigProvider></BannerProvider>);
  const puts = () => request.mock.calls.filter(([path, method]) => path === 'config' && method === 'PUT');
  return { request, puts };
}

async function edit() {
  await waitFor(() => expect((screen.getByRole('button', { name: 'Edit' }) as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
  expect(screen.getByText('1 unsaved change')).toBeTruthy();
  expect(screen.queryByRole('button', { name: /Preview/ })).toBeNull();
}

describe('one-click save', () => {
  it('saves without a confirmation and reports the actual effect', async () => {
    const { puts } = mount(null);
    await edit();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(puts()).toHaveLength(1));
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(puts()[0][2]).toEqual({ toml: edited, revision: 1 });
    await waitFor(() => expect(screen.getByText('Saved. DNS restarted and the cache was cleared.')).toBeTruthy());
  });

  it('reports a save applied without restarting DNS', async () => {
    mount(null, false);
    await edit();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.getByText('Saved and applied without restarting DNS.')).toBeTruthy());
  });

  it('asks first when the console address or protocol changes', async () => {
    const change = { from: 'http', to: 'https', next_origin: 'https://dns.example.com:3000', reauthenticate: true, requires_http_confirmation: false } as const;
    const { puts } = mount(change);
    await edit();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    const dialog = await screen.findByRole('dialog', { name: 'Confirm' });
    expect(dialog.textContent).toContain('moves to HTTPS at https://dns.example.com:3000');
    expect(puts()).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(puts()).toHaveLength(1));
    expect(puts()[0][2]).toEqual({ toml: edited, revision: 1 });
    await waitFor(() => expect(transportChanged).toHaveBeenCalledWith(change));
  });

  it('asks before an HTTPS to HTTP downgrade and sends nothing when cancelled', async () => {
    const change = { from: 'https', to: 'http', next_origin: 'http://192.0.2.20:3000', reauthenticate: true, requires_http_confirmation: true } as const;
    const { puts } = mount(change);
    await edit();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    let dialog = await screen.findByRole('dialog', { name: 'Confirm' });
    expect(dialog.textContent).toContain('unencrypted HTTP');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(puts()).toHaveLength(0);
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    dialog = await screen.findByRole('dialog', { name: 'Confirm' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(puts()).toHaveLength(1));
    expect(puts()[0][2]).toEqual({ toml: edited, revision: 1, allow_http_downgrade: true });
  });

  it('confirms a save whose response was lost by reading the configuration, without resending it', async () => {
    let saved = false;
    const request = vi.fn(async (path: string, method = 'GET', body?: unknown) => {
      if (path === 'config' && method === 'GET') return saved ? { toml: edited, revision: 2, has_backup: true } : { toml: original, revision: 1, has_backup: false };
      if (path === 'config/parse') return { toml: (body as { toml: string }).toml, settings };
      if (path === 'config/preview') return { toml: edited, settings: { ...settings, max_inflight: 11 }, transport_change: null };
      if (path === 'config/validate') return { valid: true, restart_required: false, transport_change: null };
      if (path === 'config' && method === 'PUT') { saved = true; throw new ApiError(0, 'NETWORK', 'connection lost'); }
      throw new Error(`Unexpected ${method} ${path}`);
    });
    function Harness() {
      const config = useConfig();
      return <><button type="button" disabled={!config.draft} onClick={() => config.updateField('max_inflight', '11')}>Edit</button><SaveBar language="en" /></>;
    }
    render(<BannerProvider><ConfigProvider api={{ request } as unknown as ApiClient} active refreshSession={async () => {}}><ConfirmProvider language="en"><Harness /></ConfirmProvider></ConfigProvider></BannerProvider>);
    await edit();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.getByText('Saved and applied.')).toBeTruthy());
    expect(screen.queryByRole('button', { name: 'Check save result' })).toBeNull();
    expect(request.mock.calls.filter(([path, method]) => path === 'config' && method === 'PUT')).toHaveLength(1);
  });
});
