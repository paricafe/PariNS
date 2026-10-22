import { useId, useRef } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { SPRING_IOS } from '../../components/beui/motion';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { configList } from '../../model';
import { subscriptionDraft, subscriptionId } from '../../model/subscriptions';
import { formatBytes, formatNumber, translate, type Language } from '../../i18n';
import { useSubscriptions } from './useSubscriptions';
import { SourceCard, failureText } from './SourceCard';
import { DomainCheck } from './DomainCheck';
import './subscriptions.css';

export function SubscriptionsPanel({ language }: { language: Language }) {
  const { api } = useSession();
  const { draft, setSources, locked, busy } = useConfig();
  const keyPrefix = useId();
  const nextKey = useRef(0);
  const view = useSubscriptions(api, draft?.original);
  const reduced = useReducedMotion();
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  if (!draft) return <p role="status">{translate('app.connecting', language)}</p>;
  const saved = draft.savedSources;
  const sources = draft.sources ?? configList(draft.settings, 'filter_subscriptions.sources').map((source) => subscriptionDraft(source));
  const observedOperation = view.operation ?? view.state?.operation ?? (view.unknown ? view.state?.recent_operation : null);
  const sourceName = (id: string) => sources.find((item) => item.id === id)?.name || saved.find((item) => item.id === id)?.name || id;
  const working = view.pending || view.state?.operation?.status === 'running' || locked || busy;
  const add = (example: boolean) => {
    const key = `${keyPrefix}-${++nextKey.current}`;
    const name = example ? 'Natsuki List' : '';
    setSources([...sources, { ...subscriptionDraft({ id: subscriptionId(name, sources.map((item) => item.id)), name,
      url: example ? 'https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/main/natsuki-list.list' : '',
      format: 'domain_list', enabled: false, auto_update: true, update_interval_hours: 24 }, key), autoId: true }]);
  };
  return <><section className="panel subscription-panel"><div className="heading-row"><h2>{t('title')}</h2>
    <div className="button-group"><button type="button" className="button quiet" onClick={() => void view.load()}>{t('refreshStatus')}</button>
      <button type="button" className="button secondary" disabled={working || !view.state?.sources.length} onClick={() => void view.refresh(null)}>{t('refreshAll')}</button></div></div>
    <p className="muted small">{t('help')}</p>
    {view.error && <p role="alert" className="notice error">{failureText(view.error, language)}</p>}
    {view.state?.unavailable_reason === 'update_in_progress'
      ? <p className="notice" role="status">{translate('subscriptions.frozenStatus', language)}</p>
      : view.state?.unavailable_reason && <p className="notice error" role="alert">{failureText({ code: view.state.unavailable_reason, line: null }, language)}</p>}
    {view.unknown && <p className="notice" role="status">{t('unknown')}</p>}
    {(observedOperation || view.pending) && <p className="notice" role="status">{t(view.pending ? 'running' : observedOperation!.status)}
      {observedOperation?.source_id && <> · {sourceName(observedOperation.source_id)}</>}
      {observedOperation?.rules != null && <> · {formatNumber(observedOperation.rules, language)} {t('input')}</>}
      {observedOperation?.error && <> · {failureText(observedOperation.error, language)}</>}
    </p>}
    {view.state && <p className="subscription-summary">{translate('subscriptions.summary', language, { rules: formatNumber(view.state.index_rules, language), memory: formatBytes(view.state.retained_bytes, language), disk: formatBytes(view.state.disk_bytes, language) })}</p>}
    {!sources.length && <p className="muted">{t('empty')}</p>}
    <div className="settings-stack"><AnimatePresence initial={false}>{sources.map((source, index) => {
      const baseline = saved?.find((item) => item.id === source.id && item.url === source.url && item.format === source.format);
      const status = baseline ? view.state?.sources.find((item) => item.id === source.id) : undefined;
      return <motion.div key={source.key} className="list-item" initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} animate={{ opacity: 1, height: 'auto', transitionEnd: { overflow: 'visible' } }}
        exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} transition={reduced ? { duration: 0.12 } : SPRING_IOS}><SourceCard source={source} index={index} status={status} language={language} locked={locked || busy} working={working}
        savedSource={baseline} subscriptionsEnabled={view.state?.enabled === true} takenIds={sources.filter((_, at) => at !== index).map((item) => item.id)}
        change={(next) => setSources(sources.map((item, at) => at === index ? next : item))} remove={() => setSources(sources.filter((_, at) => at !== index))}
        refresh={() => void view.refresh(source.id)} /></motion.div>;
    })}</AnimatePresence></div>
    <div className="button-group"><button type="button" className="button secondary" disabled={locked || busy || sources.length >= 16} onClick={() => add(false)}>{t('add')}</button>
      <button type="button" className="button quiet" disabled={locked || busy || sources.length >= 16} onClick={() => add(true)}>{t('example')}</button></div>
    <p className="muted small">{t('exampleHelp')} <a href="https://github.com/Natsuki-Kaede/Natsuki-List" target="_blank" rel="noreferrer">Natsuki List · GPLv3</a></p>
  </section><DomainCheck api={api} language={language} names={Object.fromEntries(saved.map((source) => [source.id, source.name]))} /></>;
}
