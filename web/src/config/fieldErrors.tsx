import { Link, useLocation } from 'react-router-dom';
import { presentIssue, translate, type Language } from '../i18n';
import { ModelError, settingPages, type CacheRuleDraft, type SettingPageId } from '../model';
import type { ConfigIssue } from './context';

export const cacheRuleFields: readonly [keyof CacheRuleDraft, string][] = [['name', 'name'], ['suffix', 'match'], ['qtype', 'qtype'], ['bypass', 'behavior'], ['max_ttl_secs', 'positiveTtl'], ['negative_ttl_cap_secs', 'negativeTtl'], ['prefetch', 'prefetch'], ['stale', 'stale']];

export function fieldErrorTarget(path: string): { page: SettingPageId; id?: string; labelKey: string; index?: number; kind?: 'rule' | 'source' } | null {
  const rule = /^cache\.rules\.(\d+)\.([a-z_]+)$/.exec(path);
  const ruleField = rule && cacheRuleFields.find(([key]) => key === rule[2]);
  if (rule && ruleField) return { page: 'cache', labelKey: `views.${ruleField[1]}`, index: Number(rule[1]), kind: 'rule' };
  const source = /^filter_subscriptions\.sources\.(\d+)\.([a-z0-9_]+)$/.exec(path);
  const sourceLabels: Record<string, string> = {
    name: 'subscriptions.name', url: 'subscriptions.url', id: 'subscriptions.id', format: 'subscriptions.format',
    update_interval_hours: 'subscriptions.interval', block_response: 'subscriptions.blockResponse',
    block_ipv4: 'settings.filter.block_ipv4.label', block_ipv6: 'settings.filter.block_ipv6.label',
  };
  if (source && sourceLabels[source[2]]) return { page: 'filters', id: `setting-${path.replaceAll('.', '-')}`, labelKey: sourceLabels[source[2]], index: Number(source[1]), kind: 'source' };
  for (const page of Object.values(settingPages)) {
    for (const group of page.groups) {
      const field = group.fields.find((field) => path === field.path || path.startsWith(`${field.path}.`));
      if (field) return { page: page.id as SettingPageId, id: `setting-${field.path.replaceAll('.', '-')}${field.type === 'endpoint' ? path === `${field.path}.port` ? '-port' : '-address' : ''}`, labelKey: field.labelKey };
    }
  }
  return null;
}

export function matchesFieldError(error: ConfigIssue | null, path: string): error is ModelError {
  return error instanceof ModelError && Boolean(error.path && (error.path === path || error.path.startsWith(`${path}.`)));
}

export function formatFieldPath(path: string, language: Language): string {
  const target = fieldErrorTarget(path);
  if (!target) return path;
  const label = translate(target.labelKey, language);
  return target.kind ? translate(`shell.${target.kind}Field`, language, { number: target.index! + 1, field: label }) : label;
}

export function ConfigErrorNotice({ error, language, onDismiss }: { error: ConfigIssue; language: Language; onDismiss?(): void }) {
  const location = useLocation();
  const target = error instanceof ModelError && error.path ? fieldErrorTarget(error.path) : null;
  const page = target ? translate(settingPages[target.page].titleKey, language) : '';
  const label = target ? translate(target.labelKey, language) : '';
  const field = target?.kind ? translate(`shell.${target.kind}Field`, language, { number: target.index! + 1, field: label }) : label;
  const detail = presentIssue(error, language);
  return <div className="notice error" role="alert">
    {target ? translate('shell.fieldError', language, { page, field, detail }) : detail}
    {target && location.pathname !== `/${target.page}` && <> <Link to={`/${target.page}`}>{translate('shell.correctField', language, { page })}</Link></>}
    {onDismiss && <button type="button" className="notice-close" onClick={onDismiss} aria-label={translate('shell.dismissNotice', language)}>×</button>}
  </div>;
}
