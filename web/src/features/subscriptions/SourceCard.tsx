import { Switch } from '../../components/beui';
import { Select } from '../../components/Popover';
import { translate, formatDate, formatNumber, hasTranslation, type Language } from '../../i18n';
import { SUBSCRIPTION_ID, subscriptionId, type SubscriptionDraft, type SubscriptionSource } from '../../model/subscriptions';
import type { Failure, SourceStatus } from './types';

export function failureText(failure: Failure, language: Language) {
  const key = `subscriptions.${failure.code}`;
  return `${translate(hasTranslation(key) ? key : 'subscriptions.error', language)}${failure.line === null ? '' : ` (${translate('subscriptions.line', language, { line: failure.line })})`}`;
}

/** Status in words: saved sources report the server's state; new ones download when the configuration is saved. */
function stateKey(source: SubscriptionDraft, status: SourceStatus | undefined, savedSource: SubscriptionSource | undefined): string {
  if (status?.active) return 'active';
  if (!source.enabled) return 'inactive';
  if (!savedSource) return 'pendingSave';
  return status?.ready ? 'prepared' : 'missing';
}

export function SourceCard({ source, index, status, language, locked, working, savedSource, subscriptionsEnabled, takenIds, change, remove, refresh }: {
  source: SubscriptionDraft; index: number; status?: SourceStatus; language: Language;
  locked: boolean; working: boolean; savedSource?: SubscriptionSource; subscriptionsEnabled: boolean;
  /** IDs of the other sources, so a generated ID stays unique. */
  takenIds: readonly string[];
  change(source: SubscriptionDraft): void; remove(): void; refresh(): void;
}) {
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  const date = (value: number | null | undefined) => value == null ? t('never') : formatDate(value * 1000, language, { dateStyle: 'medium', timeStyle: 'short' });
  const field = (name: 'name' | 'url', wide = false) => <label className={`field ${wide ? 'wide' : ''}`}>{t(name)}
    <input id={`setting-filter_subscriptions-sources-${index}-${name}`} value={source[name]} autoComplete="off" spellCheck={false}
      onChange={(event) => change(name === 'name' && source.autoId
        ? { ...source, name: event.target.value, id: subscriptionId(event.target.value, takenIds) }
        : { ...source, [name]: event.target.value })} />
  </label>;
  return <fieldset className="subscription-card" disabled={locked}>
    <legend>{source.name || source.id || `${t('title')} ${index + 1}`}</legend>
    <div className="fields-grid">
      {field('name')}{field('url', true)}
      <div className="field"><span>{t('format')}</span><Select label={t('format')} value={source.format} onChange={(format) => change({ ...source, format: format as SubscriptionDraft['format'] })}
        options={[{ value: 'domain_list', label: t('domain_list') }, { value: 'hosts_blocklist', label: t('hosts_blocklist') }]} /></div>
      <label className="field">{t('interval')}<input id={`setting-filter_subscriptions-sources-${index}-update_interval_hours`} inputMode="numeric" value={source.update_interval_hours} onChange={(event) => change({ ...source, update_interval_hours: event.target.value })} /></label>
      <Switch label={t('enabled')} checked={source.enabled} onCheckedChange={(enabled) => change({ ...source, enabled })} />
      <Switch label={t('auto')} checked={source.auto_update} onCheckedChange={(auto_update) => change({ ...source, auto_update })} />
      <details className="more-options wide" open={!SUBSCRIPTION_ID.test(source.id) || undefined}><summary>{translate('shell.moreOptions', language)}</summary>
        <label className="field">{t('id')}<input id={`setting-filter_subscriptions-sources-${index}-id`} value={source.id} autoComplete="off" spellCheck={false}
          aria-describedby={`subscription-${source.key}-id-help`} onChange={(event) => change({ ...source, id: event.target.value, autoId: false })} />
          <small id={`subscription-${source.key}-id-help`} className="muted">{t('idHelp')}</small></label>
      </details>
    </div>
    <p className="small"><strong>{t(stateKey(source, status, savedSource))}</strong></p>
    {status && <dl className="subscription-stats">
      <div><dt>{t('input')}</dt><dd>{formatNumber(status.input_rules, language)}</dd></div>
      <div><dt>{t('lastSuccess')}</dt><dd>{date(status.last_success)}</dd></div>
      <div><dt>{t('lastAttempt')}</dt><dd>{date(status.last_attempt)}</dd></div>
      <div><dt>{t('next')}</dt><dd>{!subscriptionsEnabled || !savedSource?.enabled ? t('inactive')
        : !savedSource.auto_update ? t('manualOnly') : date(status.next_update)}</dd></div>
      <div><dt>{t('failures')}</dt><dd>{formatNumber(status.failures, language)}</dd></div>
    </dl>}
    {status?.error && <p className="notice error">{failureText(status.error, language)} {status.ready && t('old')}</p>}
    <div className="button-group"><button type="button" className="button secondary" disabled={working || !savedSource} onClick={refresh}>{t('refresh')}</button>
      <button type="button" className="button quiet" onClick={remove}>{t('remove')}</button></div>
  </fieldset>;
}
