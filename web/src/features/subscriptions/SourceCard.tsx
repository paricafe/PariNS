import { Switch } from '../../components/beui';
import { translate, formatDate, formatNumber, hasTranslation, type Language } from '../../i18n';
import type { SubscriptionDraft } from '../../model/subscriptions';
import type { Failure, SourceStatus } from './types';

export function failureText(failure: Failure, language: Language) {
  const key = `subscriptions.${failure.code}`;
  return `${translate(hasTranslation(key) ? key : 'subscriptions.error', language)}${failure.line === null ? '' : ` (${translate('subscriptions.line', language, { line: failure.line })})`}`;
}
export function SourceCard({ source, index, status, language, locked, working, saved, change, remove, prepare, refresh }: {
  source: SubscriptionDraft; index: number; status?: SourceStatus; language: Language;
  locked: boolean; working: boolean; saved: boolean;
  change(source: SubscriptionDraft): void; remove(): void; prepare(): void; refresh(): void;
}) {
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  const date = (value: number | null | undefined) => value == null ? t('never') : formatDate(value * 1000, language, { dateStyle: 'medium', timeStyle: 'short' });
  return <fieldset className="subscription-card" disabled={locked}>
    <legend>{source.name || source.id || `${t('title')} ${index + 1}`}</legend>
    <div className="fields-grid">
      {(['name', 'id', 'url'] as const).map((field) => <label className={`field ${field === 'url' ? 'wide' : ''}`} key={field}>{t(field)}
        <input id={`setting-filter_subscriptions-sources-${index}-${field}`} value={source[field]} autoComplete="off" spellCheck={false} onChange={(event) => change({ ...source, [field]: event.target.value })} />
      </label>)}
      <label className="field">{t('format')}<select value={source.format} onChange={(event) => change({ ...source, format: event.target.value as SubscriptionDraft['format'] })}>
        <option value="domain_list">{t('domain_list')}</option><option value="hosts_blocklist">{t('hosts_blocklist')}</option>
      </select></label>
      <label className="field">{t('interval')}<input id={`setting-filter_subscriptions-sources-${index}-update_interval_hours`} inputMode="numeric" value={source.update_interval_hours} onChange={(event) => change({ ...source, update_interval_hours: event.target.value })} /></label>
      <Switch label={t('enabled')} checked={source.enabled} onCheckedChange={(enabled) => change({ ...source, enabled })} />
      <Switch label={t('auto')} checked={source.auto_update} onCheckedChange={(auto_update) => change({ ...source, auto_update })} />
    </div>
    <p className="small"><strong>{status?.active ? t('active') : status?.ready ? t('prepared') : source.enabled ? t('missing') : t('inactive')}</strong></p>
    {status && <dl className="subscription-stats">
      <div><dt>{t('input')}</dt><dd>{formatNumber(status.input_rules, language)}</dd></div>
      <div><dt>{t('lastSuccess')}</dt><dd>{date(status.last_success)}</dd></div>
      <div><dt>{t('lastAttempt')}</dt><dd>{date(status.last_attempt)}</dd></div>
      <div><dt>{t('next')}</dt><dd>{date(status.next_update)}</dd></div>
      <div><dt>{t('failures')}</dt><dd>{formatNumber(status.failures, language)}</dd></div>
    </dl>}
    {status?.error && <p className="notice error">{failureText(status.error, language)} {status.ready && t('old')}</p>}
    <div className="button-group"><button type="button" className="button secondary" disabled={working || !source.id || !source.url || status?.active} onClick={prepare}>{t('prepare')}</button>
      <button type="button" className="button secondary" disabled={working || !saved} onClick={refresh}>{t('refresh')}</button>
      <button type="button" className="button quiet" onClick={remove}>{t('remove')}</button></div>
  </fieldset>;
}
