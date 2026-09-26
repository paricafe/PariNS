import { useEffect, useRef, useState } from 'react';
import type { ApiClient } from '../../session/client';
import { ApiError, StaleRequest } from '../../session/client';
import { translate, type Language } from '../../i18n';
import type { CheckResult, Failure } from './types';
import { failureText } from './SourceCard';

export function DomainCheck({ api, language, names }: { api: ApiClient; language: Language; names: Record<string, string> }) {
  const [name, setName] = useState('');
  const [result, setResult] = useState<CheckResult | null>(null);
  const [error, setError] = useState<Failure | null>(null);
  const [working, setWorking] = useState(false);
  const owner = useRef(0);
  const edit = useRef(0);
  useEffect(() => { owner.current += 1; return () => { owner.current += 1; }; }, [api]);
  const t = (key: string) => translate(`subscriptions.${key}`, language);
  return <section className="panel"><h2>{t('checkTitle')}</h2><p className="muted small">{t('checkHelp')}</p>
    <form className="fields-grid" onSubmit={async (event) => {
      event.preventDefault(); if (working) return;
      const token = owner.current; const input = edit.current; setWorking(true); setError(null); setResult(null);
      try { const checked = await api.request<CheckResult>('filter/check', 'POST', { name: name.trim() }); if (token === owner.current && input === edit.current) setResult(checked); }
      catch (reason) { if (token === owner.current && !(reason instanceof StaleRequest)) setError({ code: reason instanceof ApiError ? reason.code : 'error', line: null }); }
      finally { if (token === owner.current) setWorking(false); }
    }}><label className="field">{t('domain')}<input value={name} required spellCheck={false} onChange={(event) => { edit.current += 1; setName(event.target.value); setResult(null); }} /></label>
      <button type="submit" className="button secondary" disabled={working}>{t(working ? 'running' : 'check')}</button></form>
    {error && <p role="alert" className="error">{failureText(error, language)}</p>}
    {result && <div className="inspection-result" role="status"><h3>{t(result.decision)}</h3><p className="muted small">{t('generation')}: {result.generation}</p>
      {result.witness && <p>{t('witness')}: <code>{result.witness.rule}</code> · {t(result.witness.scope)} · {result.witness.source_id === null ? t('local') : names[result.witness.source_id] || result.witness.source_id}</p>}
    </div>}
  </section>;
}
