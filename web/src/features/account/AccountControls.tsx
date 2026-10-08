import { useEffect, useRef, useState, type FormEvent } from 'react';
import { UserRound } from 'lucide-react';
import { Drawer } from '../../components/beui';
import { useConfirm } from '../../components/ConfirmProvider';
import { toConfigIssue, useConfig } from '../../config/context';
import { presentIssue, translate, type Language } from '../../i18n';
import { StaleRequest } from '../../session/client';
import { useSession } from '../../session/context';

function AccountForm({ language, setBusy }: { language: Language; setBusy: (busy: boolean) => void }) {
  const { api, rotateCredentials } = useSession();
  const config = useConfig();
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
    if (mutation.current || username === null || config.busy || config.locked || config.dirty) return;
    setError(null);
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(username)) { setError('account.usernameInvalid'); return; }
    const bytes = new TextEncoder().encode(newPassword).length;
    if (bytes < 12 || bytes > 256) { setError('account.passwordInvalid'); return; }
    if (newPassword !== confirmation) { setError('account.mismatch'); return; }
    mutation.current = true; setSubmitting(true); setBusy(true);
    try { await rotateCredentials(currentPassword, username, newPassword); }
    catch (reason) {
      if (mounted.current && !(reason instanceof StaleRequest)) setError(toConfigIssue(reason));
    } finally {
      mutation.current = false; setBusy(false);
      if (mounted.current) { setSubmitting(false); setCurrentPassword(''); setNewPassword(''); setConfirmation(''); }
    }
  };
  return <>
    <p>{t('intro')}</p>
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
        <button className="button primary" type="submit" disabled={submitting || config.busy || config.locked || config.dirty}>{submitting ? translate('app.working', language) : t('submit')}</button>
      </form>}
  </>;
}

export function AccountControls({ language }: { language: Language }) {
  const { api } = useSession();
  const config = useConfig();
  const confirmAction = useConfirm();
  const [openOwner, setOpenOwner] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ReturnType<typeof toConfigIssue> | null>(null);
  const owner = api.currentEpoch;
  const open = openOwner === owner;
  const t = (key: string) => translate(`account.${key}`, language);
  const close = () => { setOpenOwner(null); setError(null); };
  const discard = async (trigger: HTMLElement) => {
    if (!await confirmAction('app.discardHelp', 'app.discard', trigger) || owner !== api.currentEpoch) return;
    try { await config.reload(); }
    catch (reason) { if (owner === api.currentEpoch) setError(toConfigIssue(reason)); }
  };
  return <>
    <button type="button" className="icon-button" aria-label={t('title')} title={t('title')} disabled={busy || config.busy || config.locked} onClick={() => setOpenOwner(owner)}><UserRound size={18} aria-hidden="true" /></button>
    <Drawer open={open} onClose={close} title={t('title')} closeLabel={translate('ui.closeDialog', language)}>
      {config.locked || config.busy ? <p role="status">{t('locked')}</p> : config.dirty ? <>
        <p>{t('draftHelp')}</p>
        <div className="button-group">
          <button type="button" className="button primary" onClick={close}>{t('backToDraft')}</button>
          <button type="button" className="button secondary" onClick={(event) => void discard(event.currentTarget)}>{translate('app.discard', language)}</button>
        </div>
      </> : <AccountForm key={owner} language={language} setBusy={setBusy} />}
      {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    </Drawer>
  </>;
}
