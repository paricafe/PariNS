import { useEffect, useState, type FormEvent } from 'react';
import { networkTemplate } from './model';
import { hasTranslation, translate, type Language } from './i18n';
import { ApiError, CookieUnavailable, WebLocksUnavailable } from './session/client';
import { useSession } from './session/context';

function present(error: unknown, language: Language): string {
  if (error instanceof CookieUnavailable) return translate('app.cookieUnavailable', language);
  if (error instanceof WebLocksUnavailable) return translate('app.webLocksUnavailable', language);
  if (error instanceof ApiError && hasTranslation(`api.${error.code}`)) return translate(`api.${error.code}`, language, { detail: error.message, status: error.status });
  return error instanceof Error ? error.message : translate('app.offline', language);
}

export function LoginPage({ language }: { language: Language }) {
  const { login } = useSession();
  const [username, setUsername] = useState(''); const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const t = (key: string) => translate(`ui.${key}`, language);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null);
    try { await login(username.trim(), password); setPassword(''); }
    catch (reason) { setError(present(reason, language)); }
    finally { setBusy(false); }
  };
  return <section className="auth-wrap"><div className="auth-card">
    <p className="eyebrow">{t('welcome')}</p><h1>{t('loginTitle')}</h1><p className="muted">{t('loginIntro')}</p>
    <form onSubmit={(event) => void submit(event)} className="auth-form">
      <label htmlFor="login-username">{t('username')}</label><input id="login-username" value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" required maxLength={64} />
      <label htmlFor="login-password">{t('password')}</label><input id="login-password" value={password} onChange={(event) => setPassword(event.target.value)} type="password" autoComplete="current-password" required />
      {error && <p className="error" role="alert">{error}</p>}
      <button className="button primary" type="submit" disabled={busy}>{busy ? translate('app.working', language) : t('login')}</button>
    </form><p className="muted small">{t('sessionHelp')}</p>
  </div></section>;
}

function socketAddress(value: string): boolean {
  const match = /^(?:\[([0-9a-fA-F:.]+)\]|(\d{1,3}(?:\.\d{1,3}){3})):(\d{1,5})$/.exec(value);
  return Boolean(match && Number(match[3]) > 0 && Number(match[3]) <= 65535 && (match[1] ? match[1].includes(':') : match[2].split('.').every((part) => Number(part) <= 255)));
}

export function SetupPage({ language }: { language: Language }) {
  const { api, setup } = useSession();
  const [template, setTemplate] = useState<string | null>(null);
  const [step, setStep] = useState(0);
  const [token, setToken] = useState(''); const [username, setUsername] = useState('admin');
  const [password, setPassword] = useState(''); const [confirmPassword, setConfirmPassword] = useState('');
  const [listen, setListen] = useState('127.0.0.1:5353'); const [servers, setServers] = useState('');
  const [bootstrap, setBootstrap] = useState(''); const [mode, setMode] = useState('weighted'); const [preferH3, setPreferH3] = useState(false);
  const [toml, setToml] = useState(''); const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const [templateError, setTemplateError] = useState<unknown>(null);
  const t = (key: string) => translate(`ui.${key}`, language);
  useEffect(() => { void api.request<{ toml: string }>('template').then((result) => setTemplate(result.toml)).catch(setTemplateError); }, [api]);
  const next = () => {
    setError(null);
    if (step === 0 && (password !== confirmPassword || password.length < 12 || !token.trim() || !username.trim())) { setError(translate('app.passwordMismatch', language)); return; }
    if (step === 1) {
      if (!socketAddress(listen.trim())) { setError(translate('app.addressFormat', language)); return; }
      const upstreams = servers.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
      if (!upstreams.length) { setError(translate('settings.validation.required', language)); return; }
      if (!template) { setError(translate('app.startFailed', language)); return; }
      try { setToml(networkTemplate(template, listen.trim(), { servers: upstreams, bootstrap: bootstrap.split(/\r?\n/).map((line) => line.trim()).filter(Boolean), mode, prefer_h3: preferH3 })); }
      catch (reason) { setError(present(reason, language)); return; }
    }
    setStep((current) => Math.min(current + 1, 2));
  };
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (step < 2) { next(); return; }
    setBusy(true); setError(null);
    try { await setup(username.trim(), password, toml, token.trim()); setToken(''); setPassword(''); setConfirmPassword(''); }
    catch (reason) { setError(present(reason, language)); }
    finally { setBusy(false); }
  };
  return <section className="auth-wrap"><div className="auth-card setup-card">
    <p className="eyebrow">{t('setupEyebrow')}</p><h1>{t('setupTitle')}</h1><p className="muted">{t('setupIntro')}</p>
    <ol className="setup-steps" aria-label={t('setupProgress')}>{(['admin', 'network', 'review'] as const).map((key, index) => <li key={key} aria-current={step === index ? 'step' : undefined}><span>{`0${index + 1}`}</span> {t(key)}</li>)}</ol>
    <form onSubmit={(event) => void submit(event)} className="auth-form">
      {step === 0 && <>
        <p className="muted small">{t('tokenIntro')}</p>
        <label htmlFor="setup-token">{t('token')}</label><input id="setup-token" type="password" value={token} onChange={(event) => setToken(event.target.value)} required autoComplete="off" />
        <label htmlFor="setup-username">{t('adminUsername')}</label><input id="setup-username" value={username} onChange={(event) => setUsername(event.target.value)} required maxLength={64} autoComplete="username" />
        <label htmlFor="setup-password">{t('adminPassword')}</label><input id="setup-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} required minLength={12} autoComplete="new-password" />
        <label htmlFor="setup-confirm">{t('confirmPassword')}</label><input id="setup-confirm" type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required minLength={12} autoComplete="new-password" />
      </>}
      {step === 1 && <>
        <p className="muted small">{t('networkIntro')}</p>
        <label htmlFor="setup-listen">{t('listen')}</label><input id="setup-listen" value={listen} onChange={(event) => setListen(event.target.value)} required spellCheck={false} />
        <small>{t('listenHelp')}</small>
        <label htmlFor="setup-servers">{t('upstream')}</label><textarea id="setup-servers" rows={5} value={servers} onChange={(event) => setServers(event.target.value)} required spellCheck={false} />
        <label htmlFor="setup-mode">{translate('settings.upstreams.mode.label', language)}</label><select id="setup-mode" value={mode} onChange={(event) => setMode(event.target.value)}><option value="weighted">{translate('settings.upstreams.mode.weighted', language)}</option><option value="parallel">{translate('settings.upstreams.mode.parallel', language)}</option></select>
        <label className="toggle-line"><input type="checkbox" checked={preferH3} onChange={(event) => setPreferH3(event.target.checked)} />{translate('settings.upstreams.prefer_h3.label', language)}</label>
        <label htmlFor="setup-bootstrap">{translate('settings.upstreams.bootstrap.label', language)}</label><textarea id="setup-bootstrap" rows={3} value={bootstrap} onChange={(event) => setBootstrap(event.target.value)} spellCheck={false} />
      </>}
      {step === 2 && <><p className="muted small">{t('reviewIntro')}</p><label htmlFor="setup-toml">{t('setupConfig')}</label><textarea id="setup-toml" className="toml-editor" value={toml} onChange={(event) => setToml(event.target.value)} spellCheck={false} /></>}
      {(error !== null || templateError !== null) && <p className="error" role="alert">{error ?? present(templateError, language)}</p>}
      <div className="button-group">{step > 0 && <button className="button secondary" type="button" onClick={() => setStep((current) => current - 1)}>{t('back')}</button>}
        <button className="button primary" type="submit" disabled={busy || template === null}>{busy ? translate('app.working', language) : step === 2 ? t('start') : t('continue')}</button></div>
    </form>
  </div></section>;
}
