import { useEffect, useRef, useState, type FormEvent } from 'react';
import { useConfirm } from '../../components/ConfirmProvider';
import { toConfigIssue, useConfig } from '../../config/context';
import { presentIssue, translate, type Language } from '../../i18n';
import { StaleRequest } from '../../session/client';
import { useSession } from '../../session/context';

/** What GET /api/account/turnstile reports; never the secret key. */
export interface TurnstileView {
  /** `test_keys`: Cloudflare test keys decide every sign-in alike. */
  turnstile: { site_key: string; secret_set: boolean; test_keys: 'pass' | 'fail' | null } | null;
  loopback: boolean;
}

/** Turnstile keys. Setting or clearing them signs everyone out. */
export function TurnstilePanel({ view, language, setBusy }: { view: TurnstileView; language: Language; setBusy: (busy: boolean) => void }) {
  const { changeTurnstile } = useSession();
  const config = useConfig();
  const confirmAction = useConfirm();
  const [action, setAction] = useState<'save' | 'clear' | null>(null);
  const [siteKey, setSiteKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [replaceSecret, setReplaceSecret] = useState(true);
  const [password, setPassword] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<ReturnType<typeof toConfigIssue> | null>(null);
  const mounted = useRef(false);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const t = (key: string) => translate(`turnstile.${key}`, language);
  const current = view.turnstile;
  const locked = submitting || config.busy || config.locked;
  const open = (next: 'save' | 'clear') => {
    setAction(next); setSiteKey(current?.site_key ?? ''); setSecretKey(''); setReplaceSecret(!current); setPassword(''); setError(null);
  };
  const close = () => { setAction(null); setSecretKey(''); setPassword(''); setError(null); };
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!action || locked) return;
    const trigger = event.currentTarget instanceof HTMLFormElement ? event.currentTarget.querySelector<HTMLElement>('button[type="submit"]') ?? undefined : undefined;
    setError(null);
    // Like signing out: the change ends every session, so an unsaved draft is lost.
    if (config.dirty && !await confirmAction('turnstile.draftConfirm', 'account.continue', trigger)) return;
    setSubmitting(true); setBusy(true);
    try {
      const turnstile = action === 'clear' ? null : { site_key: siteKey.trim(), secret_key: replaceSecret ? secretKey.trim() : undefined };
      await changeTurnstile({ current_password: password, turnstile });
      config.discard();
    } catch (reason) {
      if (mounted.current && !(reason instanceof StaleRequest)) setError(toConfigIssue(reason));
    } finally {
      setBusy(false);
      if (mounted.current) { setSubmitting(false); setPassword(''); }
    }
  };
  return <section className="security-section">
    <h3>{t('title')}<span className="security-status">{t(current ? 'on' : 'off')}</span></h3>
    <p className="muted small">{t('help')}</p>
    {current?.test_keys && <p className="notice" role="note">{t(current.test_keys === 'pass' ? 'testKeysPass' : 'testKeysFail')}</p>}
    {current && view.loopback && <p className="notice" role="note">{t('loopbackWarning')}</p>}
    {!action && <div className="button-group">
      <button className="button secondary" type="button" disabled={locked} onClick={() => open('save')}>{t(current ? 'edit' : 'enable')}</button>
      {current && <button className="button danger" type="button" disabled={locked} onClick={() => open('clear')}>{t('clear')}</button>}
    </div>}
    {action && <form className="auth-form security-confirm" onSubmit={(event) => void submit(event)}>
      {action === 'save' && <>
        <label htmlFor="turnstile-site-key">{t('siteKey')}</label>
        <input id="turnstile-site-key" value={siteKey} onChange={(event) => setSiteKey(event.target.value)} maxLength={256} required spellCheck={false} autoComplete="off" disabled={submitting} />
        <label htmlFor="turnstile-secret-key">{t('secretKey')}</label>
        {replaceSecret
          ? <input id="turnstile-secret-key" type="password" value={secretKey} onChange={(event) => setSecretKey(event.target.value)} maxLength={256} required spellCheck={false} autoComplete="off" disabled={submitting} aria-describedby="turnstile-secret-help" />
          : <div className="button-group"><strong id="turnstile-secret-key">{t('secretSet')}</strong><button className="button quiet" type="button" onClick={() => setReplaceSecret(true)} disabled={submitting}>{t('secretReplace')}</button></div>}
        {replaceSecret && current && <button className="button quiet" type="button" onClick={() => { setReplaceSecret(false); setSecretKey(''); }} disabled={submitting}>{t('secretKeep')}</button>}
        <small id="turnstile-secret-help" className="muted">{t('secretHelp')}</small>
      </>}
      <p className="muted small">{t('confirmHelp')}</p>
      <label htmlFor="turnstile-password">{translate('account.currentPassword', language)}</label>
      <input id="turnstile-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} autoComplete="current-password" required disabled={submitting} />
      {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
      <div className="button-group">
        <button className="button secondary" type="button" onClick={close} disabled={submitting}>{t('cancel')}</button>
        <button className="button primary" type="submit" disabled={locked}>{submitting ? translate('app.working', language) : t(action === 'clear' ? 'clear' : 'save')}</button>
      </div>
    </form>}
  </section>;
}
