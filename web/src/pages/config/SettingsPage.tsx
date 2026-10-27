import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { Ellipsis } from 'lucide-react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { SPRING_IOS } from '../../components/beui/motion';
import { useNavigate } from 'react-router-dom';
import { cacheRuleDraft, configList, defaultCacheRule, fieldDisplayValue, getPath, settingPages, ModelError, type CacheRuleDraft, type RawFieldValue, type SettingField, type SettingPageId } from '../../model';
import { formatBytes, formatNumber, hasTranslation, presentIssue, translate, type Language } from '../../i18n';
import { toConfigIssue, useConfig, type ConfigIssue } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import { Switch, Tabs, Drawer } from '../../components/beui';
import { InfoTip } from '../../components/InfoTip';
import { ExpandRow } from '../../components/ExpandRow';
import { Stepper } from '../../components/Stepper';
import { Segmented } from '../../components/Segmented';
import { Menu, Select } from '../../components/Popover';
import { useBanner } from '../../components/Banner';
import { useConfirm } from '../../components/ConfirmProvider';
import { TransportHint, transportChangeText } from '../../components/TransportHint';
import { lineDiff } from './lineDiff';
import { StorageTools } from './StorageTools';
import { SecurityRuntime } from './SecurityRuntime';
import { DohRuntimeRow } from './DohRuntime';
import type { SnapshotReport } from '../observability/storage';
import { UpdatePanel } from '../../features/updates/UpdatePanel';
import { SubscriptionsPanel } from '../../features/subscriptions/SubscriptionsPanel';
import { UnitInput, unitFamily } from '../../components/UnitInput';
import { cacheBreakdown, cacheHitRate, type StatusView } from '../observability/metrics';
import { scopeNote } from '../observability/diagnostics';

const cacheTabs = ['usage', 'settings', 'rules', 'inspect'] as const;
type CacheTab = typeof cacheTabs[number];

function FieldRow({ field, language, value, onChange, disabled }: { field: SettingField; language: Language; value: RawFieldValue; onChange(value: RawFieldValue): void; disabled?: boolean }) {
  const id = `setting-${field.path.replaceAll('.', '-')}`;
  const t = (key: string) => translate(key, language);
  const label = t(field.labelKey);
  const help = field.helpKey ? t(field.helpKey) : null;
  const tip = help && !field.visibleHelp ? <InfoTip text={help} label={translate('shell.about', language, { label })} /> : null;
  const noteId = help && field.visibleHelp ? `${id}-help` : undefined;
  const note = noteId ? <small id={noteId} className="row-note">{help}</small> : null;
  // "wide" rows (text fields, unit inputs, steppers) stack under the label on phones; switches and short pickers stay inline.
  const row = (control: ReactNode, layout: 'inline' | 'wide' | 'stacked' = 'wide', labelFor: string | undefined = id) => <div className="row" data-layout={layout}>
    <div className="row-label">{labelFor ? <label htmlFor={labelFor}>{label}</label> : <span>{label}</span>}{tip}</div>
    <div className="row-control">{control}</div>{note}
  </div>;
  if (field.type === 'endpoint') {
    const parts = typeof value === 'object' ? value : { address: '', port: '' };
    return <div className="row" data-layout="wide">
      <div className="row-label"><span>{label}</span></div>
      <div className="row-control endpoint-grid">
        <input id={`${id}-address`} aria-label={t('settings.listener.address.label')} value={parts.address} disabled={disabled} spellCheck={false} autoComplete="off" placeholder={t('settings.listener.address.placeholder')} onChange={(event) => onChange({ ...parts, address: event.target.value })} />
        <input id={`${id}-port`} aria-label={t('settings.listener.port.label')} className="port-input" value={parts.port} disabled={disabled} inputMode="numeric" spellCheck={false} autoComplete="off" placeholder="853" onChange={(event) => onChange({ ...parts, port: event.target.value })} />
      </div>
      {/^0+$/.test(parts.port.trim()) && <small className="row-note">{t('settings.listener.port.automatic')}</small>}
    </div>;
  }
  if (field.type === 'checkbox') return <div className="row" data-layout="inline">
    <div className="row-label"><label htmlFor={id}>{label}</label>{tip}</div>
    <div className="row-control"><Switch id={id} checked={value === true} disabled={disabled} onCheckedChange={onChange} aria-describedby={noteId} /></div>
    {note && <div className="settings-expand row-note-wrap" data-open={value === true} inert={value !== true}><div>{note}</div></div>}
  </div>;
  const textValue = typeof value === 'string' ? value : '';
  const family = field.type === 'number' ? unitFamily(field.path) : null;
  if (family) return row(<UnitInput id={id} label={label} family={family} max={field.max} value={textValue} disabled={disabled} language={language} onChange={onChange} describedBy={noteId} />);
  if (field.type === 'number' && field.min !== undefined && field.max !== undefined && field.max - field.min <= 256)
    return row(<Stepper id={id} label={label} value={textValue} min={field.min} max={field.max} disabled={disabled} language={language} onChange={onChange} describedBy={noteId} />);
  if (field.type === 'select') {
    const options = (field.options ?? []).map(([choice, key]) => ({ value: choice, label: t(key) }));
    return row(options.length <= 4 ? <Segmented id={id} label={label} value={textValue} options={options} disabled={disabled} onChange={onChange} />
      : <Select id={id} label={label} value={textValue} options={options} disabled={disabled} onChange={onChange} />, options.length <= 2 ? 'inline' : 'wide', undefined);
  }
  if (field.type === 'lines') return row(<textarea id={id} rows={4} value={textValue} disabled={disabled} spellCheck={false} aria-describedby={noteId}
    placeholder={hasTranslation(`settings.${field.path}.placeholder`) ? t(`settings.${field.path}.placeholder`) : undefined} onChange={(event) => onChange(event.target.value)} />, 'stacked');
  return row(<input id={id} type="text" className={field.type === 'number' ? 'number-input' : undefined} inputMode={field.type === 'number' ? 'numeric' : undefined} value={textValue} disabled={disabled} spellCheck={false} autoComplete="off" aria-describedby={noteId}
    placeholder={hasTranslation(`settings.${field.path}.placeholder`) ? t(`settings.${field.path}.placeholder`) : undefined} onChange={(event) => onChange(event.target.value)} />);
}

function GenericSettings({ pageId, language }: { pageId: SettingPageId; language: Language }) {
  const { draft, locked, updateField, setOptional, error } = useConfig();
  const [moreOpen, setMoreOpen] = useState<ReadonlySet<string>>(new Set());
  const page = settingPages[pageId];
  // A validation error on a field under “More options” opens that row so focus can reach it.
  useEffect(() => {
    if (!(error instanceof ModelError) || !error.path) return;
    const owner = page.groups.find((group) => group.fields.some((field) => field.advanced && (error.path === field.path || error.path?.startsWith(`${field.path}.`))));
    if (owner) setMoreOpen((open) => new Set(open).add(owner.id));
  }, [error, page]);
  if (!draft) return <p className="muted">{translate('app.connecting', language)}</p>;
  const t = (key: string) => translate(key, language);
  const value = (field: SettingField) => draft.fields[field.path] ?? fieldDisplayValue(getPath(draft.settings, field.path), field);
  const recursive = (draft.fields['resolution.mode'] ?? getPath(draft.settings, 'resolution.mode')) === 'recursive';
  const hasError = (field: SettingField) => error instanceof ModelError && error.path === field.path;
  // A conditional group or field stays visible while it holds a validation error, so focus can reach it.
  const shown = (condition: { path: string; value: string } | undefined) => !condition || (draft.fields[condition.path] ?? getPath(draft.settings, condition.path)) === condition.value;
  return <div className="settings-stack">{page.groups.map((group) => {
    if (!shown(group.when) && !group.fields.some(hasError)) return null;
    const enabled = group.optional ? draft.optional[group.optional] ?? getPath(draft.settings, group.optional) !== null : true;
    const filterExternal = group.id === 'filterRules' && Boolean(draft.fields.filter_file ?? getPath(draft.settings, 'filter_file'));
    const disabled = locked || !enabled || filterExternal;
    const visible = group.fields.filter((field) => shown(field.when) || hasError(field));
    const main = visible.filter((field) => !field.advanced);
    const more = visible.filter((field) => field.advanced);
    const open = moreOpen.has(group.id);
    const headingId = `group-${group.id}`;
    const rows = (fields: readonly SettingField[]) => fields.map((field) => <FieldRow key={field.path} field={field} language={language} disabled={disabled} value={value(field)} onChange={(next) => updateField(field.path, next)} />);
    return <section className="settings-group" key={group.id} aria-labelledby={headingId}>
      <h2 id={headingId} className="group-title">{t(group.titleKey)}</h2>
      <div className="grouped">
        {group.optional && <div className="row" data-layout="inline"><div className="row-label"><label htmlFor={`enable-${group.id}`}>{t(group.enableKey)}</label></div>
          <div className="row-control"><Switch id={`enable-${group.id}`} checked={enabled} disabled={locked} onCheckedChange={(next) => setOptional(group.optional!, next)} /></div></div>}
        {group.id === 'doh' && <DohRuntimeRow language={language} />}
        <div className="settings-expand" data-open={enabled} inert={!enabled}><div>
          {rows(main)}
          {more.length > 0 && <ExpandRow label={t('shell.moreOptions')} open={open} onToggle={(next) => setMoreOpen((current) => { const set = new Set(current); if (next) set.add(group.id); else set.delete(group.id); return set; })}>{rows(more)}</ExpandRow>}
        </div></div>
      </div>
      {group.id === 'upstreams' && recursive && <p className="group-footer notice-footer">{t('settings.group.upstreams.recursiveHint')}</p>}
      {(filterExternal || group.helpKey) && <p className={group.id === 'resolution' && recursive ? 'group-footer notice-footer' : 'group-footer'}>{t(filterExternal ? 'settings.filter_file.help' : group.helpKey!)}</p>}
    </section>;
  })}</div>;
}

/** Optional comparison of the TOML being edited with the saved configuration. */
function TomlChanges({ language }: { language: Language }) {
  const { draft } = useConfig();
  const [open, setOpen] = useState(false);
  const t = (key: string) => translate(key, language);
  const diff = useMemo(() => open && draft ? lineDiff(draft.original, draft.toml) : [], [open, draft?.original, draft?.toml]);
  if (!draft) return null;
  return <>
    <button type="button" className="button secondary" disabled={draft.toml === draft.original} onClick={() => setOpen(true)}>{t('app.showChanges')}</button>
    <Drawer open={open} variant="sheet" className="diff-sheet" title={t('app.diffTitle')} closeLabel={t('ui.closeDialog')} onClose={() => setOpen(false)}>
      <p className="muted small">{t('app.diffHelp')}</p>
      {draft.previewTransportChange && <TransportHint change={draft.previewTransportChange} language={language} />}
      {diff.some((line) => line.kind !== 'same') ? <pre className="diff-lines"><code>{diff.map((line, index) => <span className={`diff-${line.kind}`} key={index}>{line.kind === 'add' ? '+' : line.kind === 'remove' ? '−' : ' '}{line.text || ' '}</span>)}</code></pre>
        : <p className="muted">{t('app.noChanges')}</p>}
    </Drawer>
  </>;
}

export function CacheRules({ language }: { language: Language }) {
  const { draft, locked, setRules } = useConfig();
  const reduced = useReducedMotion();
  // Row keys follow add, move and remove, so a removed row is the one that animates out.
  const keys = useRef<number[]>([]);
  const serial = useRef(0);
  const t = (key: string) => translate(`views.${key}`, language);
  if (!draft) return null;
  const rules = draft.rules ?? configList(draft.settings, 'cache.rules').map((rule) => cacheRuleDraft(rule));
  if (keys.current.length !== rules.length) keys.current = rules.map(() => serial.current++);
  const update = (index: number, key: keyof CacheRuleDraft, value: string) => setRules(rules.map((rule, at) => at === index ? { ...rule, [key]: value } : rule));
  const move = (index: number, to: number) => {
    const next = [...rules]; [next[index], next[to]] = [next[to], next[index]];
    const order = [...keys.current]; [order[index], order[to]] = [order[to], order[index]];
    keys.current = order; setRules(next);
  };
  const choices: Partial<Record<keyof CacheRuleDraft, readonly [string, string][]>> = {
    suffix: [['false', 'exact'], ['true', 'suffix']], bypass: [['false', 'allow'], ['true', 'bypass']],
    prefetch: [['', 'inherit'], ['true', 'on'], ['false', 'off']], stale: [['', 'inherit'], ['true', 'on'], ['false', 'off']],
  };
  const labels: [keyof CacheRuleDraft, string][] = [['name', 'name'], ['suffix', 'match'], ['qtype', 'qtype'], ['bypass', 'behavior'], ['max_ttl_secs', 'positiveTtl'], ['negative_ttl_cap_secs', 'negativeTtl'], ['prefetch', 'prefetch'], ['stale', 'stale']];
  return <section className="panel">
    <div className="heading-row"><div className="row-label"><h2>{t('rules')}</h2><InfoTip text={t('rulesHelp')} label={translate('shell.about', language, { label: t('rules') })} /></div>
      <button type="button" className="button secondary" disabled={locked || rules.length >= 4096} onClick={() => { keys.current = [...keys.current, serial.current++]; setRules([...rules, cacheRuleDraft(defaultCacheRule())]); }}>{t('addRule')}</button></div>
    {rules.length === 0 && <p className="muted">{t('noRules')}</p>}
    <div className="rule-list"><AnimatePresence initial={false}>{rules.map((rule, index) => { const id = keys.current[index]; const number = { number: index + 1 }; return <motion.fieldset className="rule-row" disabled={locked} key={id}
      initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} animate={{ opacity: 1, height: 'auto', transitionEnd: { overflow: 'visible' } }} exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} transition={reduced ? { duration: 0.12 } : SPRING_IOS}><legend>{translate('views.rule', language, number)}</legend>
      <div className="fields-grid">{labels.map(([key, label]) => <div className="field" key={key}>
        <label htmlFor={`rule-${id}-${key}`}>{t(label)}</label>
        {choices[key] ? <Segmented id={`rule-${id}-${key}`} label={t(label)} value={rule[key]} disabled={locked} onChange={(value) => update(index, key, value)} options={choices[key]!.map(([value, option]) => ({ value, label: t(option) }))} />
          : <input id={`rule-${id}-${key}`} value={rule[key]} inputMode={key.endsWith('_secs') ? 'numeric' : undefined} spellCheck={false} autoComplete="off" onChange={(event) => update(index, key, event.target.value)} />}
      </div>)}</div>
      <div className="rule-actions"><button type="button" className="button quiet" aria-label={translate('views.moveRuleUp', language, number)} disabled={locked || index === 0} onClick={() => move(index, index - 1)}>↑</button>
        <button type="button" className="button quiet" aria-label={translate('views.moveRuleDown', language, number)} disabled={locked || index === rules.length - 1} onClick={() => move(index, index + 1)}>↓</button>
        <button type="button" className="button quiet" disabled={locked} onClick={() => { keys.current = keys.current.filter((_, at) => at !== index); setRules(rules.filter((_, at) => at !== index)); }}>{t('removeRule')}</button></div>
    </motion.fieldset>; })}</AnimatePresence></div>
  </section>;
}

interface Inspection { epoch: number; explanation: { state: 'fresh' | 'stale' | 'miss' | 'bypass'; reason: string; scope: string | null; policy?: Record<string, unknown> }; inspection: { variants: Record<string, unknown>[]; truncated: boolean; ecs_sharing?: 'observing' | 'shared' | 'revoked' | null } }

/** Restore outcome as one sentence; the raw reason stays available behind ⓘ. */
export function restoreSummary(report: SnapshotReport, language: Language): string {
  const reason = report.reason;
  const counts = { restored: formatNumber(report.restored, language), skipped: formatNumber(report.skipped, language) };
  if (reason === null) return translate('shell.restoreDone', language, counts);
  // Running out of time keeps the entries already loaded.
  if (reason === 'deadline') return translate('shell.restoreDeadline', language, counts);
  const key = reason === 'no_clean_snapshot' ? 'restoreNoSnapshot' : reason === 'persistence_disabled' ? 'restoreDisabled'
    : reason.startsWith('unsupported snapshot version') ? 'restoreFormat' : reason.startsWith('snapshot policy mismatch') ? 'restorePolicy' : 'restoreFailed';
  return translate(`shell.${key}`, language);
}

function CacheUsage({ language }: { language: Language }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const banner = useBanner();
  const [status, setStatus] = useState<StatusView | null>(null);
  const [error, setError] = useState<ConfigIssue | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const load = async () => { try { setStatus(await api.request<StatusView>('status')); setError(null); } catch (reason) { setError(toConfigIssue(reason)); } };
  useEffect(() => { void load(); }, [api]);
  const cache = status?.running ? status.cache : null;
  const lookups = cacheBreakdown(status?.metrics.counters);
  const hitRate = cacheHitRate(status?.metrics.counters);
  const count = (value: number) => formatNumber(value, language);
  const details: [string, number, number][] = cache ? [
    ['positiveNegative', cache.positive_entries, cache.negative_entries], ['freshStale', cache.hits, cache.stale_hits],
    ['missBypass', cache.misses, cache.bypasses], ['evictionRejection', cache.evictions, cache.rejections],
    ['ecsSharedHitsEntries', cache.ecs_shared_hits, cache.ecs_shared_entries],
  ] : [];
  const persistence = status?.cache_persistence;
  const row = (label: ReactNode, value: ReactNode) => <div className="row" data-layout="inline"><div className="row-label"><span>{label}</span></div><div className="row-control"><span className="row-value num">{value}</span></div></div>;
  return <>
    {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    <section className="settings-group" aria-labelledby="cache-usage-title">
      <h2 id="cache-usage-title" className="group-title">{t('usageTitle')}</h2>
      <div className="grouped">
        {!cache ? <div className="row"><span className="muted">{t('statsUnavailable')}</span></div> : <>
          <div className="usage-meters">{([['entryBudget', cache.entries, cache.max_entries, formatNumber(cache.entries, language), formatNumber(cache.max_entries, language)],
            ['byteBudget', cache.bytes, cache.max_bytes, formatBytes(cache.bytes, language), formatBytes(cache.max_bytes, language)]] as const).map(([key, used, max, usedText, maxText]) =>
            <div className="usage-meter" key={key}><span className="small muted">{t(key)}</span><strong className="num">{usedText} <span className="muted">/ {maxText}</span></strong>
              <div className="usage-track" role="meter" aria-valuemin={0} aria-valuemax={max || 1} aria-valuenow={used} aria-label={t(key)}><i style={{ transform: `scaleX(${max ? Math.min(1, used / max) : 0})` }} /></div></div>)}</div>
          <div className="row" data-layout="inline"><div className="row-label"><span>{translate('shell.hitRate', language)}</span><InfoTip text={translate('shell.hitRateHelp', language)} label={translate('shell.about', language, { label: translate('shell.hitRate', language) })} /></div>
            <div className="row-control"><span className="row-value num">{hitRate === null ? '—' : `${formatNumber(hitRate, language, { maximumFractionDigits: 1 })}%`}</span></div>
            {lookups && <small className="row-note">{translate('shell.cacheLookups', language, { hits: count(lookups.hits), stale: count(lookups.stale), joined: count(lookups.joined), misses: count(lookups.misses) })}</small>}</div>
          <ExpandRow label={translate('storage.detailedCounts', language)}>{details.map(([key, first, second]) => <div key={key}>{row(t(key), `${formatNumber(first, language)} / ${formatNumber(second, language)}`)}</div>)}
            {row(t('ecsSharingRevocations'), formatNumber(cache.ecs_sharing_revocations, language))}</ExpandRow>
        </>}
      </div>
      <div className="group-actions"><button className="button quiet" type="button" onClick={() => void load()}>{translate('ui.refresh', language)}</button>
        {cache && <button type="button" className="button danger" disabled={working} onClick={async (event) => {
          const trigger = event.currentTarget;
          if (!status?.cache || !await confirmAction(t('clearAllConfirm'), 'views.clearAll', trigger)) return;
          setWorking(true); try { await api.request('cache/invalidate', 'POST', { all: true, epoch: status.cache.epoch }); await load(); banner(t('cleared')); } catch (reason) { setError(toConfigIssue(reason)); } finally { setWorking(false); }
        }}>{t('clearAll')}</button>}</div>
    </section>
    {persistence && <section className="settings-group" aria-labelledby="cache-restore-title">
      <h2 id="cache-restore-title" className="group-title">{translate('settings.group.persistence.title', language)}</h2>
      <div className="grouped"><div className="row" data-layout="inline"><div className="row-label"><span>{restoreSummary(persistence, language)}</span>
        {persistence.reason && <InfoTip text={persistence.reason} label={translate('shell.rawReason', language)} />}</div>
        {persistence.bytes > 0 && <div className="row-control"><span className="row-value num muted">{formatBytes(persistence.bytes, language)}</span></div>}</div></div>
    </section>}
  </>;
}

interface Variant { name: string; qtype: string; scope: string; state: 'fresh' | 'stale' | 'expired' }

export function CacheInspect({ language, dirty }: { language: Language; dirty: boolean }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const [name, setName] = useState(''); const [qtype, setQtype] = useState('A'); const [subnet, setSubnet] = useState('');
  const [edns, setEdns] = useState(true); const [dnssec, setDnssec] = useState(false); const [cd, setCd] = useState(false); const [rd, setRd] = useState(true);
  const [result, setResult] = useState<Inspection | null>(null); const [error, setError] = useState<ConfigIssue | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const request = () => api.request<Inspection>('cache/inspect', 'POST', { name: name.trim(), qtype, subnet: subnet.trim() || null, edns, dnssec_ok: dnssec, checking_disabled: cd, recursion_desired: rd });
  const inspect = async (event: FormEvent) => {
    event.preventDefault(); setWorking(true); setResult(null); setError(null);
    try { setResult(await request()); }
    catch (reason) { setError(toConfigIssue(reason)); }
    finally { setWorking(false); }
  };
  /** Clears the listed entry's name, type and scope against the inspected epoch, then shows the cache again. */
  const clear = async (variant: Variant, trigger: HTMLElement) => {
    if (!result || !await confirmAction(t('clearEntryConfirm'), 'views.clearEntry', trigger)) return;
    setWorking(true); setError(null);
    try {
      await api.request('cache/invalidate', 'POST', { name: variant.name, qtype: variant.qtype, scope: variant.scope, epoch: result.epoch });
      setResult(await request());
    } catch (reason) { setError(toConfigIssue(reason)); } finally { setWorking(false); }
  };
  const variants = (result?.inspection.variants ?? []) as unknown as Variant[];
  return <section className="panel"><h2>{t('inspectTitle')}</h2>{dirty && <p className="notice">{t('inspectActiveConfig')}</p>}
    <form onSubmit={(event) => void inspect(event)} className="fields-grid">
      <label className="field">{t('name')}<input value={name} onChange={(event) => setName(event.target.value)} required spellCheck={false} /></label>
      <label className="field">{t('inspectQtype')}<input value={qtype} onChange={(event) => setQtype(event.target.value)} required spellCheck={false} /></label>
      <label className="field">{t('subnet')}<input value={subnet} onChange={(event) => setSubnet(event.target.value)} spellCheck={false} /></label>
      <details className="more-options wide"><summary>{translate('shell.moreOptions', language)}</summary>
        <div className="checkbox-row">{[['EDNS', edns, setEdns], ['DO', dnssec, setDnssec], ['CD', cd, setCd], ['RD', rd, setRd]].map(([label, checked, setter]) =>
          <label key={label as string}><input type="checkbox" checked={checked as boolean} onChange={(event) => (setter as (value: boolean) => void)(event.target.checked)} />{label as string}</label>)}</div>
      </details>
      <button className="button primary" disabled={working} type="submit">{t('inspect')}</button>
    </form>
    {error && <p className="error" role="alert">{presentIssue(error, language)}</p>}
    {result && <div className="inspection-result"><h3>{t(`explain${result.explanation.state[0].toUpperCase()}${result.explanation.state.slice(1)}`)}</h3>
      <p>{t('explanation').replace('{reason}', t(result.explanation.reason)).replace('{scope}', (result.explanation.scope === null ? t('none') : hasTranslation(`views.${result.explanation.scope}`) ? t(result.explanation.scope) : result.explanation.scope))}</p>
      {result.inspection.ecs_sharing && <p>{t(`ecsSharing${result.inspection.ecs_sharing[0].toUpperCase()}${result.inspection.ecs_sharing.slice(1)}`)}</p>}
      <p className="muted small">{t('variants')}: {variants.length}</p>
      <ul className="variant-list">{variants.map((variant, index) => <li key={index} className="variant-row">
        <span>{scopeNote(variant.scope, language)} · {t(`entry_${variant.state}`)}</span>
        <button type="button" className="button danger" disabled={working} onClick={(event) => void clear(variant, event.currentTarget)}>{t('clearEntry')}</button>
      </li>)}</ul>
      {result.inspection.truncated && <p>{t('variantsTruncated')}</p>}
    </div>}
  </section>;
}

function CertificateImport({ language }: { language: Language }) {
  const { draft, locked, busy: configBusy, importCertificate } = useConfig();
  const confirmAction = useConfirm();
  const [target, setTarget] = useState<'dot' | 'doh' | 'doq' | null>(null);
  const [certificate, setCertificate] = useState(''); const [privateKey, setPrivateKey] = useState('');
  const [error, setError] = useState<ConfigIssue | null>(null); const [busy, setBusy] = useState(false);
  const t = (key: string) => translate(key, language);
  if (!draft) return null;
  const protocols = ['dot', 'doh', 'doq'] as const;
  const enabledProtocols = protocols.filter((protocol) => draft.optional[protocol] ?? getPath(draft.settings, protocol) !== null);
  const close = async () => {
    if (busy) return;
    if ((certificate || privateKey) && !await confirmAction(t('app.discardPem'), 'shell.discardPem')) return;
    setTarget(null); setCertificate(''); setPrivateKey(''); setError(null);
  };
  return <section className="settings-group" aria-labelledby="certificate-import-title"><h2 id="certificate-import-title" className="group-title">{t('ui.pasteCertificate')}</h2><div className="grouped padded">
    {enabledProtocols.length ? <div className="button-group">{enabledProtocols.map((protocol) => <button type="button" className="button secondary" key={protocol}
      disabled={locked || configBusy} onClick={() => { setTarget(protocol); setError(null); }}>{t('ui.importCertificate')} · {protocol === 'doh' ? 'DoH' : protocol === 'dot' ? 'DoT' : 'DoQ'}</button>)}</div>
      : <p className="muted small">{t('shell.enableProtocolFirst')}</p>}
    <Drawer open={target !== null} variant="sheet" title={`${t('ui.pasteCertificate')} · ${target === 'doh' ? 'DoH' : target === 'dot' ? 'DoT' : target === 'doq' ? 'DoQ' : ''}`} closeLabel={t('ui.closeDialog')} onClose={() => void close()}>
    <form className="fields-grid" onSubmit={async (event) => {
      event.preventDefault(); if (!target) return;
      setBusy(true); setError(null);
      try {
        await importCertificate(target, certificate, privateKey);
        setCertificate(''); setPrivateKey('');
        setTarget(null);
      } catch (reason) {
        if (reason instanceof StaleRequest || reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code)) {
          setCertificate(''); setPrivateKey('');
          setError('app.certificateImportUnknown');
        } else setError(toConfigIssue(reason));
      }
      finally { setBusy(false); }
    }}>
      <fieldset className="fields-grid wide" disabled={busy || locked || configBusy}>
        <label className="field wide">{t('ui.certificate')}<textarea rows={5} value={certificate} onChange={(event) => setCertificate(event.target.value)} required spellCheck={false} autoComplete="off" /></label>
        <label className="field wide">{t('ui.privateKey')}<textarea rows={5} value={privateKey} onChange={(event) => setPrivateKey(event.target.value)} required spellCheck={false} autoComplete="off" /></label>
        <button type="submit" className="button primary">{t('app.importCertificate')}</button>
      </fieldset>
    </form>{error && <p className="error" role="alert">{presentIssue(error, language)}</p>}</Drawer>
  </div><p className="group-footer">{t('ui.certificateHelp')}</p></section>;
}

export function SettingsPage({ pageId, language }: { pageId: SettingPageId | 'advanced'; language: Language }) {
  const { draft, setToml, preview, validate, ensureParsed, previewRollback, rollback, exportDraft, busy, error, setError, discard, dirty } = useConfig();
  const { state, transportChanged } = useSession();
  const confirmAction = useConfirm();
  const banner = useBanner();
  const navigate = useNavigate();
  const [cacheTab, setCacheTab] = useState<CacheTab>('usage');
  const [notice, setNotice] = useState<string | null>(null);
  const needsFormFlush = Boolean(draft && (Object.keys(draft.fields).length || Object.keys(draft.optional).length || draft.rules !== null || draft.sources !== null));
  const needsParse = Boolean(draft?.stale);
  useEffect(() => {
    if (pageId === 'advanced' && needsFormFlush) void preview().catch(() => {});
    if (pageId !== 'advanced' && needsParse) void ensureParsed().catch((reason) => setError(toConfigIssue(reason)));
  }, [pageId, needsFormFlush, needsParse]);
  useEffect(() => {
    if (!(error instanceof ModelError) || !error.path || pageId === 'advanced') return;
    const rule = /^cache\.rules\.(\d+)\.([a-z_]+)$/.exec(error.path);
    if (rule && cacheTab !== 'rules') { setCacheTab('rules'); return; }
    if (pageId === 'cache' && !rule && cacheTab !== 'settings') { setCacheTab('settings'); return; }
    const id = rule ? `rule-${rule[1]}-${rule[2]}` : `setting-${error.path.replaceAll('.', '-')}`;
    // Rows under “More options” open on the error; focus once their height transition has started.
    window.setTimeout(() => document.getElementById(id)?.focus(), 0);
  }, [error, pageId, cacheTab]);
  const t = (key: string) => translate(key, language);
  const errorPage = error instanceof ModelError && error.path
    ? (Object.entries(settingPages).find(([, page]) => page.groups.some((group) => group.fields.some((field) => error.path === field.path || error.path?.startsWith(`${field.path}.`))))?.[0] as SettingPageId | undefined)
      ?? (error.path.startsWith('cache.rules.') ? 'cache' : error.path.startsWith('filter_subscriptions.') ? 'filters' : 'dns')
    : 'dns';
  const intro = pageId === 'advanced' ? null : t(settingPages[pageId].introKey);
  const title = pageId === 'advanced' ? t('app.advanced') : t(settingPages[pageId].titleKey);
  if (pageId !== 'advanced' && needsParse) return <div className="page-stack"><h1>{title}</h1><p className="notice" role="status">{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate('/advanced')}>{t('app.returnToToml')}</button></div>;
  // A check without saving belongs with the raw TOML editor; settings pages simply save.
  const check = () => void validate().then((result) => {
    if (result.transport_change) setNotice(transportChangeText(result.transport_change, language));
    else banner(t(dirty ? result.restart_required ? 'app.validRestart' : 'app.validCache' : 'shell.validSaved'));
  }).catch(() => {});
  const actions = [
    { label: t(dirty ? 'ui.export' : 'shell.exportConfig'), disabled: !draft || busy, onSelect: () => void exportDraft().then(() => banner(t('app.exported'))).catch((reason) => setError(toConfigIssue(reason))) },
    { label: t('ui.rollback'), destructive: true, disabled: !draft?.hasBackup || busy, onSelect: (trigger: HTMLElement) => void (async () => {
      try {
        const prepared = await previewRollback();
        if (!prepared) return;
        const change = prepared.transportChange;
        const message = change ? `${t('app.rollbackHelp')} ${transportChangeText(change, language)} ${change.requires_http_confirmation ? t('app.confirmHttpDowngrade') : ''}` : t('app.rollbackHelp');
        if (!await confirmAction(message, 'ui.rollback', trigger)) return;
        const result = await rollback(prepared);
        if (result.transportChange) { discard(); transportChanged(result.transportChange); }
        else if (result.refreshed) banner(t(result.restartRequired ? 'app.restoredRestarted' : 'app.restored'));
      } catch (reason) { setError(toConfigIssue(reason)); }
    })() },
  ];
  return <div className="page-stack"><header className="page-head large-title"><div><h1>{title}</h1>{intro && <p className="muted">{intro}</p>}</div>
      <Menu label={t('shell.more')} icon={<Ellipsis size={17} aria-hidden="true" />} actions={actions} /></header>
    {error && <div className="notice error" role="alert">{presentIssue(error, language)}<button type="button" onClick={() => setError(null)} aria-label={t('shell.dismissNotice')}>×</button></div>}
    {notice && <div className="notice" role="status">{notice}<button type="button" onClick={() => setNotice(null)} aria-label={t('shell.dismissNotice')}>×</button></div>}
    {pageId === 'advanced' ? needsFormFlush ? <div className="notice"><p>{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate(`/${errorPage}`)}>{t('app.returnToEdit')}</button></div> : <section className="panel"><div className="toml-tools button-group"><button type="button" className="button secondary" disabled={!draft || busy} onClick={check}>{t('ui.validate')}</button><TomlChanges language={language} /></div><label htmlFor="advanced-toml" className="sr-only">TOML</label><textarea id="advanced-toml" className="toml-editor" value={draft?.toml ?? ''} disabled={busy || draft?.unknownApply} spellCheck={false} onChange={(event) => setToml(event.target.value)} /></section>
      : pageId === 'cache' ? <>
        <Tabs label={title} items={cacheTabs.map((tab) => ({ value: tab, label: t(`views.cacheTab${tab[0].toUpperCase()}${tab.slice(1)}`) }))} value={cacheTab} onChange={(value) => setCacheTab(value as CacheTab)} />
        {cacheTab === 'usage' && <CacheUsage language={language} />}
        {cacheTab === 'settings' && <GenericSettings pageId="cache" language={language} />}
        {cacheTab === 'rules' && <CacheRules language={language} />}
        {cacheTab === 'inspect' && <CacheInspect language={language} dirty={dirty} />}
      </> : <>{pageId === 'security' && <section className="settings-group" aria-labelledby="console-address">
        <h2 id="console-address" className="group-title">{t('app.transportCurrentAddress')}</h2>
        <div className="grouped"><div className="row" data-layout="inline"><div className="row-label"><span>{t(state.transport?.scheme === 'https' ? 'app.transportHttps' : 'app.transportHttp')}</span></div>
          <div className="row-control"><span className="row-value transport-address">{state.transport?.origin ?? window.location.origin}</span></div></div></div>
        <p className="group-footer">{t(state.transport?.scheme === 'https' ? 'app.transportHttpsHelp' : 'app.transportHttpHelp')}</p>
      </section>}
      {pageId === 'security' ? <SecurityRuntime language={language}><GenericSettings pageId={pageId} language={language} /></SecurityRuntime> : <>
        {pageId === 'storage' && <StorageTools language={language} />}{pageId === 'updates' && <UpdatePanel language={language} />}<GenericSettings pageId={pageId} language={language} /></>}{pageId === 'filters' && <SubscriptionsPanel language={language} />}{pageId === 'security' && <CertificateImport language={language} />}</>}
  </div>;
}
