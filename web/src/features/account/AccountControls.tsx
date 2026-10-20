import { useEffect, useRef, useState, type FormEvent } from 'react';
import { UserRound } from 'lucide-react';
import { Drawer } from '../../components/beui';
import { useConfirm } from '../../components/ConfirmProvider';
import { toConfigIssue, useConfig } from '../../config/context';
import { presentIssue, translate, type Language } from '../../i18n';
import { StaleRequest } from '../../session/client';
import { useSession } from '../../session/context';
import { passwordProblem } from './password';
import { TurnstilePanel, type TurnstileView } from './TurnstilePanel';

function AccountForm({ language, setBusy }: { language: Language; setBusy: (busy: boolean) => void }) {
  const { api, rotateCredentials } = useSession();
  const config = useConfig();
  const confirmAction = useConfirm();
  const [username, setUsername] = useState<string | null>(null);
  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [confirmation, setConfirmation] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<ReturnType<typeof toConfigIssue> | null>(null);
  const mounted = useRef(false);
  const mutation = useRef(false);
  const t = (key: string) => translate(`account.${key}`, language);
  useEffect(() => {
    mounted.current = true;
    void api.request<{ username: string }>('account').then((result) => {
      if (mounted.current) setUsername(result.username);
    }).catch((reason) => {
      if (mounted.current && !(reason instanceof StaleRequest)) setError(toConfigIssue(reason));
    });
    return () => { mounted.current = false; };
  }, [api]);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (mutation.current || username === null || config.busy || config.locked) return;
    const trigger = event.currentTarget instanceof HTMLFormElement ? event.currentTarget.querySelector<HTMLElement>('button[type="submit"]') ?? undefined : undefined;
    setError(null);
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(username)) { setError('account.usernameInvalid'); return; }
    // The same 12–256 byte limit as Rust.
    const problem = passwordProblem(newPassword);
    if (problem) { setError(problem); return; }
    if (newPassword !== confirmation) { setError('account.mismatch'); return; }
    // Like signing out: the change ends every session, so an unsaved draft is lost.
    if (config.dirty && !await confirmAction('account.draftConfirm', 'account.continue', trigger)) return;
    mutation.current = true; setSubmitting(true); setBusy(true);
    try { await rotateCredentials(currentPassword, username, newPassword); config.discard(); }
    catch (reason) {
      if (mounted.current && !(reason instanceof StaleRequest)) setError(toConfigIssue(reason));
    } finally {
      mutation.current = false; setBusy(false);
      if (mounted.current) { setSubmitting(false); setCurrentPassword(''); setNewPassword(''); setConfirmation(''); }
    }
  };
  return <section className="security-section">
    <h3>{translate('turnstile.passwordTitle', language)}</h3>
    <p className="muted small">{t('intro')}</p>
    {username === null ? <>{error ? <p className="notice error" role="alert">{presentIssue(error, language)}</p> : <p role="status">{translate('app.reading', language)}</p>}</> :
      <form className="auth-form" onSubmit={(event) => void submit(event)}>
        <label htmlFor="account-username">{t('username')}</label>
        <input id="account-username" value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" maxLength={64} required spellCheck={false} aria-describedby="account-username-help" disabled={submitting} />
        <small id="account-username-help" className="muted">{t('usernameHelp')}</small>
        <label htmlFor="account-current-password">{t('currentPassword')}</label>
        <input id="account-current-password" type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} autoComplete="current-password" required disabled={submitting} />
        <label htmlFor="account-new-password">{t('newPassword')}</label>
        <input id="account-new-password" type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} autoComplete="new-password" required aria-describedby="account-password-help" disabled={submitting} />
        <small id="account-password-help" className="muted">{t('passwordHelp')}</small>
        <label htmlFor="account-confirm-password">{t('confirmPassword')}</label>
        <input id="account-confirm-password" type="password" value={confirmation} onChange={(event) => setConfirmation(event.target.value)} autoComplete="new-password" required disabled={submitting} />
        {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
        <button className="button primary" type="submit" disabled={submitting || config.busy || config.locked}>{submitting ? translate('app.working', language) : t('submit')}</button>
      </form>}
  </section>;
}

function AccountBody({ language, setBusy }: { language: Language; setBusy: (busy: boolean) => void }) {
  const { api } = useSession();
  const [view, setView] = useState<TurnstileView | null>(null);
  const [error, setError] = useState<ReturnType<typeof toConfigIssue> | null>(null);
  useEffect(() => {
    let live = true;
    void api.request<TurnstileView>('account/turnstile').then((result) => { if (live) setView(result); })
      .catch((reason) => { if (live && !(reason instanceof StaleRequest)) setError(toConfigIssue(reason)); });
    return () => { live = false; };
  }, [api]);
  // The password form never waits for the Turnstile settings; only their panel does.
  return <>
    <AccountForm language={language} setBusy={setBusy} />
    {view ? <TurnstilePanel view={view} language={language} setBusy={setBusy} />
      : error ? <p className="notice error" role="alert">{presentIssue(error, language)}</p>
        : <p role="status">{translate('app.reading', language)}</p>}
  </>;
}

export function AccountControls({ language }: { language: Language }) {
  const { api } = useSession();
  const config = useConfig();
  const [openOwner, setOpenOwner] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const owner = api.currentEpoch;
  const open = openOwner === owner;
  const t = (key: string) => translate(`account.${key}`, language);
  const close = () => { setOpenOwner(null); };
  return <>
    <button type="button" className="icon-button" aria-label={t('title')} title={t('title')} disabled={busy || config.busy || config.locked} onClick={() => setOpenOwner(owner)}><UserRound size={18} aria-hidden="true" /></button>
    <Drawer open={open} onClose={close} title={t('title')} closeLabel={translate('ui.closeDialog', language)}>
      {config.locked || config.busy ? <p role="status">{t('locked')}</p> : <AccountBody key={owner} language={language} setBusy={setBusy} />}
    </Drawer>
  </>;
}
