// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '../../components/ConfirmProvider';
import { SessionProvider, useSession } from '../../session/context';
import { LoginPage } from '../../AuthPages';
import { AccountControls } from './AccountControls';
import type { TurnstileView } from './TurnstilePanel';

const reload = vi.hoisted(() => vi.fn());
vi.mock('../updates/reload', () => ({ reloadConsole: reload }));
const config = vi.hoisted(() => ({ busy: false, locked: false, dirty: false, reload: vi.fn(), discard: vi.fn() }));
vi.mock('../../config/context', async (original) => ({ ...await original<typeof import('../../config/context')>(), useConfig: () => config }));

const transport = { scheme: 'http', origin: null };
const signedOut = { setup_required: false, authenticated: false, session: null, transport };
const signedIn = { setup_required: false, authenticated: true, session: { binding: 'binding-1' }, transport };
const noOptions = { turnstile: null, turnstile_skipped: false };
const json = (value: unknown, status = 200) => ({ ok: status < 400, status, json: async () => value }) as Response;
const off: TurnstileView = { turnstile: null, loopback: false };

type Route = (body: Record<string, unknown> | undefined) => Response | Promise<Response>;
function serve(routes: Record<string, Route>) {
  const fetcher = vi.fn(async (url: string, init?: RequestInit) => {
    const route = routes[url];
    if (!route) throw new Error(`Unexpected request ${url}`);
    return route(init?.body ? JSON.parse(init.body as string) as Record<string, unknown> : undefined);
  });
  vi.stubGlobal('fetch', fetcher);
  return (url: string) => (fetcher.mock.calls as unknown as [string, RequestInit][])
    .filter(([called, init]) => called === url && init?.body !== undefined).map(([, init]) => JSON.parse(init.body as string) as Record<string, unknown>);
}

function Harness() {
  const { state } = useSession();
  return <ConfirmProvider language="en">{state.phase === 'login' ? <LoginPage language="en" /> : state.phase === 'ready' ? <><p>READY</p><AccountControls language="en" /></> : null}</ConfirmProvider>;
}

async function password() {
  fireEvent.change(await screen.findByLabelText('Username'), { target: { value: 'admin' } });
  fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'long-admin-password' } });
  await waitFor(() => expect((screen.getByRole('button', { name: 'Sign in' }) as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
}

beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function () { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function () { this.removeAttribute('open'); };
});
beforeEach(() => { config.dirty = false; config.discard.mockReset(); reload.mockReset(); });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); delete window.turnstile; });

describe('sign-in with Turnstile', () => {
  it('signs in with only the username and password while Turnstile is off', async () => {
    let authenticated = false;
    const sent = serve({
      '/api/session': () => json(authenticated ? signedIn : signedOut),
      '/api/login/options': () => json(noOptions),
      '/api/login': () => { authenticated = true; return json(signedIn); },
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    await password();
    expect(await screen.findByText('READY')).toBeTruthy();
    expect(sent('/api/login')).toEqual([{ username: 'admin', password: 'long-admin-password' }]);
    expect(reload).not.toHaveBeenCalled();
  });

  it('sends the Turnstile token with the password and reloads after signing in', async () => {
    const turnstile = { render: vi.fn((_: HTMLElement, options: Record<string, unknown>) => { (options.callback as (token: string) => void)('token-1'); return 'widget'; }), reset: vi.fn(), remove: vi.fn() };
    window.turnstile = turnstile;
    let authenticated = false;
    const sent = serve({
      '/api/session': () => json(authenticated ? signedIn : signedOut),
      '/api/login/options': () => json({ ...noOptions, turnstile: { site_key: 'site-key', action: 'login' } }),
      '/api/login': () => { authenticated = true; return json(signedIn); },
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    await waitFor(() => expect(turnstile.render).toHaveBeenCalled());
    expect(turnstile.render.mock.calls[0][1]).toMatchObject({ sitekey: 'site-key', action: 'login' });
    await password();
    await waitFor(() => expect(reload).toHaveBeenCalledTimes(1));
    expect(sent('/api/login')).toEqual([{ username: 'admin', password: 'long-admin-password', turnstile_token: 'token-1' }]);
  });

  it('reloads a document served before Turnstile was turned on instead of showing a widget its policy blocks', async () => {
    const turnstile = { render: vi.fn(() => 'widget'), reset: vi.fn(), remove: vi.fn() };
    window.turnstile = turnstile;
    let authenticated = false;
    let enabled = false;
    serve({
      '/api/session': () => json(authenticated ? signedIn : signedOut),
      '/api/login/options': () => json(enabled ? { ...noOptions, turnstile: { site_key: 'site-key', action: 'login' } } : noOptions),
      '/api/login': () => { authenticated = true; return json(signedIn); },
      '/api/account': () => json({ username: 'admin' }),
      '/api/account/turnstile': (body) => {
        if (!body) return json(off);
        enabled = true; authenticated = false;
        return json({ reauthentication_required: true });
      },
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    await password();
    fireEvent.click(await screen.findByRole('button', { name: 'Account & security' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Turn on human verification' }));
    fireEvent.change(screen.getByLabelText('Site key'), { target: { value: 'site-key' } });
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'secret-key' } });
    const form = screen.getByRole('button', { name: 'Save human verification' }).closest('form')!;
    fireEvent.change(form.querySelector('#turnstile-password')!, { target: { value: 'long-admin-password' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save human verification' }));
    expect(await screen.findByRole('heading', { name: 'Sign in to PariNS' })).toBeTruthy();
    await waitFor(() => expect(reload).toHaveBeenCalledTimes(1));
    expect(turnstile.render).not.toHaveBeenCalled();
  });

  it('does not send a password without a Turnstile token, and explains the loopback exception', async () => {
    window.turnstile = { render: vi.fn(() => 'widget'), reset: vi.fn(), remove: vi.fn() };
    const sent = serve({
      '/api/session': () => json(signedOut),
      '/api/login/options': () => json({ ...noOptions, turnstile: { site_key: 'site-key', action: 'login' } }),
    });
    const view = render(<SessionProvider><Harness /></SessionProvider>);
    await password();
    expect((await screen.findByRole('alert')).textContent).toBe('Complete the human verification first.');
    expect(sent('/api/login')).toHaveLength(0);
    view.unmount();
    serve({ '/api/session': () => json(signedOut), '/api/login/options': () => json({ ...noOptions, turnstile_skipped: true }) });
    render(<SessionProvider><Harness /></SessionProvider>);
    expect(await screen.findByText(/skips human verification/)).toBeTruthy();
  });

  it('keeps sign-in closed until the sign-in options load, with a retry', async () => {
    let available = false;
    serve({
      '/api/session': () => json(signedOut),
      '/api/login/options': () => available ? json(noOptions) : json({ error: { code: 'STORAGE_BUSY', message: 'busy' } }, 503),
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    expect((await screen.findByRole('alert')).textContent).toContain("Couldn't load the sign-in options");
    expect((screen.getByRole('button', { name: 'Sign in' }) as HTMLButtonElement).disabled).toBe(true);
    available = true;
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
    expect((screen.getByRole('button', { name: 'Sign in' }) as HTMLButtonElement).disabled).toBe(false);
  });
});

describe('Turnstile settings', () => {
  async function openSettings() {
    fireEvent.click(await screen.findByRole('button', { name: 'Account & security' }));
    await screen.findByRole('heading', { name: /Human verification \(Cloudflare Turnstile\)/ });
  }

  it('turns Turnstile on with the current password, then signs everyone out', async () => {
    const sent = serve({
      '/api/session': () => json(signedIn),
      '/api/login/options': () => json(noOptions),
      '/api/account': () => json({ username: 'admin' }),
      '/api/account/turnstile': (body) => body ? json({ reauthentication_required: true }) : json(off),
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    await openSettings();
    fireEvent.click(screen.getByRole('button', { name: 'Turn on human verification' }));
    fireEvent.change(screen.getByLabelText('Site key'), { target: { value: 'site-key' } });
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'secret-key' } });
    const form = screen.getByRole('button', { name: 'Save human verification' }).closest('form')!;
    fireEvent.change(form.querySelector('#turnstile-password')!, { target: { value: 'long-admin-password' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save human verification' }));
    expect(await screen.findByRole('heading', { name: 'Sign in to PariNS' })).toBeTruthy();
    expect(screen.getByRole('status').textContent).toContain('Human verification updated');
    expect(sent('/api/account/turnstile')).toEqual([{ current_password: 'long-admin-password', turnstile: { site_key: 'site-key', secret_key: 'secret-key' } }]);
  });

  it('keeps the secret write-only, warns about loopback and clears with the password', async () => {
    const on: TurnstileView = { turnstile: { site_key: 'site-key', secret_set: true, test_keys: null }, loopback: true };
    const sent = serve({
      '/api/session': () => json(signedIn),
      '/api/login/options': () => json(noOptions),
      '/api/account': () => json({ username: 'admin' }),
      '/api/account/turnstile': (body) => body ? json({ reauthentication_required: true }) : json(on),
    });
    const view = render(<SessionProvider><Harness /></SessionProvider>);
    await openSettings();
    expect(screen.getByText(/through a tunnel or proxy on this server, every sign-in skips human verification/)).toBeTruthy();
    expect(screen.queryByText(/^Test keys/)).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Change keys' }));
    expect((screen.getByLabelText('Site key') as HTMLInputElement).value).toBe('site-key');
    expect(screen.getByText('Set')).toBeTruthy();
    fireEvent.change(screen.getByLabelText('Current password', { selector: '#turnstile-password' }), { target: { value: 'long-admin-password' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save human verification' }));
    await screen.findByRole('heading', { name: 'Sign in to PariNS' });
    expect(sent('/api/account/turnstile')).toEqual([{ current_password: 'long-admin-password', turnstile: { site_key: 'site-key' } }]);
    view.unmount();
    serve({
      '/api/session': () => json(signedIn),
      '/api/login/options': () => json(noOptions),
      '/api/account': () => json({ username: 'admin' }),
      '/api/account/turnstile': (body) => body ? json({ reauthentication_required: true }) : json(on),
    });
    render(<SessionProvider><Harness /></SessionProvider>);
    await openSettings();
    fireEvent.click(screen.getByRole('button', { name: 'Clear and turn off' }));
    fireEvent.change(screen.getByLabelText('Current password', { selector: '#turnstile-password' }), { target: { value: 'long-admin-password' } });
    fireEvent.click(screen.getAllByRole('button', { name: 'Clear and turn off' }).at(-1)!);
    await screen.findByRole('heading', { name: 'Sign in to PariNS' });
  });

  it('names Cloudflare test keys and warns about loopback only while Turnstile is on', async () => {
    let view: TurnstileView = { loopback: true, turnstile: { site_key: '1x00000000000000000000AA', secret_set: true, test_keys: 'pass' } };
    serve({
      '/api/session': () => json(signedIn),
      '/api/login/options': () => json(noOptions),
      '/api/account': () => json({ username: 'admin' }),
      '/api/account/turnstile': () => json(view),
    });
    const rendered = render(<SessionProvider><Harness /></SessionProvider>);
    await openSettings();
    expect(screen.getByText('Test keys: every sign-in passes human verification, which gives no protection.')).toBeTruthy();
    rendered.unmount();
    view = { ...off, loopback: true };
    render(<SessionProvider><Harness /></SessionProvider>);
    await openSettings();
    expect(screen.queryByText(/tunnel or proxy/)).toBeNull();
  });
});
