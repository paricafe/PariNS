import { useEffect, useState } from 'react';
import { Navigate, NavLink, Route, Routes, useLocation, useNavigate } from 'react-router-dom';
import { Activity, BookOpenText, Database, FileCode2, Filter, LockKeyhole, Menu, Network, Settings2, X } from 'lucide-react';
import { LoginPage, SetupPage } from './AuthPages';
import { translate, chooseLanguage, presentIssue, type Language } from './i18n';
import { ConfigProvider, toConfigIssue, useConfig } from './config/context';
import { SettingsPage } from './pages/config/SettingsPage';
import { OverviewPage, LogsPage } from './pages/observability';
import { SessionProvider, useSession } from './session/context';
import { Button, Drawer } from './components/beui';
import { ConfirmProvider, useConfirm } from './components/ConfirmProvider';
import { TransportHint, transportChangeText } from './components/TransportHint';

type Theme = 'system' | 'light' | 'dark';
const navigation = [
  { path: '/overview', key: 'overview', icon: Activity, group: 'observe' },
  { path: '/logs', key: 'logs', icon: BookOpenText, group: 'observe' },
  { path: '/cache', key: 'cache', icon: Database, group: 'observe' },
  { path: '/dns', key: 'dns', icon: Network, group: 'config' },
  { path: '/filters', key: 'filters', icon: Filter, group: 'config' },
  { path: '/security', key: 'security', icon: LockKeyhole, group: 'config' },
  { path: '/runtime', key: 'runtime', icon: Settings2, group: 'config' },
  { path: '/advanced', key: 'advanced', icon: FileCode2, group: 'config' },
] as const;

function storedLanguage(): Language {
  try { return chooseLanguage(localStorage.getItem('parins-language'), navigator.languages); }
  catch { return chooseLanguage(null, navigator.languages); }
}
function storedTheme(): Theme {
  try { const stored = localStorage.getItem('parins-theme'); return stored === 'light' || stored === 'dark' ? stored : 'system'; }
  catch { return 'system'; }
}

function useAppearance() {
  const [language, setLanguage] = useState<Language>(storedLanguage);
  const [theme, setTheme] = useState<Theme>(storedTheme);
  useEffect(() => {
    const media = matchMedia('(prefers-color-scheme: dark)');
    const update = () => {
      const dark = theme === 'dark' || theme === 'system' && media.matches;
      document.documentElement.classList.toggle('dark', dark);
    };
    update(); media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, [theme]);
  useEffect(() => { document.documentElement.lang = language; }, [language]);
  const changeLanguage = (next: Language) => { setLanguage(next); try { localStorage.setItem('parins-language', next); } catch { /* presentation preference only */ } };
  const changeTheme = (next: Theme) => { setTheme(next); try { if (next === 'system') localStorage.removeItem('parins-theme'); else localStorage.setItem('parins-theme', next); } catch { /* presentation preference only */ } };
  return { language, theme, changeLanguage, changeTheme };
}

function Preferences({ language, theme, changeLanguage, changeTheme }: ReturnType<typeof useAppearance>) {
  const t = (key: string) => translate(`ui.${key}`, language);
  return <div className="preferences">
    <label className="select-label"><span className="sr-only">{t('language')}</span><select aria-label={t('language')} value={language} onChange={(event) => changeLanguage(event.target.value as Language)}><option value="zh-CN">简体中文</option><option value="en">English</option></select></label>
    <label className="select-label"><span className="sr-only">{t('theme')}</span><select aria-label={t('theme')} value={theme} onChange={(event) => changeTheme(event.target.value as Theme)}><option value="system">{t('themeAuto')}</option><option value="light">{t('themeLight')}</option><option value="dark">{t('themeDark')}</option></select></label>
  </div>;
}

function SaveBar({ language }: { language: Language }) {
  const { draft, dirty, busy, prepareSave, commitPrepared, reload, resolveUnknown, setError, discard } = useConfig();
  const { transportChanged } = useSession();
  const confirmAction = useConfirm();
  const [notice, setNotice] = useState<string | null>(null);
  useEffect(() => { if (dirty) setNotice(null); }, [dirty]);
  if (!draft || (!dirty && !draft.unknownApply && !notice)) return null;
  const t = (key: string) => translate(key, language);
  const runSave = async (trigger: HTMLElement) => {
    try {
      const prepared = await prepareSave();
      const impact = prepared.restart_required ? t('app.saveRestartHelp') : t('app.saveCacheHelp');
      const change = prepared.transportChange;
      const message = change ? `${impact} ${transportChangeText(change, language)} ${change.requires_http_confirmation ? t('app.confirmHttpDowngrade') : ''}` : impact;
      if (!await confirmAction(message, 'app.save', trigger)) return;
      const result = await commitPrepared(prepared);
      if (result.transportChange) { discard(); transportChanged(result.transportChange); return; }
      setNotice(result.refreshed ? t('app.configSaved') : null);
    } catch (reason) { setError(toConfigIssue(reason)); }
  };
  return <div className="save-bar" role="region" aria-label={t('ui.draft')}>
    <div className="save-bar-content"><div><strong>{draft.unknownApply ? t(draft.pendingRollback ? 'app.rollbackNeedsCheck' : 'app.saveUnknown') : dirty ? t('app.dirty') : notice}</strong>
      {draft.unknownApply && draft.pendingTransportChange && <TransportHint change={draft.pendingTransportChange} language={language} link />}
      {notice && (dirty || draft.unknownApply) && <span className="small">{notice}</span>}</div>
      <div className="button-group">{!dirty && !draft.unknownApply ? <button type="button" className="button quiet" onClick={() => setNotice(null)}>{t('ui.closeDialog')}</button> : draft.unknownApply ? <><button type="button" className="button secondary" disabled={busy} onClick={() => void confirmAction('app.discardHelp', 'app.discard').then((accepted) => { if (accepted) void reload().catch(() => {}); })}>{t('app.discard')}</button><button type="button" className="button primary" disabled={busy} onClick={() => void resolveUnknown().then((result) => setNotice(result === 'applied' ? t(draft.pendingRollback ? 'app.rollbackComplete' : 'app.configSaved') : result === 'pending' ? t('app.saveStillUnknown') : t('api.REVISION'))).catch((reason) => setError(toConfigIssue(reason)))}>{t(draft.pendingRollback ? 'app.checkRollback' : 'app.checkSave')}</button></> : <>
        <button type="button" className="button quiet" disabled={busy} onClick={() => void confirmAction('app.discardHelp', 'app.discard').then((accepted) => { if (accepted) void reload().catch(() => {}); })}>{t('app.discard')}</button>
        <Button variant="primary" disabled={busy} onClick={(event) => void runSave(event.currentTarget)}>{t('app.save')}</Button></>}</div>
    </div>
  </div>;
}

function ReadyConsole({ appearance }: { appearance: ReturnType<typeof useAppearance> }) {
  const { language } = appearance;
  const { api, logout } = useSession();
  const config = useConfig();
  const confirmAction = useConfirm();
  const navigate = useNavigate();
  const location = useLocation();
  const [mobileOpen, setMobileOpen] = useState(false);
  const [logoutError, setLogoutError] = useState<string | null>(null);
  useEffect(() => { setMobileOpen(false); }, [location.pathname]);
  useEffect(() => {
    if (!config.dirty) return;
    const warning = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', warning);
    return () => window.removeEventListener('beforeunload', warning);
  }, [config.dirty]);
  const t = (key: string) => translate(`ui.${key}`, language);
  const signOut = async () => {
    if (config.busy || config.locked) return;
    if (config.dirty && !await confirmAction('app.logoutHelp', 'ui.logout')) return;
    config.discard();
    try { await logout(); } catch (reason) { setLogoutError(reason instanceof Error ? reason.message : String(reason)); }
  };
  const current = navigation.find((item) => item.path === location.pathname);
  const nav = <nav className="nav-groups" aria-label={t('pagesLabel')}>
    {(['observe', 'config'] as const).map((group) => <div className="nav-group" key={group}>
      <span className="nav-caption">{group === 'observe' ? language === 'en' ? 'Observe' : '观察' : t('configuration')}</span>
      {navigation.filter((item) => item.group === group).map((item) => <NavLink key={item.path} to={item.path} className={({ isActive }) => `nav-item${isActive ? ' active' : ''}`} onClick={(event) => {
        const draft = config.draft;
        if (item.path === '/advanced' && draft && (Object.keys(draft.fields).length || Object.keys(draft.optional).length || draft.rules !== null)) {
          event.preventDefault();
          void config.preview().then(() => { setMobileOpen(false); navigate('/advanced'); }).catch(() => {});
          return;
        }
        if (item.path !== '/advanced' && location.pathname === '/advanced' && draft?.stale) {
          event.preventDefault();
          void config.ensureParsed().then(() => { setMobileOpen(false); navigate(item.path); }).catch(() => {});
          return;
        }
        setMobileOpen(false);
      }}><item.icon size={18} strokeWidth={1.8} aria-hidden="true" /><span>{t(item.key)}</span></NavLink>)}
    </div>)}
  </nav>;
  return <div className="app-shell">
    <aside className="sidebar" aria-label={t('navLabel')}>
      <div className="sidebar-brand"><span className="brand-mark">P</span><span><strong>PariNS</strong><small>{t('brand')}</small></span><button type="button" className="icon-button mobile-close" onClick={() => setMobileOpen(false)} aria-label={t('closeNavigation')}><X size={20} /></button></div>
      {nav}
    </aside>
    <Drawer open={mobileOpen} onClose={() => setMobileOpen(false)} title="PariNS" side="left" closeLabel={t('closeNavigation')}>{nav}</Drawer>
    <div className="shell-main"><header className="topbar">
      <button type="button" className="icon-button mobile-menu" onClick={() => setMobileOpen(true)} aria-label={t('showNavigation')}><Menu size={20} /></button>
      <span className="topbar-title">{current ? t(current.key) : t('overview')}</span><span className="spacer" />
      <Preferences {...appearance} /><button type="button" className="button quiet logout-button" disabled={config.busy || config.locked} onClick={() => void signOut()}>{t('logout')}</button>
    </header><main id="main-content" tabIndex={-1} className="main-content">
      <a className="skip-target sr-only" id="main-content-start" href="#main-content">{t('skip')}</a>
      {logoutError && <p role="alert" className="notice error">{logoutError}</p>}
      {config.error && (location.pathname === '/overview' || location.pathname === '/logs') && <p role="alert" className="notice error">{presentIssue(config.error, language)}</p>}
      <Routes>
        <Route path="/overview" element={<OverviewPage api={api} language={language} onOpenDns={() => navigate('/dns')} />} />
        <Route path="/logs" element={<LogsPage api={api} language={language} onOpenSettings={() => navigate('/runtime')} />} />
        {(['dns', 'cache', 'filters', 'security', 'runtime', 'advanced'] as const).map((pageId) => <Route key={pageId} path={`/${pageId}`} element={<SettingsPage pageId={pageId} language={language} />} />)}
        <Route path="*" element={<Navigate to="/overview" replace />} />
      </Routes>
    </main></div><SaveBar language={language} />
  </div>;
}

function RootView() {
  const appearance = useAppearance();
  const { state, recheck, retryLogout, api } = useSession();
  const t = (key: string) => translate(key, appearance.language);
  return <>
    <a href="#main-content" className="skip-link" onClick={(event) => { event.preventDefault(); document.getElementById('main-content')?.focus(); }}>{t('ui.skip')}</a>
    {state.phase !== 'ready' && <header className="auth-topbar"><a href="/#/overview" className="brand-link">PariNS</a><Preferences {...appearance} /></header>}
    <ConfigProvider api={api} active={state.phase === 'ready'} refreshSession={recheck}>{state.phase === 'ready' ? <ConfirmProvider language={appearance.language}><ReadyConsole appearance={appearance} /></ConfirmProvider>
      : state.phase === 'setup' ? <main id="main-content" tabIndex={-1}><SetupPage language={appearance.language} /></main>
        : state.phase === 'login' ? <main id="main-content" tabIndex={-1}><LoginPage language={appearance.language} /></main>
          : <main id="main-content" tabIndex={-1} className="state-page"><div className="state-card"><h1>{t('ui.title')}</h1>
            <p>{state.phase === 'checking' ? t('ui.connecting') : state.phase === 'cookie-unavailable' ? t('app.cookieUnavailable') : state.phase === 'transport-change' ? t('app.transportChanged') : state.phase === 'setup-unknown' ? t('app.setupUnknown') : state.phase === 'logout-unknown' ? t('app.logoutUnknown') : t('app.startFailed')}</p>
            {state.phase === 'setup-unknown' && state.nextOrigin && <a href={`${state.nextOrigin}${window.location.pathname}${window.location.hash}`}>{t('app.openNewAddress')} · {state.nextOrigin}</a>}
            {state.phase === 'transport-change' && (state.nextOrigin ? <a href={`${state.nextOrigin}${window.location.pathname}${window.location.hash}`}>{t('app.openNewAddress')} · {state.nextOrigin}</a> : <p>{t('app.transportAddressUnknown')}</p>)}
            {state.error && <p className="muted small">{state.error}</p>}
            {state.phase === 'logout-unknown' ? <button className="button primary" type="button" onClick={() => void retryLogout().catch(() => {})}>{t('app.logout')}</button>
              : state.phase !== 'checking' && state.phase !== 'transport-change' && <button className="button secondary" type="button" onClick={() => void recheck()}>{t('ui.reconnect')}</button>}
          </div></main>}</ConfigProvider>
  </>;
}

export default function App() { return <SessionProvider><RootView /></SessionProvider>; }
