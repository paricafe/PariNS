import { useEffect, useState, type FormEvent } from 'react';
import { networkTemplate } from './model';
import { hasTranslation, translate, type Language } from './i18n';
import { ApiError, CookieUnavailable } from './session/client';
import { useSession } from './session/context';
import { TurnstileWidget } from './features/account/Turnstile';
import { passwordProblem, validUsername } from './features/account/password';
import { reloadConsole } from './features/updates/reload';
import { Segmented } from './components/Segmented';
import { Switch } from './components/beui';
import { useConfirm } from './components/ConfirmProvider';

function present(error: unknown, language: Language): string {
  if (error instanceof CookieUnavailable) return translate('app.cookieUnavailable', language);
  if (error instanceof ApiError && hasTranslation(`api.${error.code}`)) return translate(`api.${error.code}`, language, { detail: error.message, status: error.status });
  return error instanceof Error ? error.message : translate('app.offline', language);
}

interface LoginOptions { turnstile: { site_key: string; action: string } | null; turnstile_skipped: boolean }

export function LoginPage({ language }: { language: Language }) {
  const { api, login, state, turnstileAllowed } = useSession();
  const [options, setOptions] = useState<LoginOptions | null>(null);
  // Sign-in waits for the options: they say whether it needs Turnstile.
  const [optionsFailed, setOptionsFailed] = useState(false);
  const [optionsAttempt, setOptionsAttempt] = useState(0);
  const [username, setUsername] = useState(''); const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState<string | null>(null); const [resetSignal, setResetSignal] = useState(0);
  const t = (key: string) => translate(`ui.${key}`, language);
  useEffect(() => {
    let live = true;
    setOptionsFailed(false);
    api.request<LoginOptions>('login/options', 'GET', undefined, undefined, { unauthenticated: true })
      .then((value) => { if (live) setOptions(value); }, () => { if (live) setOptionsFailed(true); });
    return () => { live = false; };
  }, [api, optionsAttempt]);
  const turnstile = options?.turnstile ?? null;
  // Only a document served signed out while Turnstile was on may load the widget (its script
  // policy is relaxed for it); any other document reloads once to get that policy.
  const widgetAllowed = options !== null && turnstileAllowed(Boolean(turnstile));
  useEffect(() => { if (turnstile && !widgetAllowed) reloadConsole(); }, [turnstile, widgetAllowed]);
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setBusy(true); setError(null);
    try {
      if (turnstile && !token) throw new ApiError(403, 'TURNSTILE_REQUIRED', 'Complete the human verification');
      await login(username.trim(), password, { turnstileToken: token, reload: Boolean(turnstile) });
      setPassword('');
    } catch (reason) { setError(present(reason, language)); }
    finally {
      setBusy(false);
      // Each Turnstile token is single-use.
      if (turnstile) setResetSignal((value) => value + 1);
    }
  };
  return <section className="auth-wrap"><div className="auth-card">
    <h1>{t('loginTitle')}</h1>
    {state.credentials && <p className="notice" role="status">{translate(`account.${state.credentials}`, language)}</p>}
    {optionsFailed && <div className="notice error" role="alert"><p>{translate('app.loginOptionsFailed', language)}</p>
      <button className="button secondary" type="button" onClick={() => setOptionsAttempt((value) => value + 1)}>{t('retry')}</button></div>}
    <form onSubmit={(event) => void submit(event)} className="auth-form">
      <label htmlFor="login-username">{t('username')}</label><input id="login-username" value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" required maxLength={64} />
      <label htmlFor="login-password">{t('password')}</label><input id="login-password" value={password} onChange={(event) => setPassword(event.target.value)} type="password" autoComplete="current-password" required />
      {turnstile && widgetAllowed && <TurnstileWidget siteKey={turnstile.site_key} action={turnstile.action} language={language} resetSignal={resetSignal} onToken={setToken} />}
      {options?.turnstile_skipped && <small className="muted">{translate('turnstile.loopbackLogin', language)}</small>}
      {error && <p className="error" role="alert">{error}</p>}
      <button className="button primary" type="submit" disabled={busy || options === null}>{busy ? translate('app.working', language) : t('login')}</button>
    </form>
  </div></section>;
}

type ListenMode = 'local' | 'network';
const LISTEN: Record<ListenMode, string> = { local: '127.0.0.1:5353', network: '0.0.0.0:53' };
type Preset = 'cloudflare' | 'google' | 'quad9' | 'alidns' | 'custom';
/** Hostname DoH endpoints need explicit bootstrap addresses; Rust validates both. */
const PRESETS: Record<Exclude<Preset, 'custom'>, { servers: string[]; bootstrap: string[] }> = {
  cloudflare: { servers: ['https://cloudflare-dns.com/dns-query'], bootstrap: ['1.1.1.1:53', '1.0.0.1:53'] },
  google: { servers: ['https://dns.google/dns-query'], bootstrap: ['8.8.8.8:53', '8.8.4.4:53'] },
  quad9: { servers: ['https://dns.quad9.net/dns-query'], bootstrap: ['9.9.9.9:53', '149.112.112.112:53'] },
  alidns: { servers: ['https://dns.alidns.com/dns-query'], bootstrap: ['223.5.5.5:53', '223.6.6.6:53'] },
};
const lines = (value: string) => value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);

/** An occupied listener address: setup binds every listener before saving and reports CONFIG_CONFLICT. */
function listenerInUse(error: unknown): error is ApiError {
  return error instanceof ApiError && error.code === 'CONFIG_CONFLICT';
}

export function SetupPage({ language }: { language: Language }) {
  const { api, setup } = useSession();
  const confirm = useConfirm();
  const [template, setTemplate] = useState<string | null>(null);
  const [step, setStep] = useState(0);
  const [token, setToken] = useState(''); const [username, setUsername] = useState('admin');
  const [password, setPassword] = useState(''); const [confirmPassword, setConfirmPassword] = useState('');
  const [listenMode, setListenMode] = useState<ListenMode>('local');
  const [preset, setPreset] = useState<Preset>(() => language === 'zh-CN' ? 'alidns' : 'cloudflare');
  const [servers, setServers] = useState('');
  const [bootstrap, setBootstrap] = useState(''); const [mode, setMode] = useState('weighted'); const [preferH3, setPreferH3] = useState(false);
  const [toml, setToml] = useState(''); const [generated, setGenerated] = useState(''); const [busy, setBusy] = useState(false); const [error, setError] = useState<string | null>(null);
  const [templateError, setTemplateError] = useState<unknown>(null);
  const t = (key: string) => translate(`ui.${key}`, language);
  useEffect(() => { void api.request<{ toml: string }>('template', 'GET', undefined, undefined, { unauthenticated: true }).then((result) => setTemplate(result.toml)).catch(setTemplateError); }, [api]);
  const listen = LISTEN[listenMode];
  const upstreams = preset === 'custom'
    ? { servers: lines(servers), bootstrap: lines(bootstrap), mode, prefer_h3: preferH3 }
    : { ...PRESETS[preset], mode: 'weighted', prefer_h3: false };
  const next = async (trigger?: HTMLElement) => {
    setError(null);
    if (step === 0) {
      // These match Rust's rules and name the first missing step.
      const problem = !token.trim() ? 'app.setupTokenRequired' : !username.trim() ? 'app.usernameRequired'
        : !validUsername(username.trim()) ? 'account.usernameInvalid'
        : passwordProblem(password) ?? (password !== confirmPassword ? 'account.mismatch' : null);
      if (problem) { setError(translate(problem, language)); return; }
    }
    if (step === 1) {
      if (!upstreams.servers.length) { setError(translate('settings.validation.required', language)); return; }
      if (!template) { setError(translate('app.startFailed', language)); return; }
      let candidate: string;
      try { candidate = networkTemplate(template, listen, upstreams); }
      catch (reason) { setError(present(reason, language)); return; }
      // A configuration edited by hand in the review step is kept unless the network settings changed.
      if (toml !== generated && candidate !== generated && !await confirm(translate('shell.setupRegenerate', language), 'shell.regenerate', trigger)) return;
      if (toml === generated || candidate !== generated) { setToml(candidate); setGenerated(candidate); }
    }
    setStep((current) => Math.min(current + 1, 2));
  };
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (step < 2) { void next((event.nativeEvent as SubmitEvent).submitter ?? undefined); return; }
    setBusy(true); setError(null);
    try { await setup(username.trim(), password, toml, token.trim()); setToken(''); setPassword(''); setConfirmPassword(''); }
    catch (reason) {
      if (listenMode === 'network' && toml === generated && listenerInUse(reason)) setError(translate('ui.port53InUse', language, { detail: reason.message }));
      else setError(present(reason, language));
    }
    finally { setBusy(false); }
  };
  return <section className="auth-wrap"><div className="auth-card setup-card">
    <h1>{t('setupTitle')}</h1>
    <ol className="setup-steps" aria-label={t('setupProgress')}>{(['admin', 'network', 'review'] as const).map((key, index) => <li key={key} aria-current={step === index ? 'step' : undefined} data-done={step > index ? '' : undefined}><span>{`0${index + 1}`}</span> {t(key)}</li>)}</ol>
    <form onSubmit={(event) => void submit(event)} className="auth-form">
      {step === 0 && <>
        <p className="muted small">{t('tokenIntro')}</p>
        <label htmlFor="setup-token">{t('token')}</label><input id="setup-token" type="password" value={token} onChange={(event) => setToken(event.target.value)} required autoComplete="off" />
        <label htmlFor="setup-username">{t('adminUsername')}</label><input id="setup-username" value={username} onChange={(event) => setUsername(event.target.value)} required maxLength={64} autoComplete="username" />
        <label htmlFor="setup-password">{t('adminPassword')}</label><input id="setup-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} required minLength={12} autoComplete="new-password" aria-describedby="setup-password-help" />
        <small id="setup-password-help" className="muted">{translate('account.passwordHelp', language)}</small>
        <label htmlFor="setup-confirm">{t('confirmPassword')}</label><input id="setup-confirm" type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required minLength={12} autoComplete="new-password" />
      </>}
      {step === 1 && <>
        <p className="muted small">{t('networkIntro')}</p>
        <span className="setup-label">{t('listenMode')}</span>
        <Segmented label={t('listenMode')} value={listenMode} onChange={(value) => setListenMode(value as ListenMode)}
          options={(['local', 'network'] as const).map((value) => ({ value, label: t(`listen_${value}`) }))} />
        <small>{translate(`ui.listen_${listenMode}_help`, language, { address: listen })}</small>
        <label htmlFor="setup-preset">{t('upstream')}</label>
        <select id="setup-preset" value={preset} onChange={(event) => setPreset(event.target.value as Preset)}>
          {(['cloudflare', 'google', 'quad9', 'alidns', 'custom'] as const).map((value) => <option key={value} value={value}>{t(`preset_${value}`)}</option>)}
        </select>
        {preset !== 'custom'
          ? <small>{translate('ui.presetHelp', language, { servers: PRESETS[preset].servers.join(', '), bootstrap: PRESETS[preset].bootstrap.join(', ') })}</small>
          : <>
            <label htmlFor="setup-servers">{translate('settings.upstreams.servers.label', language)}</label><textarea id="setup-servers" rows={5} placeholder={translate('settings.upstreams.servers.placeholder', language)} value={servers} onChange={(event) => setServers(event.target.value)} required spellCheck={false} />
            <span className="setup-label">{translate('settings.upstreams.mode.label', language)}</span>
            <Segmented label={translate('settings.upstreams.mode.label', language)} value={mode} onChange={setMode}
              options={(['weighted', 'parallel'] as const).map((value) => ({ value, label: translate(`settings.upstreams.mode.${value}`, language) }))} />
            <small>{translate('settings.upstreams.mode.help', language)}</small>
            <Switch checked={preferH3} onCheckedChange={setPreferH3} label={translate('settings.upstreams.prefer_h3.label', language)} />
            <label htmlFor="setup-bootstrap">{translate('settings.upstreams.bootstrap.label', language)}</label><textarea id="setup-bootstrap" rows={3} value={bootstrap} placeholder={translate('settings.upstreams.bootstrap.placeholder', language)} onChange={(event) => setBootstrap(event.target.value)} spellCheck={false} />
            <small>{translate('settings.upstreams.bootstrap.help', language)}</small>
          </>}
      </>}
      {step === 2 && <><p className="muted small">{t('reviewIntro')}</p>
        {toml !== generated ? <p className="notice">{translate('shell.setupEdited', language)}</p> : <dl className="setup-summary">
          <div className="detail-row"><span>{t('listen')}</span><strong className="transport-address">{listen}</strong></div>
          <div className="detail-row"><span>{translate('settings.group.upstreams.title', language)}</span><strong className="transport-address">{upstreams.servers.map((line, index) => <span key={index} style={{ display: 'block' }}>{line}</span>)}</strong></div>
          <div className="detail-row"><span>{translate('settings.upstreams.mode.label', language)}</span><strong>{translate(`settings.upstreams.mode.${upstreams.mode}`, language)}</strong></div>
          <div className="detail-row"><span>{translate('settings.upstreams.prefer_h3.label', language)}</span><strong>{translate(upstreams.prefer_h3 ? 'shell.setupYes' : 'shell.setupNo', language)}</strong></div>
          <div className="detail-row"><span>{translate('settings.upstreams.bootstrap.label', language)}</span><strong className="transport-address">{upstreams.bootstrap.length ? upstreams.bootstrap.join(', ') : translate('shell.setupNone', language)}</strong></div>
        </dl>}
        <details className="setup-advanced"><summary>{translate('shell.setupFullConfig', language)}</summary>
          <p className="muted small">{translate('shell.setupFullConfigHelp', language)}</p>
          <label htmlFor="setup-toml">{t('setupConfig')}</label><textarea id="setup-toml" className="toml-editor" value={toml} onChange={(event) => setToml(event.target.value)} spellCheck={false} />
        </details></>}
      {(error !== null || templateError !== null) && <p className="error" role="alert">{error ?? present(templateError, language)}</p>}
      <div className="button-group">{step > 0 && <button className="button secondary" type="button" onClick={() => setStep((current) => current - 1)}>{t('back')}</button>}
        <button className="button primary" type="submit" disabled={busy || template === null}>{busy ? translate('app.working', language) : step === 2 ? t('start') : t('continue')}</button></div>
    </form>
  </div></section>;
}
