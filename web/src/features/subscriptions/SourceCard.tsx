import { Switch } from '../../components/beui';
import { Select } from '../../components/Popover';
import { matchesFieldError } from '../../config/fieldErrors';
import type { ConfigIssue } from '../../config/context';
import { translate, formatDate, formatNumber, hasTranslation, presentIssue, type Language } from '../../i18n';
import { SUBSCRIPTION_ID, subscriptionId, type SubscriptionDraft, type SubscriptionSource } from '../../model/subscriptions';
import { BLOCK_RESPONSES, type BlockResponse } from '../../model';
import type { Failure, SourceStatus } from './types';

export function failureText(failure: Failure, language: Language) {
  const key = `subscriptions.${failure.code}`;
  return `${translate(hasTranslation(key) ? key : 'subscriptions.error', language)}${failure.line === null ? '' : ` (${translate('subscriptions.line', language, { line: failure.line })})`}`;
}

function stateKey(source: SubscriptionDraft, status: SourceStatus | undefined, savedSource: SubscriptionSource | undefined): string {
  if (status?.active) return 'active';
  if (!source.enabled) return 'inactive';
  if (!savedSource) return 'pendingSave';
  return status?.ready ? 'prepared' : 'missing';
}

export function SourceCard({ source, index, status, language, locked, working, savedSource, subscriptionsEnabled, takenIds, error = null, change, remove, refresh }: {
  source: SubscriptionDraft; index: number; status?: SourceStatus; language: Language;
  locked: boolean; working: boolean; savedSource?: SubscriptionSource; subscriptionsEnabled: boolean;
  takenIds: readonly string[]; error?: ConfigIssue | null;
  change(source: SubscriptionDraft): void; remove(): void; refresh(): void;
}) {
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  const date = (value: number | null | undefined) => value == null ? t('never') : formatDate(value * 1000, language, { dateStyle: 'medium', timeStyle: 'short' });
  const fieldPath = (name: string) => `filter_subscriptions.sources.${index}.${name}`;
  const fieldId = (name: string) => `setting-filter_subscriptions-sources-${index}-${name}`;
  const fieldIssue = (name: string) => matchesFieldError(error, fieldPath(name)) ? error : null;
  const field = (name: 'name' | 'url', wide = false) => {
    const id = fieldId(name);
    const issue = fieldIssue(name);
    return <label className={`field ${wide ? 'wide' : ''}`}>{t(name)}
      <input id={id} value={source[name]} autoComplete="off" spellCheck={false}
        aria-invalid={issue ? true : undefined} aria-describedby={issue ? `${id}-error` : undefined}
        onChange={(event) => change(name === 'name' && source.autoId
          ? { ...source, name: event.target.value, id: subscriptionId(event.target.value, takenIds) }
          : { ...source, [name]: event.target.value })} />
      {issue && <small id={`${id}-error`} className="field-error">{presentIssue(issue, language)}</small>}
    </label>;
  };
  const intervalId = fieldId('update_interval_hours');
  const intervalIssue = fieldIssue('update_interval_hours');
  const idInputId = fieldId('id');
  const idIssue = fieldIssue('id');
  const idHelpId = `subscription-${source.key}-id-help`;
  return <fieldset className="subscription-card" disabled={locked}>
    <legend>{source.name || source.id || `${t('title')} ${index + 1}`}</legend>
    <div className="fields-grid">
      {field('name')}{field('url', true)}
      <div className="field"><span>{t('format')}</span><Select label={t('format')} value={source.format} onChange={(format) => change({ ...source, format: format as SubscriptionDraft['format'] })}
        options={[{ value: 'domain_list', label: t('domain_list') }, { value: 'hosts_blocklist', label: t('hosts_blocklist') }]} /></div>
      <label className="field">{t('interval')}<input id={intervalId} inputMode="numeric" value={source.update_interval_hours}
        aria-invalid={intervalIssue ? true : undefined} aria-describedby={intervalIssue ? `${intervalId}-error` : undefined}
        onChange={(event) => change({ ...source, update_interval_hours: event.target.value })} />
        {intervalIssue && <small id={`${intervalId}-error`} className="field-error">{presentIssue(intervalIssue, language)}</small>}</label>
      <div className="field"><span>{t('blockResponse')}</span><Select id={fieldId('block_response')} label={t('blockResponse')} value={source.block_response ?? ''}
        onChange={(mode) => change({ ...source, block_response: (mode || null) as BlockResponse | null })}
        options={[{ value: '', label: t('followGlobal') }, ...BLOCK_RESPONSES.map((mode) => ({ value: mode, label: translate(`settings.filter.block_response.${mode}`, language) }))]} /></div>
      {source.block_response === 'custom_ip' && (['block_ipv4', 'block_ipv6'] as const).map((name) => {
        const id = fieldId(name);
        const issue = fieldIssue(name);
        return <label className="field" key={name}>{translate(`settings.filter.${name}.label`, language)}
          <input id={id} value={source[name] ?? ''} autoComplete="off" spellCheck={false}
            aria-invalid={issue ? true : undefined} aria-describedby={issue ? `${id}-error` : undefined}
            onChange={(event) => change({ ...source, [name]: event.target.value })} />
          {issue && <small id={`${id}-error`} className="field-error">{presentIssue(issue, language)}</small>}</label>;
      })}
      <Switch label={t('enabled')} checked={source.enabled} onCheckedChange={(enabled) => change({ ...source, enabled })} />
      <Switch label={t('auto')} checked={source.auto_update} onCheckedChange={(auto_update) => change({ ...source, auto_update })} />
      <details className="more-options wide" open={!SUBSCRIPTION_ID.test(source.id) || Boolean(idIssue) || undefined}><summary>{translate('shell.moreOptions', language)}</summary>
        <label className="field">{t('id')}<input id={idInputId} value={source.id} autoComplete="off" spellCheck={false}
          aria-invalid={idIssue ? true : undefined} aria-describedby={idIssue ? `${idHelpId} ${idInputId}-error` : idHelpId}
          onChange={(event) => change({ ...source, id: event.target.value, autoId: false })} />
          <small id={idHelpId} className="muted">{t('idHelp')}</small>
          {idIssue && <small id={`${idInputId}-error`} className="field-error">{presentIssue(idIssue, language)}</small>}</label>
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
      <button type="button" className="button quiet danger-text" onClick={remove}>{t('remove')}</button></div>
  </fieldset>;
}
