import { useEffect, useMemo, useRef, useState } from 'react';
import { motion, useReducedMotion, useScroll, useTransform } from 'motion/react';
import { DURATION, SPRING_IOS, SPRING_IOS_FAST } from './components/beui/motion';
import { Navigate, NavLink, Route, Routes, useLocation, useNavigate } from 'react-router-dom';
import { Activity, BookOpenText, CircleArrowUp, Database, FileCode2, Filter, LockKeyhole, Menu, Network, Settings2, X } from 'lucide-react';
import { LoginPage, SetupPage } from './AuthPages';
import { translate, presentIssue, type Language } from './i18n';
import { useAppearance, type Appearance } from './appearance/useAppearance';
import { ConfigProvider, toConfigIssue, useConfig, type ConfigIssue } from './config/context';
import { SettingsPage } from './pages/config/SettingsPage';
import { OverviewPage, LogsPage } from './pages/observability';
import { SessionProvider, useSession } from './session/context';
import { Button, Drawer } from './components/beui';
import { ConfirmProvider, useConfirm } from './components/ConfirmProvider';
import { TransportHint, transportChangeText } from './components/TransportHint';
import { BannerProvider, useBanner } from './components/Banner';
import { Select } from './components/Popover';
import { ThemeButton } from './components/ThemeButton';
import { AppearanceSheet } from './components/AppearanceSheet';
import { Backdrop } from './components/Backdrop';
import { lineDiff } from './pages/config/lineDiff';
import { UpdatesProvider } from './features/updates/context';
import { UpdateHint } from './features/updates/UpdatePanel';
import { AccountControls } from './features/account/AccountControls';
import { getPath } from './model/config';

const navigation = [
  { path: '/overview', key: 'overview', icon: Activity, group: 'observe' },
  { path: '/logs', key: 'logs', icon: BookOpenText, group: 'observe' },
  { path: '/cache', key: 'cache', icon: Database, group: 'observe' },
  { path: '/dns', key: 'dns', icon: Network, group: 'config' },
  { path: '/filters', key: 'filters', icon: Filter, group: 'config' },
  { path: '/security', key: 'security', icon: LockKeyhole, group: 'config' },
  { path: '/storage', key: 'storage', icon: Database, group: 'config' },
  { path: '/runtime', key: 'runtime', icon: Settings2, group: 'config' },
  { path: '/updates', key: 'updates', icon: CircleArrowUp, group: 'config' },
  { path: '/advanced', key: 'advanced', icon: FileCode2, group: 'config' },
] as const;

function upstreamServers(settings: unknown): number | null {
  const servers = getPath(settings, 'upstreams.servers');
  return Array.isArray(servers) ? servers.length : null;
}

function Preferences({ appearance }: { appearance: Appearance }) {
  const { language } = appearance;
  return <div className="preferences">
    <Select label={translate('ui.language', language)} value={language} onChange={(next) => appearance.changeLanguage(next as Language)} options={[{ value: 'zh-CN', label: '简体中文' }, { value: 'en', label: 'English' }]} />
    <ThemeButton theme={appearance.theme} language={language} onChange={appearance.changeTheme} />
    <AppearanceSheet appearance={appearance} />
  </div>;
}

function DiffSheet({ open, onClose, language }: { open: boolean; onClose(): void; language: Language }) {
  const { draft } = useConfig();
  const t = (key: string) => translate(key, language);
  const diff = useMemo(() => open && draft ? lineDiff(draft.original, draft.toml) : [], [open, draft?.original, draft?.toml]);
  return <Drawer open={open} variant="sheet" className="diff-sheet" title={t('app.diffTitle')} closeLabel={t('ui.closeDialog')} onClose={onClose}>
    <p className="muted small">{t('app.diffHelp')}</p>
    {draft?.previewTransportChange && <TransportHint change={draft.previewTransportChange} language={language} />}
    {diff.some((line) => line.kind !== 'same') ? <pre className="diff-lines"><code>{diff.map((line, index) => <span className={`diff-${line.kind}`} key={index}>{line.kind === 'add' ? '+' : line.kind === 'remove' ? '−' : ' '}{line.text || ' '}</span>)}</code></pre>
      : <p className="muted">{t('app.noChanges')}</p>}
  </Drawer>;
}

function SaveBar({ language }: { language: Language }) {
  const reduced = useReducedMotion();
  const { draft, dirty, changes, busy, preview, prepareSave, commitPrepared, reload, resolveUnknown, setError, discard } = useConfig();
  const { transportChanged } = useSession();
  const confirmAction = useConfirm();
  const banner = useBanner();
  const [previewOpen, setPreviewOpen] = useState(false);
  // The trigger is disabled while the preview request runs, so the sheet cannot capture it; return focus here.
  const previewTrigger = useRef<HTMLButtonElement>(null);
  if (!draft || (!dirty && !draft.unknownApply)) return null;
  const t = (key: string) => translate(key, language);
  // The bar unmounts once the draft is settled; keep keyboard focus inside the page rather than on <body>.
  const settleFocus = () => requestAnimationFrame(() => { if (!document.activeElement || document.activeElement === document.body) document.getElementById('main-content')?.focus({ preventScroll: true }); });
  const runSave = async (trigger: HTMLElement) => {
    try {
      const prepared = await prepareSave();
      const impact = prepared.restart_required ? t('app.saveRestartHelp') : t('app.saveCacheHelp');
      const change = prepared.transportChange;
      const message = change ? `${impact} ${transportChangeText(change, language)} ${change.requires_http_confirmation ? t('app.confirmHttpDowngrade') : ''}` : impact;
      if (!await confirmAction(message, 'app.save', trigger)) return;
      const result = await commitPrepared(prepared);
      if (result.transportChange) { discard(); transportChanged(result.transportChange); return; }
      if (result.refreshed) banner(t('app.configSaved'));
      settleFocus();
    } catch (reason) { setError(toConfigIssue(reason)); }
  };
  const discardChanges = () => void confirmAction('app.discardHelp', 'app.discard').then((accepted) => { if (accepted) void reload().then(settleFocus).catch(() => {}); });
  return <motion.div className="save-bar" role="region" aria-label={t('ui.draft')} initial={reduced ? false : { opacity: 0, y: 40, scale: .96 }} animate={{ opacity: 1, y: 0, scale: 1 }} transition={reduced ? { duration: 0 } : SPRING_IOS}>
    <div className="save-bar-content"><div><strong>{draft.unknownApply ? t(draft.pendingRollback ? 'app.rollbackNeedsCheck' : 'app.saveUnknown') : changes === 1 ? t('shell.unsavedOne') : translate('shell.unsavedMany', language, { count: changes })}</strong>
      {draft.unknownApply && draft.pendingTransportChange && <TransportHint change={draft.pendingTransportChange} language={language} link />}</div>
      <div className="button-group">{draft.unknownApply ? <><button type="button" className="button secondary" disabled={busy} onClick={discardChanges}>{t('app.discard')}</button><button type="button" className="button primary" disabled={busy} onClick={() => void resolveUnknown().then((result) => {
          if (result === 'applied') { banner(t(draft.pendingRollback ? 'app.rollbackComplete' : 'app.configSaved')); settleFocus(); }
          else setError(result === 'pending' ? 'app.saveStillUnknown' : 'api.REVISION');
        }).catch((reason) => setError(toConfigIssue(reason)))}>{t(draft.pendingRollback ? 'app.checkRollback' : 'app.checkSave')}</button></> : <>
        <button type="button" className="button quiet" disabled={busy} onClick={discardChanges}>{t('app.discard')}</button>
        <button ref={previewTrigger} type="button" className="button secondary" disabled={busy} onClick={() => void preview().then(() => setPreviewOpen(true)).catch(() => {})}>{t('ui.preview')}</button>
        <Button variant="primary" disabled={busy} onClick={(event) => void runSave(event.currentTarget)}>{t('app.save')}</Button></>}</div>
    </div>
    <DiffSheet open={previewOpen} onClose={() => { setPreviewOpen(false); requestAnimationFrame(() => previewTrigger.current?.focus()); }} language={language} />
  </motion.div>;
}

function ReadyConsole({ appearance }: { appearance: Appearance }) {
  const { language } = appearance;
  const { api, logout } = useSession();
  const config = useConfig();
  const confirmAction = useConfirm();
  const navigate = useNavigate();
  const location = useLocation();
  const reduced = useReducedMotion();
  // Page transitions use the Web Animations API: time-based, no inline styles left behind, and already
  // finished if the tab was hidden. Phones push the new page in from the right (iOS); wider screens fade it up.
  const routeContent = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const element = routeContent.current;
    if (!element?.animate) return;
    const phone = window.matchMedia?.('(max-width: 767px)').matches ?? false;
    const frames = reduced ? [{ opacity: 0 }, { opacity: 1 }]
      : phone ? [{ opacity: 0.4, transform: 'translateX(30%)' }, { opacity: 1, transform: 'none' }]
        : [{ opacity: 0, transform: 'translateY(10px)' }, { opacity: 1, transform: 'none' }];
    const animation = element.animate(frames, { duration: reduced ? 150 : phone ? 420 : DURATION.enter * 1000, easing: 'cubic-bezier(.2, .9, .25, 1)' });
    return () => animation.cancel();
  }, [location.pathname, reduced]);
  // iOS large title: the bar title and material fade in as the page title scrolls away.
  const { scrollY } = useScroll();
  const titleOpacity = useTransform(scrollY, [28, 64], [0, 1]);
  const barMaterial = useTransform(scrollY, [0, 48], [0, 1]);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [logoutError, setLogoutError] = useState<ConfigIssue | null>(null);
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
    try { await logout(); } catch (reason) { setLogoutError(toConfigIssue(reason)); }
  };
  const current = navigation.find((item) => item.path === location.pathname);
  const nav = (scope: string) => <nav className="nav-groups" aria-label={t('pagesLabel')}>
    {(['observe', 'config'] as const).map((group) => <div className="nav-group" key={group}>
      <span className="nav-caption">{t(group === 'observe' ? 'observe' : 'configuration')}</span>
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
      }}>{({ isActive }) => <><item.icon size={18} strokeWidth={1.8} aria-hidden="true" /><span>{t(item.key)}</span>{isActive && <motion.i className="nav-indicator" aria-hidden="true" layoutId={`navigation-${scope}`} transition={reduced ? { duration: 0 } : SPRING_IOS_FAST} />}</>}</NavLink>)}
    </div>)}
  </nav>;
  return <div className="app-shell">
    <aside className="sidebar" aria-label={t('navLabel')}>
      <div className="sidebar-brand"><span className="brand-mark" aria-hidden="true">P</span><span><strong>PariNS</strong><small>{t('brand')}</small></span><button type="button" className="icon-button mobile-close" onClick={() => setMobileOpen(false)} aria-label={t('closeNavigation')}><X size={20} /></button></div>
      {nav('desktop')}
    </aside>
    <Drawer open={mobileOpen} onClose={() => setMobileOpen(false)} title="PariNS" side="left" closeLabel={t('closeNavigation')}>{nav('mobile')}</Drawer>
    <div className="shell-main"><header className="topbar"><motion.span className="topbar-material" aria-hidden="true" style={{ opacity: barMaterial }} />
      <button type="button" className="icon-button mobile-menu" onClick={() => setMobileOpen(true)} aria-label={t('showNavigation')}><Menu size={20} /></button>
      <motion.span className="topbar-title" style={{ opacity: titleOpacity }}>{current ? t(current.key) : t('overview')}</motion.span><span className="spacer" />
      <Preferences appearance={appearance} /><AccountControls language={language} /><button type="button" className="button quiet logout-button" disabled={config.busy || config.locked} onClick={() => void signOut()}>{t('logout')}</button>
    </header><main id="main-content" tabIndex={-1} className="main-content">
      <a className="skip-target sr-only" id="main-content-start" href="#main-content">{t('skip')}</a>
      {logoutError && <p role="alert" className="notice error">{presentIssue(logoutError, language)}</p>}
      {config.error && (location.pathname === '/overview' || location.pathname === '/logs') && <p role="alert" className="notice error">{presentIssue(config.error, language)}</p>}
      <div ref={routeContent} className="route-content"><Routes>
        <Route path="/overview" element={<><UpdateHint language={language} /><OverviewPage api={api} language={language} onOpenDns={() => navigate('/dns')} settingsState={config.draft?.unknownApply ? 'unknown' : config.dirty ? 'unsaved' : 'saved'} upstreamCount={upstreamServers(config.draft?.settings)} /></>} />
        <Route path="/logs" element={<LogsPage api={api} language={language} onOpenSettings={() => navigate('/storage')} />} />
        {(['dns', 'cache', 'filters', 'security', 'storage', 'runtime', 'updates', 'advanced'] as const).map((pageId) => <Route key={pageId} path={`/${pageId}`} element={<SettingsPage pageId={pageId} language={language} />} />)}
        <Route path="*" element={<Navigate to="/overview" replace />} />
      </Routes></div>
    </main></div><SaveBar language={language} />
  </div>;
}

function RootView() {
  const appearance = useAppearance();
  const { state, recheck, retryLogout, api } = useSession();
  const t = (key: string) => translate(key, appearance.language);
  return <>
    <Backdrop background={appearance.background} image={appearance.image} overlay={appearance.overlay} dark={appearance.dark} />
    <a href="#main-content" className="skip-link" onClick={(event) => { event.preventDefault(); document.getElementById('main-content')?.focus(); }}>{t('ui.skip')}</a>
    {state.phase !== 'ready' && <header className="auth-topbar"><a href="/#/overview" className="brand-link"><span className="brand-mark" aria-hidden="true">P</span>PariNS</a><Preferences appearance={appearance} /></header>}
    <ConfigProvider api={api} active={state.phase === 'ready'} refreshSession={recheck}><UpdatesProvider active={state.phase === 'ready'}>{state.phase === 'ready' ? <ConfirmProvider language={appearance.language}><ReadyConsole appearance={appearance} /></ConfirmProvider>
      : state.phase === 'setup' ? <main id="main-content" tabIndex={-1}><SetupPage language={appearance.language} /></main>
        : state.phase === 'login' ? <main id="main-content" tabIndex={-1}><LoginPage language={appearance.language} /></main>
          : <main id="main-content" tabIndex={-1} className="state-page"><div className="state-card"><h1>{t('ui.title')}</h1>
            <p>{state.phase === 'checking' ? t('ui.connecting') : state.phase === 'cookie-unavailable' ? t('app.cookieUnavailable') : state.phase === 'transport-change' ? t('app.transportChanged') : state.phase === 'setup-unknown' ? t('app.setupUnknown') : state.phase === 'logout-unknown' ? t('app.logoutUnknown') : t('app.startFailed')}</p>
            {state.phase === 'setup-unknown' && state.nextOrigin && <a href={`${state.nextOrigin}${window.location.pathname}${window.location.hash}`}>{t('app.openNewAddress')} · {state.nextOrigin}</a>}
            {state.phase === 'transport-change' && (state.nextOrigin ? <a href={`${state.nextOrigin}${window.location.pathname}${window.location.hash}`}>{t('app.openNewAddress')} · {state.nextOrigin}</a> : <p>{t('app.transportAddressUnknown')}</p>)}
            {state.error && <p className="muted small">{state.error}</p>}
            {state.phase === 'logout-unknown' ? <button className="button primary" type="button" onClick={() => void retryLogout().catch(() => {})}>{t('app.logout')}</button>
              : state.phase !== 'checking' && state.phase !== 'transport-change' && <button className="button secondary" type="button" onClick={() => void recheck()}>{t('ui.reconnect')}</button>}
          </div></main>}</UpdatesProvider></ConfigProvider>
  </>;
}

export default function App() { return <SessionProvider><BannerProvider><RootView /></BannerProvider></SessionProvider>; }
