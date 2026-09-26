import { useId, useRef } from 'react';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { getPath } from '../../model';
import { subscriptionDraft, type SubscriptionSource } from '../../model/subscriptions';
import { formatNumber, translate, type Language } from '../../i18n';
import { useSubscriptions } from './useSubscriptions';
import { SourceCard, failureText } from './SourceCard';
import { DomainCheck } from './DomainCheck';
import './subscriptions.css';

export function SubscriptionsPanel({ language }: { language: Language }) {
  const { api } = useSession();
  const { draft, setSources, locked, busy } = useConfig();
  const keyPrefix = useId();
  const nextKey = useRef(0);
  const view = useSubscriptions(api, draft?.revision);
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  if (!draft) return <p role="status">{translate('app.connecting', language)}</p>;
  const saved = draft.savedSources;
  const revisionMatches = view.state?.config_revision === draft.revision;
  const sources = draft.sources ?? ((getPath(draft.settings, 'filter_subscriptions.sources') as SubscriptionSource[] | undefined) ?? []).map((source) => subscriptionDraft(source));
  const observedOperation = view.operation ?? view.state?.operation ?? (view.unknown ? view.state?.recent_operation : null);
  const working = view.pending || view.state?.operation?.status === 'running' || locked || busy;
  const add = (example: boolean) => {
    const key = `${keyPrefix}-${++nextKey.current}`;
    setSources([...sources, subscriptionDraft({ id: example ? 'natsuki' : '', name: example ? 'Natsuki List' : '',
      url: example ? 'https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/main/natsuki-list.list' : '',
      format: 'domain_list', enabled: false, auto_update: true, update_interval_hours: 24 }, key)]);
  };
  return <><section className="panel subscription-panel"><div className="heading-row"><h2>{t('title')}</h2>
    <div className="button-group"><button type="button" className="button quiet" onClick={() => void view.load()}>{t('refreshStatus')}</button>
      <button type="button" className="button secondary" disabled={working || !view.state?.sources.length} onClick={() => void view.start('refresh', null)}>{t('refreshAll')}</button></div></div>
    <p className="muted small">{t('help')}</p><p className="muted small">{t('savedOnly')}</p>
    {view.state && !revisionMatches && <p className="notice" role="status">{t('revision_conflict')}</p>}
    {view.error && <p role="alert" className="notice error">{failureText(view.error, language)}</p>}
    {view.state?.unavailable_reason && <p className="notice error" role="alert">{failureText({ code: view.state.unavailable_reason, line: null }, language)}</p>}
    {view.unknown && <p className="notice" role="status">{t('unknown')}</p>}
    {view.replaced && !view.pending && <p className="notice" role="status">{t('replaced')}</p>}
    {(observedOperation || view.pending) && <p className="notice" role="status">{view.unknown && <>{t('recent')} · </>}{t(view.pending ? 'running' : observedOperation!.status)}
      {observedOperation?.source_id && <> · {observedOperation.source_id}</>}
      {observedOperation?.rules != null && <> · {formatNumber(observedOperation.rules, language)} {t('input')}</>}
      {observedOperation?.error && <> · {failureText(observedOperation.error, language)}</>}
    </p>}
    {view.state && <><dl className="subscription-stats">
      <div><dt>{t('generation')}</dt><dd>{formatNumber(view.state.generation, language)}</dd></div>
      <div><dt>{t('input')}</dt><dd>{formatNumber(view.state.input_rules, language)}</dd></div>
      <div><dt>{t('indexed')}</dt><dd>{formatNumber(view.state.index_rules, language)}</dd></div>
      <div><dt>{t('memory')}</dt><dd>{formatNumber(view.state.index_bytes, language)} / {formatNumber(view.state.retained_bytes, language)} B</dd></div>
      <div><dt>{t('disk')}</dt><dd>{formatNumber(view.state.disk_bytes, language)} B</dd></div>
    </dl><p className="muted small">{t('countsHelp')}</p></>}
    {!sources.length && <p className="muted">{t('empty')}</p>}
    <div className="settings-stack">{sources.map((source, index) => {
      const baseline = saved?.find((item) => item.id === source.id && item.url === source.url && item.format === source.format);
      const status = baseline && revisionMatches ? view.state?.sources.find((item) => item.id === source.id) : undefined;
      return <SourceCard key={source.key} source={source} index={index} status={status} language={language} locked={locked || busy} working={working} saved={Boolean(baseline)}
        change={(next) => setSources(sources.map((item, at) => at === index ? next : item))} remove={() => setSources(sources.filter((_, at) => at !== index))}
        prepare={() => void view.start('prepare', source)} refresh={() => void view.start('refresh', source.id)} />;
    })}</div>
    <div className="button-group"><button type="button" className="button secondary" disabled={locked || busy || sources.length >= 16} onClick={() => add(false)}>{t('add')}</button>
      <button type="button" className="button quiet" disabled={locked || busy || sources.length >= 16} onClick={() => add(true)}>{t('example')}</button></div>
    <p className="muted small">{t('exampleHelp')} <a href="https://github.com/Natsuki-Kaede/Natsuki-List" target="_blank" rel="noreferrer">Natsuki List · GPLv3</a></p>
  </section><DomainCheck api={api} language={language} names={Object.fromEntries((revisionMatches ? saved ?? [] : []).map((source) => [source.id, source.name]))} /></>;
}
