// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { SessionProvider, useSession } from './session/context';
import { SetupPage } from './AuthPages';
import type { Language } from './i18n';

const template = 'listen = "127.0.0.1:5353"\nmax_tcp_connections = 64\n\n[upstreams]\nservers = ["udp://127.0.0.1:5354"]\nmode = "weighted"\nprefer_h3 = false\nbootstrap = []\n';
const setupView = { setup_required: true, authenticated: false, session: null, transport: { scheme: 'http', origin: null } };
const json = (value: unknown, status = 200) => ({ ok: status < 400, status, json: async () => value }) as Response;

function Harness({ language }: { language: Language }) {
  const { state } = useSession();
  return state.phase === 'setup' ? <SetupPage language={language} /> : null;
}
function requests(setup: () => Response) {
  const fetcher = vi.fn(async (url: string, _init?: RequestInit) => {
    if (url === '/api/session') return json(setupView);
    if (url === '/api/template') return json({ toml: template });
    if (url === '/api/setup/preview') return json({ valid: true, transport_change: null });
    if (url === '/api/setup') return setup();
    throw new Error(`Unexpected request ${url}`);
  });
  vi.stubGlobal('fetch', fetcher);
  return fetcher;
}
async function account() {
  fireEvent.change(await screen.findByLabelText('Setup token'), { target: { value: 'a'.repeat(64) } });
  fireEvent.change(screen.getByLabelText('Admin password'), { target: { value: 'long-admin-password' } });
  fireEvent.change(screen.getByLabelText('Confirm password'), { target: { value: 'long-admin-password' } });
  await waitFor(() => expect(screen.getByRole('button', { name: 'Continue' })).toHaveProperty('disabled', false));
  fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
  await screen.findByRole('radiogroup', { name: 'DNS listener' });
}
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

describe('first-run setup wizard', () => {
  it('needs only a listening mode and an upstream preset, and explains an occupied port 53', async () => {
    const fetcher = requests(() => json({ error: { code: 'CONFIG_CONFLICT', message: 'bind DNS TCP 0.0.0.0:53: Address already in use (os error 98)' } }, 422));
    render(<SessionProvider><Harness language="en" /></SessionProvider>);
    await account();
    expect(screen.getByRole('radio', { name: 'Local testing only' }).getAttribute('aria-checked')).toBe('true');
    expect(screen.getByText(/Listens on 127\.0\.0\.1:5353/)).toBeTruthy();
    expect((screen.getByLabelText(/^Upstream DNS/) as HTMLSelectElement).value).toBe('cloudflare');
    expect(screen.queryByLabelText('Addresses (one per line)')).toBeNull();
    fireEvent.click(screen.getByRole('radio', { name: 'LAN & internet (:53)' }));
    expect(screen.getByText(/Listens on 0\.0\.0\.0:53/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
    expect(await screen.findByText('0.0.0.0:53')).toBeTruthy();
    expect(screen.getByText('https://cloudflare-dns.com/dns-query')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Save and start DNS' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('Port 53 is already in use');
    expect(alert.textContent).toContain('Address already in use');
    expect(alert.textContent).toContain('choose “Local testing only”');
    const sent = (fetcher.mock.calls as unknown as [string, RequestInit][]).find(([url]) => url === '/api/setup')![1];
    const toml = (JSON.parse(sent.body as string) as { toml: string }).toml;
    expect(toml).toContain('listen = "0.0.0.0:53"');
    expect(toml).toContain('servers = ["https://cloudflare-dns.com/dns-query"]');
    expect(toml).toContain('bootstrap = ["1.1.1.1:53","1.0.0.1:53"]');
    expect(sent.headers).toMatchObject({ 'X-PariNS-Setup': 'a'.repeat(64) });
  });

  it('keeps other setup errors generic and offers custom upstreams', async () => {
    requests(() => json({ error: { code: 'INVALID_CONFIG', message: 'hostname upstreams require explicit bootstrap IP:port addresses' } }, 422));
    render(<SessionProvider><Harness language="en" /></SessionProvider>);
    await account();
    fireEvent.change(screen.getByLabelText(/^Upstream DNS/), { target: { value: 'custom' } });
    fireEvent.change(screen.getByLabelText('Addresses (one per line)'), { target: { value: 'tls://dns.example:853' } });
    fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
    expect(await screen.findByText('tls://dns.example:853')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Save and start DNS' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('The configuration needs fixing');
    expect(alert.textContent).not.toContain('Port 53');
  });

  it('defaults Chinese users to a domestic preset', async () => {
    requests(() => json({}));
    render(<SessionProvider><Harness language="zh-CN" /></SessionProvider>);
    fireEvent.change(await screen.findByLabelText('设置令牌'), { target: { value: 'a'.repeat(64) } });
    fireEvent.change(screen.getByLabelText('管理员密码'), { target: { value: 'long-admin-password' } });
    fireEvent.change(screen.getByLabelText('确认密码'), { target: { value: 'long-admin-password' } });
    await waitFor(() => expect(screen.getByRole('button', { name: '继续' })).toHaveProperty('disabled', false));
    fireEvent.click(screen.getByRole('button', { name: '继续' }));
    expect(((await screen.findByLabelText(/^上游 DNS/)) as HTMLSelectElement).value).toBe('alidns');
  });

  it('names the first missing or invalid account detail', async () => {
    requests(() => json({}));
    render(<SessionProvider><Harness language="en" /></SessionProvider>);
    const token = await screen.findByLabelText('Setup token');
    const form = token.closest('form')!;
    const submitted = async (message: string) => { fireEvent.submit(form); expect((await screen.findByRole('alert')).textContent).toBe(message); };
    expect(screen.getByText('At least 12 characters.')).toBeTruthy();
    await submitted('Enter the setup token.');
    fireEvent.change(token, { target: { value: 'a'.repeat(64) } });
    fireEvent.change(screen.getByLabelText('Admin username'), { target: { value: ' ' } });
    await submitted('Enter a username.');
    fireEvent.change(screen.getByLabelText('Admin username'), { target: { value: 'admin' } });
    fireEvent.change(screen.getByLabelText('Admin password'), { target: { value: 'short' } });
    await submitted('The password needs at least 12 characters.');
    fireEvent.change(screen.getByLabelText('Admin password'), { target: { value: 'long-admin-password' } });
    fireEvent.change(screen.getByLabelText('Confirm password'), { target: { value: 'other-admin-password' } });
    await submitted("The new passwords don't match.");
  });
});
