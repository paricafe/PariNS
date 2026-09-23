import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { cacheRuleDraft, defaultCacheRule, fieldDisplayValue, getPath, settingPages, ModelError, type CacheRule, type CacheRuleDraft, type RawFieldValue, type SettingField, type SettingPageId } from '../../model';
import { formatNumber, presentIssue, translate, type Language } from '../../i18n';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import { Switch, Tabs, Button, Drawer } from '../../components/beui';
import { useConfirm } from '../../components/ConfirmProvider';
import { lineDiff } from './lineDiff';

const cacheTabs = ['usage', 'settings', 'rules', 'inspect'] as const;
type CacheTab = typeof cacheTabs[number];

function Field({ field, language, value, onChange, disabled }: { field: SettingField; language: Language; value: RawFieldValue; onChange(value: RawFieldValue): void; disabled?: boolean }) {
  const id = `setting-${field.path.replaceAll('.', '-')}`;
  const t = (key: string) => translate(key, language);
  if (field.type === 'endpoint') {
    const parts = typeof value === 'object' ? value : { address: '', port: '' };
    return <div className="field wide endpoint">
      <div className="endpoint-grid">
        <label htmlFor={`${id}-address`}>{t('settings.listener.address.label')}
          <input id={`${id}-address`} value={parts.address} disabled={disabled} spellCheck={false} autoComplete="off" onChange={(event) => onChange({ ...parts, address: event.target.value })} />
        </label>
        <label htmlFor={`${id}-port`}>{t('settings.listener.port.label')}
          <input id={`${id}-port`} value={parts.port} disabled={disabled} inputMode="numeric" spellCheck={false} autoComplete="off" onChange={(event) => onChange({ ...parts, port: event.target.value })} />
        </label>
      </div>
      <small>{t('settings.listener.address.help')}</small>
      {/^0+$/.test(parts.port.trim()) && <small>{t('settings.listener.port.automatic')}</small>}
    </div>;
  }
  if (field.type === 'checkbox') return <div className="switch-field">
    <Switch id={id} checked={value === true} disabled={disabled} onCheckedChange={onChange} label={t(field.labelKey)} />
    {field.helpKey && <small>{t(field.helpKey)}</small>}
  </div>;
  const textValue = typeof value === 'string' ? value : '';
  return <div className={`field ${field.type === 'lines' ? 'wide' : ''}`}>
    <label htmlFor={id}>{t(field.labelKey)}</label>
    {field.type === 'lines' ? <textarea id={id} rows={5} value={textValue} disabled={disabled} spellCheck={false} onChange={(event) => onChange(event.target.value)} />
      : field.type === 'select' ? <select id={id} value={textValue} disabled={disabled} onChange={(event) => onChange(event.target.value)}>
        {field.options?.map(([choice, key]) => <option key={choice} value={choice}>{t(key)}</option>)}
      </select>
        : <input id={id} type="text" inputMode={field.type === 'number' ? 'numeric' : undefined} value={textValue} disabled={disabled} spellCheck={false} autoComplete="off" onChange={(event) => onChange(event.target.value)} />}
    {field.helpKey && <small>{t(field.helpKey)}</small>}
  </div>;
}

function GenericSettings({ pageId, language }: { pageId: SettingPageId; language: Language }) {
  const { draft, locked, updateField, setOptional } = useConfig();
  if (!draft) return <p className="muted">{translate('app.connecting', language)}</p>;
  const page = settingPages[pageId];
  const t = (key: string) => translate(key, language);
  return <div className="settings-stack">{page.groups.map((group) => {
    const enabled = group.optional ? draft.optional[group.optional] ?? getPath(draft.settings, group.optional) !== null : true;
    const filterExternal = group.id === 'filterRules' && Boolean(draft.fields.filter_file ?? getPath(draft.settings, 'filter_file'));
    return <section className="panel settings-group" key={group.id}>
      <div className="group-head"><div><h2>{t(group.titleKey)}</h2><p className="muted small">{t(group.helpKey)}</p></div>
        {group.optional && <Switch checked={enabled} disabled={locked} onCheckedChange={(next) => setOptional(group.optional!, next)} label={t(group.enableKey)} />}
      </div>
      <fieldset disabled={locked || !enabled || filterExternal} className="fields-grid"><legend className="sr-only">{t(group.titleKey)}</legend>
        {group.fields.map((field) => <Field key={field.path} field={field} language={language} disabled={locked || !enabled || filterExternal}
          value={draft.fields[field.path] ?? fieldDisplayValue(getPath(draft.settings, field.path), field)}
          onChange={(value) => updateField(field.path, value)} />)}
      </fieldset>
      {filterExternal && <p className="muted small">{t('settings.filter_file.help')}</p>}
    </section>;
  })}</div>;
}

function CacheRules({ language }: { language: Language }) {
  const { draft, locked, setRules } = useConfig();
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
    <div className="heading-row"><div><h2>{t('rules')}</h2><p className="muted small">{t('rulesHelp')}</p></div>
      <button type="button" className="button secondary" disabled={locked || rules.length >= 256} onClick={() => setRules([...rules, cacheRuleDraft(defaultCacheRule())])}>{t('addRule')}</button></div>
    {rules.length === 0 && <p className="muted">{t('noRules')}</p>}
    <div className="rule-list">{rules.map((rule, index) => <fieldset className="rule-row" disabled={locked} key={index}><legend>{t('rule').replace('{number}', String(index + 1))}</legend>
      <div className="fields-grid">{labels.map(([key, label]) => <div className="field" key={key}>
        <label htmlFor={`rule-${index}-${key}`}>{t(label)}</label>
        {choices[key] ? <select id={`rule-${index}-${key}`} value={rule[key]} onChange={(event) => update(index, key, event.target.value)}>{choices[key]!.map(([value, option]) => <option key={value} value={value}>{t(option)}</option>)}</select>
          : <input id={`rule-${index}-${key}`} value={rule[key]} inputMode={key.endsWith('_secs') ? 'numeric' : undefined} spellCheck={false} autoComplete="off" onChange={(event) => update(index, key, event.target.value)} />}
      </div>)}</div>
      <div className="rule-actions"><button type="button" className="button quiet" disabled={locked || index === 0} onClick={() => { const next = [...rules]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; setRules(next); }}>↑</button>
        <button type="button" className="button quiet" disabled={locked || index === rules.length - 1} onClick={() => { const next = [...rules]; [next[index], next[index + 1]] = [next[index + 1], next[index]]; setRules(next); }}>↓</button>
        <button type="button" className="button quiet" disabled={locked} onClick={() => setRules(rules.filter((_, at) => at !== index))}>{t('removeRule')}</button></div>
    </fieldset>)}</div>
  </section>;
}

interface CacheStatus { revision: number; running: boolean; cache?: Record<string, number>; refresh?: Record<string, number> }
interface Inspection { revision: number; epoch: number; explanation: { state: string; reason: string; scope: string; policy?: Record<string, unknown> }; inspection: { variants: Record<string, unknown>[]; truncated: boolean } }

function CacheUsage({ language }: { language: Language }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const [status, setStatus] = useState<CacheStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const load = async () => { try { setStatus(await api.request<CacheStatus>('status')); setError(null); } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); } };
  useEffect(() => { void load(); }, [api]);
  const cache = status?.running ? status.cache : null;
  const rows: [string, number | undefined, number | undefined][] = [
    ['entriesLimit', cache?.entries, cache?.max_entries], ['bytesLimit', cache?.bytes, cache?.max_bytes],
    ['positiveNegative', cache?.positive_entries, cache?.negative_entries], ['freshStale', cache?.hits, cache?.stale_hits],
    ['missBypass', cache?.misses, cache?.bypasses], ['evictionRejection', cache?.evictions, cache?.rejections],
  ];
  return <section className="panel"><div className="heading-row"><h2>{t('usageTitle')}</h2><button className="button secondary" type="button" onClick={() => void load()}>{translate('ui.refresh', language)}</button></div>
    {error && <p className="error" role="alert">{error}</p>}
    {!cache ? <p className="muted">{t('statsUnavailable')}</p> : <>
      <div className="metric-grid">{[['entryBudget', cache.entries, cache.max_entries], ['byteBudget', cache.bytes, cache.max_bytes]].map(([key, used, max]) =>
        <div className="meter-card" key={key}><span>{t(String(key))}</span><strong>{formatNumber(Number(used), language)} / {formatNumber(Number(max), language)}</strong><meter min={0} max={Number(max) || 1} value={Number(used)} aria-label={t(String(key))} /></div>)}</div>
      <div className="detail-list">{rows.map(([key, first, second]) => <div className="detail-row" key={key}><span>{t(key)}</span><strong>{formatNumber(first ?? 0, language)} / {formatNumber(second ?? 0, language)}</strong></div>)}</div>
      <p className="muted small">{t('bytesNote')}</p>
    </>}
    <div className="danger-zone"><button type="button" className="button danger" disabled={!cache || working} onClick={async () => {
      if (!status?.cache || !await confirmAction('views.clearAllConfirm', 'views.clearAll')) return;
      setWorking(true); try { await api.request('cache/invalidate', 'POST', { all: true, revision: status.revision, epoch: status.cache.epoch }); await load(); } catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); } finally { setWorking(false); }
    }}>{t('clearAll')}</button></div>
  </section>;
}

function CacheInspect({ language, dirty }: { language: Language; dirty: boolean }) {
  const { api } = useSession();
  const confirmAction = useConfirm();
  const [name, setName] = useState(''); const [qtype, setQtype] = useState('A'); const [subnet, setSubnet] = useState('');
  const [edns, setEdns] = useState(true); const [dnssec, setDnssec] = useState(false); const [cd, setCd] = useState(false); const [rd, setRd] = useState(true);
  const [scope, setScope] = useState(''); const [result, setResult] = useState<Inspection | null>(null); const [error, setError] = useState<string | null>(null);
  const [working, setWorking] = useState(false);
  const t = (key: string) => translate(`views.${key}`, language);
  const inspect = async (event: FormEvent) => {
    event.preventDefault(); setWorking(true); setResult(null); setError(null);
    try { setResult(await api.request<Inspection>('cache/inspect', 'POST', { name: name.trim(), qtype, subnet: subnet.trim() || null, edns, dnssec_ok: dnssec, checking_disabled: cd, recursion_desired: rd })); }
    catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
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
    {error && <p className="error" role="alert">{error}</p>}
    {result && <div className="inspection-result"><h3>{t(result.explanation.state === 'stale' ? 'staleState' : result.explanation.state)}</h3>
      <p>{t('explanation').replace('{reason}', t(result.explanation.reason)).replace('{scope}', t(result.explanation.scope))}</p>
      <p className="muted small">{t('variants')}: {result.inspection.variants.length}</p>
      <div className="variant-list">{result.inspection.variants.map((variant, index) => <details key={index}><summary>{String(variant.scope)} · {String(variant.state)}</summary><pre>{JSON.stringify(variant, null, 2)}</pre></details>)}</div>
      {result.inspection.truncated && <p>{t('variantsTruncated')}</p>}
      <div className="danger-zone"><label className="field">{t('scope')}<input value={scope} onChange={(event) => setScope(event.target.value)} /></label>
        <button type="button" className="button danger" disabled={working} onClick={async () => {
          if (!await confirmAction('views.clearSelectedConfirm', 'views.clearSelected')) return;
          setWorking(true);
          try { await api.request('cache/invalidate', 'POST', { name: name.trim(), qtype, scope: scope.trim() || null, revision: result.revision, epoch: result.epoch }); setResult(null); }
          catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); } finally { setWorking(false); }
        }}>{t('clearSelected')}</button></div>
    </div>}
  </section>;
}

function CertificateImport({ language }: { language: Language }) {
  const { draft, locked, busy: configBusy, importCertificate } = useConfig();
  const confirmAction = useConfirm();
  const [target, setTarget] = useState<'dot' | 'doh' | 'doq' | 'doh3' | null>(null);
  const [certificate, setCertificate] = useState(''); const [privateKey, setPrivateKey] = useState('');
  const [error, setError] = useState<string | null>(null); const [busy, setBusy] = useState(false);
  const t = (key: string) => translate(key, language);
  if (!draft) return null;
  const protocols = ['dot', 'doh', 'doq', 'doh3'] as const;
  const close = async () => {
    if (busy) return;
    if ((certificate || privateKey) && !await confirmAction('app.discardPem', 'ui.closeDialog')) return;
    setTarget(null); setCertificate(''); setPrivateKey(''); setError(null);
  };
  return <section className="panel"><h2>{t('ui.pasteCertificate')}</h2><p className="muted small">{t('ui.certificateHelp')}</p>
    <div className="button-group">{protocols.map((protocol) => <button type="button" className="button secondary" key={protocol}
      disabled={locked || configBusy || !(draft.optional[protocol] ?? getPath(draft.settings, protocol) !== null)}
      onClick={() => { setTarget(protocol); setError(null); }}>{t('ui.importCertificate')} · {protocol.toUpperCase()}</button>)}</div>
    <Drawer open={target !== null} title={`${t('ui.pasteCertificate')} · ${target?.toUpperCase() ?? ''}`} closeLabel={t('ui.closeDialog')} onClose={() => void close()}>
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
          setError(t('app.certificateImportUnknown'));
        } else setError(reason instanceof ApiError || reason instanceof ModelError ? presentIssue(reason, language) : reason instanceof Error ? presentIssue(reason.message, language) : String(reason));
      }
      finally { setBusy(false); }
    }}>
      <fieldset className="fields-grid wide" disabled={busy || locked || configBusy}>
        <label className="field wide">{t('ui.certificate')}<textarea rows={5} value={certificate} onChange={(event) => setCertificate(event.target.value)} required spellCheck={false} autoComplete="off" /></label>
        <label className="field wide">{t('ui.privateKey')}<textarea rows={5} value={privateKey} onChange={(event) => setPrivateKey(event.target.value)} required spellCheck={false} autoComplete="off" /></label>
        <button type="submit" className="button primary">{t('app.importAndFill')}</button>
      </fieldset>
    </form>{error && <p className="error" role="alert">{error}</p>}</Drawer>
  </section>;
}

export function SettingsPage({ pageId, language }: { pageId: SettingPageId | 'advanced'; language: Language }) {
  const { draft, dirty, setToml, preview, validate, ensureParsed, rollback, exportDraft, busy, error, setError } = useConfig();
  const confirmAction = useConfirm();
  const navigate = useNavigate();
  const [cacheTab, setCacheTab] = useState<CacheTab>('usage');
  const [notice, setNotice] = useState<string | null>(null);
  const [previewed, setPreviewed] = useState<{ source: string; result: string } | null>(null);
  const diff = useMemo(() => previewed ? lineDiff(previewed.source, previewed.result) : [], [previewed]);
  const needsFormFlush = Boolean(draft && (Object.keys(draft.fields).length || Object.keys(draft.optional).length || draft.rules !== null));
  const needsParse = Boolean(draft?.stale);
  useEffect(() => {
    if (pageId === 'advanced' && needsFormFlush) void preview().catch(() => {});
    if (pageId !== 'advanced' && needsParse) void ensureParsed().catch((reason) => setError(reason instanceof ModelError ? reason : reason instanceof Error ? reason.message : String(reason)));
  }, [pageId, needsFormFlush, needsParse]);
  useEffect(() => {
    if (!(error instanceof ModelError) || !error.path || pageId === 'advanced') return;
    const rule = /^cache\.rules\.(\d+)\.([a-z_]+)$/.exec(error.path);
    if (rule && cacheTab !== 'rules') { setCacheTab('rules'); return; }
    const id = rule ? `rule-${rule[1]}-${rule[2]}` : `setting-${error.path.replaceAll('.', '-')}`;
    document.getElementById(id)?.focus();
  }, [error, pageId, cacheTab]);
  const t = (key: string) => translate(key, language);
  const errorPage = error instanceof ModelError && error.path
    ? (Object.entries(settingPages).find(([, page]) => page.groups.some((group) => group.fields.some((field) => error.path === field.path || error.path?.startsWith(`${field.path}.`))))?.[0] as SettingPageId | undefined)
      ?? (error.path.startsWith('cache.rules.') ? 'cache' : 'dns')
    : 'dns';
  const intro = pageId === 'advanced' ? t('app.advancedIntro') : t(settingPages[pageId].introKey);
  const title = pageId === 'advanced' ? t('app.advanced') : t(settingPages[pageId].titleKey);
  if (pageId !== 'advanced' && needsParse) return <div className="page-stack"><h1>{title}</h1><p className="notice" role="status">{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate('/advanced')}>{t('app.returnToToml')}</button></div>;
  return <div className="page-stack"><header className="page-head"><p className="eyebrow">{t('ui.configuration')}</p><h1>{title}</h1><p className="muted">{intro}</p></header>
    {error && <div className="notice error" role="alert">{presentIssue(error, language)}<button type="button" onClick={() => setError(null)} aria-label={t('ui.closeDialog')}>×</button></div>}
    {notice && <p className="notice" role="status">{notice}</p>}
    <div className="config-tools"><span>{t('ui.draft')} {draft && <small className="muted">{t('app.revision').replace('{revision}', String(draft.revision))}</small>}</span>
      <div className="button-group"><button type="button" className="button quiet" disabled={!draft || busy} onClick={() => void exportDraft().then(() => setNotice(t('app.exported'))).catch((reason) => setError(String(reason)))}>{t('ui.export')}</button>
        <Button variant="secondary" disabled={!draft || busy} onClick={() => { const source = draft?.original; if (source === undefined) return; void preview().then((result) => { setPreviewed({ source, result }); setNotice(null); }).catch(() => {}); }}>{t('ui.preview')}</Button>
        <button type="button" className="button secondary" disabled={!draft || busy} onClick={() => void validate().then((result) => setNotice(t(result.restart_required ? 'app.validRestart' : 'app.validCache'))).catch(() => {})}>{t('ui.validate')}</button>
        <button type="button" className="button quiet" disabled={!draft?.hasBackup || busy} onClick={() => void confirmAction('app.rollbackHelp', 'ui.rollback').then((accepted) => { if (accepted) void rollback().catch(() => {}); })}>{t('ui.rollback')}</button></div>
    </div>
    {previewed && draft?.original === previewed.source && draft.toml === previewed.result && !needsFormFlush && <section className="panel diff-panel" aria-label={t('app.diffTitle')}>
      <h2>{t('app.diffTitle')}</h2><p className="muted small">{t('app.diffHelp')}</p>
      {diff.some((line) => line.kind !== 'same') ? <pre className="diff-lines"><code>{diff.map((line, index) => <span className={`diff-${line.kind}`} key={index}>{line.kind === 'add' ? '+' : line.kind === 'remove' ? '−' : ' '}{line.text || ' '}</span>)}</code></pre>
        : <p className="muted">{t('app.noChanges')}</p>}
    </section>}
    {pageId === 'advanced' ? needsFormFlush ? <div className="notice"><p>{error ? presentIssue(error, language) : t('app.working')}</p><button type="button" className="button secondary" onClick={() => navigate(`/${errorPage}`)}>{t('app.returnToEdit')}</button></div> : <section className="panel"><label htmlFor="advanced-toml">TOML</label><textarea id="advanced-toml" className="toml-editor" value={draft?.toml ?? ''} disabled={busy || draft?.unknownApply} spellCheck={false} onChange={(event) => setToml(event.target.value)} /></section>
      : pageId === 'cache' ? <>
        <Tabs label={title} items={cacheTabs.map((tab) => ({ value: tab, label: t(`views.cacheTab${tab[0].toUpperCase()}${tab.slice(1)}`) }))} value={cacheTab} onChange={(value) => setCacheTab(value as CacheTab)} />
        {cacheTab === 'usage' && <CacheUsage language={language} />}
        {cacheTab === 'settings' && <GenericSettings pageId="cache" language={language} />}
        {cacheTab === 'rules' && <CacheRules language={language} />}
        {cacheTab === 'inspect' && <CacheInspect language={language} dirty={dirty} />}
      </> : <><GenericSettings pageId={pageId} language={language} />{pageId === 'security' && <CertificateImport language={language} />}</>}
  </div>;
}
