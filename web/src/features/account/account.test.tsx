// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '../../components/ConfirmProvider';
import { SessionProvider, useSession } from '../../session/context';
import { LoginPage } from '../../AuthPages';
import { AccountControls } from './AccountControls';
import type { Language } from '../../i18n';

const config = vi.hoisted(() => ({ busy: false, locked: false, dirty: false, reload: vi.fn(), discard: vi.fn() }));
vi.mock('../../config/context', async (original) => ({ ...await original<typeof import('../../config/context')>(), useConfig: () => config }));
const session = { setup_required: false, authenticated: true, session: { binding: 'account-owner' },
  transport: { scheme: 'http', origin: null } };
const json = (value: unknown) => ({ ok: true, status: 200, json: async () => value }) as Response;
const turnstile = { turnstile: null, loopback: false };
function Harness({ language = 'en' }: { language?: Language }) {
  const { state } = useSession();
  return <ConfirmProvider language={language}>{state.phase === 'ready' ? <AccountControls language={language} /> : state.phase === 'login' ? <LoginPage language={language} /> : null}</ConfirmProvider>;
}
const tree = (language: Language = 'en') => <SessionProvider><Harness language={language} /></SessionProvider>;
async function open() {
  fireEvent.click(await screen.findByRole('button', { name: 'Account & security' }));
  await screen.findByLabelText('Current password');
}
function passwords(current = 'current-password', next = 'new-password-long', confirm = next) {
  fireEvent.change(screen.getByLabelText('Current password'), { target: { value: current } });
  fireEvent.change(screen.getByLabelText('New password'), { target: { value: next } });
  fireEvent.change(screen.getByLabelText('Confirm new password'), { target: { value: confirm } });
}
beforeAll(() => {
  HTMLDialogElement.prototype.showModal = function () { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function () { this.removeAttribute('open'); };
});
beforeEach(() => { config.busy = false; config.locked = false; config.dirty = false; config.reload.mockReset(); config.discard.mockReset(); });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
function requests(mutation: () => Promise<Response> = async () => json({ reauthentication_required: true })) {
  const fetcher = vi.fn(async (url: string) => {
    if (url === '/api/session') return json(session);
    if (url === '/api/account') return json({ username: 'admin' });
    if (url === '/api/account/turnstile') return json(turnstile);
    if (url === '/api/account/credentials') return mutation();
    throw new Error(`Unexpected request ${url}`);
  });
  vi.stubGlobal('fetch', fetcher);
  return fetcher;
}
describe('administrator credential controls', () => {
  it('validates username, password length and confirmation before sending only the accepted request', async () => {
    const fetcher = requests();
    render(tree()); await open();
    expect(screen.getByText('At least 12 characters.')).toBeTruthy();
    // The console counts UTF-8 bytes, as the server does: 11 ASCII bytes are too short, 258 bytes too long.
    passwords('current-password', 'elevenbytes');
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    expect(screen.getByRole('alert').textContent).toBe('The password needs at least 12 characters.');
    passwords('current-password', '一'.repeat(86));
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    expect(screen.getByRole('alert').textContent).toContain('at most 256 bytes');
    passwords('current-password', 'new-password-long', 'different');
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    expect(screen.getByRole('alert').textContent).toContain("don't match");
    passwords();
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'invalid name' } });
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    expect(screen.getByRole('alert').textContent).toContain('English letters');
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'new_admin' } });
    // Four Chinese characters are 12 bytes, which the server accepts.
    passwords('current-password', '一二三四');
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    await screen.findByRole('heading', { name: 'Sign in to PariNS' });
    expect(screen.getByRole('status').textContent).toContain('All previous sessions');
    expect(screen.queryByLabelText('Current password')).toBeNull();
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(1);
  });

  it('shows wrong-current-password in the selected language without logging out and erases secret inputs', async () => {
    requests(async () => ({ ok: false, status: 403, json: async () => ({ error: { code: 'CURRENT_PASSWORD_INCORRECT', message: 'Incorrect' } }) }) as Response);
    const view = render(tree()); await open(); passwords();
    view.rerender(tree('zh-CN'));
    expect((screen.getByLabelText('当前密码') as HTMLInputElement).value).toBe('current-password');
    fireEvent.click(screen.getByRole('button', { name: '修改并重新登录' }));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('当前密码不正确'));
    for (const label of ['当前密码', '新密码', '确认新密码']) expect((screen.getByLabelText(label) as HTMLInputElement).value).toBe('');
    expect(screen.getByRole('dialog', { name: '账号与安全' })).toBeTruthy();
  });

  it('clears secrets on Escape close and restores the trigger focus', async () => {
    requests(); render(tree());
    const trigger = await screen.findByRole('button', { name: 'Account & security' });
    trigger.focus(); await open(); passwords();
    fireEvent(screen.getByRole('dialog'), new Event('cancel', { bubbles: false, cancelable: true }));
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(document.activeElement).toBe(trigger);
    await open();
    for (const label of ['Current password', 'New password', 'Confirm new password']) expect((screen.getByLabelText(label) as HTMLInputElement).value).toBe('');
  });

  it('changes the account with an unsaved draft after a sign-out style confirmation', async () => {
    config.dirty = true;
    const fetcher = requests(); render(tree()); await open();
    passwords();
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    expect(await screen.findByText('Changing the account signs you out, and unsaved settings changes will be lost.')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByText(/unsaved settings changes will be lost/)).toBeNull());
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(0);
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Continue' }));
    await screen.findByRole('heading', { name: 'Sign in to PariNS' });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(1);
    expect(config.discard).toHaveBeenCalledTimes(1);
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/config')).toHaveLength(0);
  });

  it.each(['busy', 'locked'] as const)('disables entry while configuration is %s', async (lock) => {
    config[lock] = true; requests(); render(tree());
    const trigger = await screen.findByRole('button', { name: 'Account & security' }) as HTMLButtonElement;
    expect(trigger.disabled).toBe(true);
    fireEvent.click(trigger);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('masks unknown results and never retries the mutation after the form closes', async () => {
    let reject!: (reason: Error) => void;
    const fetcher = requests(() => new Promise((_resolve, fail) => { reject = fail; }));
    render(tree()); await open(); passwords();
    fireEvent.click(screen.getByRole('button', { name: 'Change and sign in again' }));
    fireEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    expect(screen.queryByLabelText('Current password')).toBeNull();
    expect((screen.getByRole('button', { name: 'Account & security' }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => { reject(new TypeError('connection lost')); });
    await screen.findByRole('heading', { name: 'Sign in to PariNS' });
    expect(screen.getByRole('status').textContent).toContain("change couldn't be confirmed");
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(1);
  });
});
