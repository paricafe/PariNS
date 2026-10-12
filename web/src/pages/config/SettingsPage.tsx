import { useEffect, useState, type FormEvent, type ReactNode } from 'react';
import { Ellipsis } from 'lucide-react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { SPRING_IOS } from '../../components/beui/motion';
import { useNavigate } from 'react-router-dom';
import { cacheRuleDraft, defaultCacheRule, fieldDisplayValue, getPath, settingPages, ModelError, type CacheRule, type CacheRuleDraft, type RawFieldValue, type SettingField, type SettingPageId } from '../../model';
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
import { transportChangeText } from '../../components/TransportHint';
import { StorageTools } from './StorageTools';
import { SecurityRuntime } from './SecurityRuntime';
import type { SnapshotReport } from '../observability/storage';
import { UpdatePanel } from '../../features/updates/UpdatePanel';
import { SubscriptionsPanel } from '../../features/subscriptions/SubscriptionsPanel';
import { UnitInput, unitFamily } from '../../components/UnitInput';

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
  if (family) return row(<UnitInput id={id} label={label} family={family} value={textValue} disabled={disabled} language={language} onChange={onChange} describedBy={noteId} />);
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
  return <div className="settings-stack">{page.groups.map((group) => {
    const enabled = group.optional ? draft.optional[group.optional] ?? getPath(draft.settings, group.optional) !== null : true;
    const filterExternal = group.id === 'filterRules' && Boolean(draft.fields.filter_file ?? getPath(draft.settings, 'filter_file'));
    const disabled = locked || !enabled || filterExternal;
    const h3 = draft.fields['doh.http3'] ?? getPath(draft.settings, 'doh.http3');
    const endpoint = draft.fields['doh.listen'] ?? fieldDisplayValue(getPath(draft.settings, 'doh.listen'), settingPages.security.groups.find((candidate) => candidate.id === 'doh')!.fields[0]);
    const port = typeof endpoint === 'object' ? endpoint.port : '';
    const main = group.fields.filter((field) => !field.advanced);
    const more = group.fields.filter((field) => field.advanced);
    const open = moreOpen.has(group.id);
    const headingId = `group-${group.id}`;
    const rows = (fields: readonly SettingField[]) => fields.map((field) => <FieldRow key={field.path} field={field} language={language} disabled={disabled} value={value(field)} onChange={(next) => updateField(field.path, next)} />);
    return <section className="settings-group" key={group.id} aria-labelledby={headingId}>
      <h2 id={headingId} className="group-title">{t(group.titleKey)}</h2>
      <div className="grouped">
        {group.optional && <div className="row" data-layout="inline"><div className="row-label"><label htmlFor={`enable-${group.id}`}>{t(group.enableKey)}</label></div>
          <div className="row-control"><Switch id={`enable-${group.id}`} checked={enabled} disabled={locked} onCheckedChange={(next) => setOptional(group.optional!, next)} /></div></div>}
        <div className="settings-expand" data-open={enabled} inert={!enabled}><div>
          {rows(main)}
          {more.length > 0 && <ExpandRow label={t('shell.moreOptions')} open={open} onToggle={(next) => setMoreOpen((current) => { const set = new Set(current); if (next) set.add(group.id); else set.delete(group.id); return set; })}>{rows(more)}</ExpandRow>}
        </div></div>
      </div>
      {group.id === 'doh' && enabled && <p className="group-footer notice-footer">{translate(h3 === true ? 'storage.draftDoh3' : 'storage.draftDoh', language, { port: /^0+$/.test(port.trim()) ? t('storage.automaticPort') : port || '—' })}</p>}
      <p className="group-footer">{filterExternal ? t('settings.filter_file.help') : t(group.helpKey)}</p>
    </section>;
  })}</div>;
}

function CacheRules({ language }: { language: Language }) {
  const { draft, locked, setRules } = useConfig();
  const reduced = useReducedMotion();
  const t = (key: string) => translate(`views.${key}`, language);
  if (!draft) return null;
  const original = getPath(draft.settings, 'cache.rules');
  const rules = draft.rules ?? (Array.isArray(original) ? original.map((rule) => cacheRuleDraft(rule as CacheRule)) : []);
  const update = (index: number, key: keyof CacheRuleDraft, value: string) => setRules(rules.map((rule, at) => at === index ? { ...rule, [key]: value } : rule));
  const choices: Partial<Record<keyof CacheRuleDraft, readonly [string, string][]>> = {
    suffix: [['false', 'exact'], ['true', 'suffix']], bypass: [['false', 'allow'], ['true', 'bypass']],
    prefetch: [['', 'inherit'], ['true', 'on'], ['false', 'off']], stale: [['', 'inherit'], ['true', 'on'], ['false', 'off']],
  };
  const labels: [keyof CacheRuleDraft, string][] = [['name', 'name'], ['suffix', 'match'], ['qtype', 'qtype'], ['bypass', 'behavior'], ['max_ttl_secs', 'positiveTtl'], ['negative_ttl_cap_secs', 'negativeTtl'], ['prefetch', 'prefetch'], ['stale', 'stale']];
  return <section className="panel">
    <div className="heading-row"><div className="row-label"><h2>{t('rules')}</h2><InfoTip text={t('rulesHelp')} label={translate('shell.about', language, { label: t('rules') })} /></div>
      <button type="button" className="button secondary" disabled={locked || rules.length >= 256} onClick={() => setRules([...rules, cacheRuleDraft(defaultCacheRule())])}>{t('addRule')}</button></div>
    {rules.length === 0 && <p className="muted">{t('noRules')}</p>}
    <div className="rule-list"><AnimatePresence initial={false}>{rules.map((rule, index) => <motion.fieldset className="rule-row" disabled={locked} key={index}
      initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} animate={{ opacity: 1, height: 'auto', transitionEnd: { overflow: 'visible' } }} exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, overflow: 'hidden' }} transition={reduced ? { duration: 0.12 } : SPRING_IOS}><legend>{t('rule').replace('{number}', String(index + 1))}</legend>
      <div className="fields-grid">{labels.map(([key, label]) => <div className="field" key={key}>
        <label htmlFor={`rule-${index}-${key}`}>{t(label)}</label>
        {choices[key] ? <Segmented id={`rule-${index}-${key}`} label={t(label)} value={rule[key]} disabled={locked} onChange={(value) => update(index, key, value)} options={choices[key]!.map(([value, option]) => ({ value, label: t(option) }))} />
          : <input id={`rule-${index}-${key}`} value={rule[key]} inputMode={key.endsWith('_secs') ? 'numeric' : undefined} spellCheck={false} autoComplete="off" onChange={(event) => update(index, key, event.target.value)} />}
      </div>)}</div>
      <div className="rule-actions"><button type="button" className="button quiet" disabled={locked || index === 0} onClick={() => { const next = [...rules]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; setRules(next); }}>↑</button>
        <button type="button" className="button quiet" disabled={locked || index === rules.length - 1} onClick={() => { const next = [...rules]; [next[index], next[index + 1]] = [next[index + 1], next[index]]; setRules(next); }}>↓</button>
        <button type="button" className="button quiet" disabled={locked} onClick={() => setRules(rules.filter((_, at) => at !== index))}>{t('removeRule')}</button></div>
    </motion.fieldset>)}</AnimatePresence></div>
  </section>;
}

interface CacheStatus { revision: number; running: boolean; cache?: Record<string, number>; refresh?: Record<string, number>; cache_persistence?: SnapshotReport }
interface Inspection { revision: number; epoch: number; explanation: { state: 'fresh' | 'stale' | 'miss' | 'bypass'; reason: string; scope: string | null; policy?: Record<string, unknown> }; inspection: { variants: Record<string, unknown>[]; truncated: boolean } }

/** Restore outcome as one sentence; the raw reason stays available behind ⓘ. */
function restoreSummary(report: SnapshotReport, language: Language): string {
  const reason = report.reason;
  if (reason === null) return translate('shell.restoreDone', language, { restored: formatNumber(report.restored, language), skipped: formatNumber(report.skipped, language) });
  const key = reason === 'no_clean_snapshot' ? 'restoreNoSnapshot' : reason === 'persistence_disabled' ? 'restoreDisabled'
    : reason.startsWith('unsupported snapshot version') ? 'restoreFormat' : reason.startsWith('snapshot policy mismatch') ? 'restorePolicy' : 'restoreFailed';
  return translate(`shell.${key}`, language);
}

function CacheUsage({ language }: { language: Language }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const banner = useBanner();
  const [status, setStatus] = useState<CacheStatus | null>(null);
  const [error, setError] = useState<ConfigIssue | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const load = async () => { try { setStatus(await api.request<CacheStatus>('status')); setError(null); } catch (reason) { setError(toConfigIssue(reason)); } };
  useEffect(() => { void load(); }, [api]);
  const cache = status?.running ? status.cache : null;
  const lookups = cache ? (cache.hits ?? 0) + (cache.stale_hits ?? 0) + (cache.misses ?? 0) : 0;
  const details: [string, number | undefined, number | undefined][] = [
    ['positiveNegative', cache?.positive_entries, cache?.negative_entries], ['freshStale', cache?.hits, cache?.stale_hits],
    ['missBypass', cache?.misses, cache?.bypasses], ['evictionRejection', cache?.evictions, cache?.rejections],
  ];
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
          {row(translate('shell.hitRate', language), lookups ? translate('shell.hitRateValue', language, { percent: formatNumber(((cache.hits ?? 0) + (cache.stale_hits ?? 0)) / lookups * 100, language, { maximumFractionDigits: 0 }), hits: formatNumber((cache.hits ?? 0) + (cache.stale_hits ?? 0), language), misses: formatNumber(cache.misses ?? 0, language) }) : '—')}
          <ExpandRow label={translate('storage.detailedCounts', language)}>{details.map(([key, first, second]) => <div key={key}>{row(t(key), `${formatNumber(first ?? 0, language)} / ${formatNumber(second ?? 0, language)}`)}</div>)}</ExpandRow>
        </>}
      </div>
      <div className="group-actions"><button className="button quiet" type="button" onClick={() => void load()}>{translate('ui.refresh', language)}</button>
        {cache && <button type="button" className="button danger" disabled={working} onClick={async (event) => {
          const trigger = event.currentTarget;
          if (!status?.cache || !await confirmAction('views.clearAllConfirm', 'views.clearAll', trigger)) return;
          setWorking(true); try { await api.request('cache/invalidate', 'POST', { all: true, revision: status.revision, epoch: status.cache.epoch }); await load(); banner(t('cleared')); } catch (reason) { setError(toConfigIssue(reason)); } finally { setWorking(false); }
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

function CacheInspect({ language, dirty }: { language: Language; dirty: boolean }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const [name, setName] = useState(''); const [qtype, setQtype] = useState('A'); const [subnet, setSubnet] = useState('');
  const [edns, setEdns] = useState(true); const [dnssec, setDnssec] = useState(false); const [cd, setCd] = useState(false); const [rd, setRd] = useState(true);
  const [scope, setScope] = useState(''); const [result, setResult] = useState<Inspection | null>(null); const [error, setError] = useState<ConfigIssue | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const inspect = async (event: FormEvent) => {
    event.preventDefault(); setWorking(true); setResult(null); setError(null);
    try { setResult(await api.request<Inspection>('cache/inspect', 'POST', { name: name.trim(), qtype, subnet: subnet.trim() || null, edns, dnssec_ok: dnssec, checking_disabled: cd, recursion_desired: rd })); }
    catch (reason) { setError(toConfigIssue(reason)); }
    finally { setWorking(false); }
  };
  return <section className="panel"><h2>{t('inspectTitle')}</h2>{dirty && <p className="notice">{t('inspectActiveConfig')}</p>}
    <form onSubmit={(event) => void inspect(event)} className="fields-grid">
      <label className="field">{t('name')}<input value={name} onChange={(event) => setName(event.target.value)} required spellCheck={false} /></label>
      <label className="field">{t('qtype')}<input value={qtype} onChange={(event) => setQtype(event.target.value)} required spellCheck={false} /></label>
      <label className="field">{t('subnet')}<input value={subnet} onChange={(event) => setSubnet(event.target.value)} spellCheck={false} /></label>
      <div className="checkbox-row wide">{[['EDNS', edns, setEdns], ['DO', dnssec, setDnssec], ['CD', cd, setCd], ['RD', rd, setRd]].map(([label, checked, setter]) =>
        <label key={label as string}><input type="checkbox" checked={checked as boolean} onChange={(event) => (setter as (value: boolean) => void)(event.target.checked)} />{label as string}</label>)}</div>
      <button className="button primary" disabled={working} type="submit">{t('inspect')}</button>
    </form>
    {error && <p className="error" role="alert">{presentIssue(error, language)}</p>}
    {result && <div className="inspection-result"><h3>{t(`explain${result.explanation.state[0].toUpperCase()}${result.explanation.state.slice(1)}`)}</h3>
      <p>{t('explanation').replace('{reason}', t(result.explanation.reason)).replace('{scope}', (result.explanation.scope === null ? t('none') : hasTranslation(`views.${result.explanation.scope}`) ? t(result.explanation.scope) : result.explanation.scope))}</p>
      <p className="muted small">{t('variants')}: {result.inspection.variants.length}</p>
      <div className="variant-list">{result.inspection.variants.map((variant, index) => <details key={index}><summary>{String(variant.scope)} · {String(variant.state)}</summary><pre>{JSON.stringify(variant, null, 2)}</pre></details>)}</div>
      {result.inspection.truncated && <p>{t('variantsTruncated')}</p>}
      <div className="danger-zone"><label className="field">{t('scope')}<input value={scope} onChange={(event) => setScope(event.target.value)} /></label>
        <button type="button" className="button danger" disabled={working} onClick={async () => {
          if (!await confirmAction('views.clearSelectedConfirm', 'views.clearSelected')) return;
          setWorking(true);
          try { await api.request('cache/invalidate', 'POST', { name: name.trim(), qtype, scope: scope.trim() || null, revision: result.revision, epoch: result.epoch }); setResult(null); }
          catch (reason) { setError(toConfigIssue(reason)); } finally { setWorking(false); }
        }}>{t('clearSelected')}</button></div>
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
    if ((certificate || privateKey) && !await confirmAction('app.discardPem', 'shell.discardPem')) return;
    setTarget(null); setCertificate(''); setPrivateKey(''); setError(null);
  };
  return <section className="settings-group" aria-labelledby="certificate-import-title"><h2 id="certificate-import-title" className="group-title">{t('ui.pasteCertificate')}</h2><div className="grouped padded">
    {enabledProtocols.length ? <div className="button-group">{enabledProtocols.map((protocol) => <button type="button" className="button secondary" key={protocol}
      disabled={locked || configBusy} onClick={() => { setTarget(protocol); setError(null); }}>{t('ui.importCertificate')} · {protocol === 'doh' ? 'DoH' : protocol === 'dot' ? 'DoT' : 'DoQ'}</button>)}</div>
      : <p className="muted small">{t('shell.enableProtocolFirst')}</p>}
    <Drawer open={target !== null} variant="sheet" title={`${t('ui.pasteCertificate')} · ${target === 'doh' ? 'DoH' : target === 'dot' ? 'DoT' : target === 'doq' ? 'DoQ' : ''}`} closeLabel={t('ui.closeDialog')} onClose={() => void close()}>
    <p className="muted small">{t('app.importFillsDraft')}</p>
    <form className="fields-grid" onSubmit={async (event) => {
      event.preventDefault(); if (!target) return;
      if (locked || configBusy || busy) return;
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
        <button type="submit" className="button primary">{t('app.importAndFill')}</button>
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
  const intro = pageId === 'advanced' ? t('app.advancedIntro') : t(settingPages[pageId].introKey);
  const title = pageId === 'advanced' ? t('app.advanced') : t(settingPages[pageId].titleKey);
  if (pageId !== 'advanced' && needsParse) return <div className="page-stack"><h1>{title}</h1><p className="notice" role="status">{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate('/advanced')}>{t('app.returnToToml')}</button></div>;
  const actions = [
    { label: t('ui.validate'), disabled: !draft || busy, onSelect: () => void validate().then((result) => {
      if (result.transport_change) setNotice(transportChangeText(result.transport_change, language));
      else banner(t(dirty ? result.restart_required ? 'app.validRestart' : 'app.validCache' : 'shell.validSaved'));
    }).catch(() => {}) },
    { label: t(dirty ? 'ui.export' : 'shell.exportConfig'), disabled: !draft || busy, onSelect: () => void exportDraft().then(() => banner(t('app.exported'))).catch((reason) => setError(toConfigIssue(reason))) },
    { label: t('ui.rollback'), destructive: true, disabled: !draft?.hasBackup || busy, onSelect: (trigger: HTMLElement) => void (async () => {
      try {
        const prepared = await previewRollback();
        const change = prepared.transportChange;
        const message = change ? `${t('app.rollbackHelp')} ${transportChangeText(change, language)} ${change.requires_http_confirmation ? t('app.confirmHttpDowngrade') : ''}` : t('app.rollbackHelp');
        if (!await confirmAction(message, 'ui.rollback', trigger)) return;
        const result = await rollback(prepared);
        if (result.transportChange) { discard(); transportChanged(result.transportChange); }
        else if (result.refreshed) banner(t('app.restored'));
      } catch (reason) { setError(toConfigIssue(reason)); }
    })() },
  ];
  return <div className="page-stack"><header className="page-head large-title"><div><h1>{title}</h1><p className="muted">{intro}</p></div>
      <Menu label={t('shell.more')} icon={<Ellipsis size={17} aria-hidden="true" />} actions={actions} /></header>
    {error && <div className="notice error" role="alert">{presentIssue(error, language)}<button type="button" onClick={() => setError(null)} aria-label={t('shell.dismissNotice')}>×</button></div>}
    {notice && <div className="notice" role="status">{notice}<button type="button" onClick={() => setNotice(null)} aria-label={t('shell.dismissNotice')}>×</button></div>}
    {pageId === 'advanced' ? needsFormFlush ? <div className="notice"><p>{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate(`/${errorPage}`)}>{t('app.returnToEdit')}</button></div> : <section className="panel"><label htmlFor="advanced-toml" className="sr-only">TOML</label><textarea id="advanced-toml" className="toml-editor" value={draft?.toml ?? ''} disabled={busy || draft?.unknownApply} spellCheck={false} onChange={(event) => setToml(event.target.value)} /></section>
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
      {pageId === 'security' && <SecurityRuntime language={language} />}{pageId === 'storage' && <StorageTools language={language} />}{pageId === 'runtime' && <UpdatePanel language={language} />}<GenericSettings pageId={pageId} language={language} />{pageId === 'filters' && <SubscriptionsPanel language={language} />}{pageId === 'security' && <CertificateImport language={language} />}</>}
  </div>;
}
