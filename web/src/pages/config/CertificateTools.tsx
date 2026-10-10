import { useEffect, useRef, useState } from 'react';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import { Button } from '../../components/beui';
import { formatDate, presentIssue, translate, type Language } from '../../i18n';

export interface CertificateStatus {
  certificate_generation: number;
  roles: { role: 'dot' | 'doh' | 'doq'; leaf_sha256: string; not_before_ms: number; not_after_ms: number }[];
  last_reload: null | { attempt_id: number; source: 'api' | 'signal'; started_at_ms: number; completed_at_ms: number | null; outcome: 'applied' | 'unchanged' | 'failed'; error_code: string | null };
}
export interface CertificateView { revision: number; certificates: CertificateStatus }
interface ReloadResult { outcome: 'applied' | 'unchanged'; revision: number; certificate_generation: number; certificates: CertificateStatus }

interface CertificateToolsProps {
  language: Language;
  view: CertificateView | null;
  readFailure: ApiError | string | null;
  read(): Promise<CertificateView>;
  hold(active: boolean): void;
  publish(view: CertificateView): void;
}

export function CertificateTools({ language, view, readFailure, read, hold, publish }: CertificateToolsProps) {
  const { api } = useSession();
  const { dirty, busy: configBusy, locked } = useConfig();
  const [busy, setBusy] = useState(false);
  const [unknown, setUnknown] = useState(false);
  const [checked, setChecked] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setError] = useState<ApiError | string | null>(null);
  const error = actionError ?? readFailure;
  const mounted = useRef(false);
  const mutation = useRef(false);
  const t = (key: string) => translate(`reliability.${key}`, language);
  const date = (time: number) => formatDate(time, language, { dateStyle: 'medium', timeStyle: 'medium' });
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const reload = async () => {
    if (!view || mutation.current || unknown || locked || configBusy) return;
    mutation.current = true; setBusy(true); setError(null); setNotice(null); hold(true);
    try {
      const result = await api.request<ReloadResult>('certificates/reload', 'POST', { revision: view.revision });
      if (mounted.current) { publish({ revision: result.revision, certificates: result.certificates }); setNotice(result.outcome); }
    } catch (reason) {
      if (mounted.current && !(reason instanceof StaleRequest)) {
        if (reason instanceof ApiError && (reason.status === 0 || reason.code === 'BAD_RESPONSE' && reason.status >= 200 && reason.status < 300)) {
          setUnknown(true); setChecked(false); setNotice('unknown');
        } else { setError(reason instanceof ApiError ? reason : String(reason)); setNotice(reason instanceof ApiError && reason.status === 422 ? 'failed' : null); }
      }
    } finally { mutation.current = false; hold(false); if (mounted.current) setBusy(false); }
  };
  const check = async () => {
    if (mutation.current) return;
    mutation.current = true; setBusy(true); setError(null); hold(true);
    try {
      await read();
      if (mounted.current) { setChecked(true); setNotice('concurrentResult'); }
    } catch (reason) { if (mounted.current && !(reason instanceof StaleRequest)) setError(reason instanceof ApiError ? reason : String(reason)); }
    finally { mutation.current = false; hold(false); if (mounted.current) setBusy(false); }
  };
  const certificates = view?.certificates;
  const last = certificates?.last_reload;
  return <section className="panel" aria-label={t('certificates')}>
    <div className="heading-row"><h2>{t('certificates')}</h2><Button variant="secondary" disabled={!view || busy || unknown || locked || configBusy} onClick={() => void reload()}>{t('reload')}</Button></div>
    <p className="small muted">{t('reloadHelp')}</p>
    {dirty && <p className="notice">{t('draftHelp')}</p>}
    {notice && <p className="notice" role="status">{t(notice)}</p>}
    {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    {unknown && <div className="button-group"><Button variant="secondary" disabled={busy} onClick={() => void check()}>{translate('storage.checkResult', language)}</Button>{checked && <Button variant="outline" disabled={busy} onClick={() => { setUnknown(false); setChecked(false); setNotice(null); }}>{t('acknowledge')}</Button>}</div>}
    {!certificates ? <p className="muted">{translate('app.reading', language)}</p> : <>
      <p>{translate('reliability.generation', language, { generation: certificates.certificate_generation })}</p>
      {!certificates.roles.length && <p className="muted">{t('noCertificates')}</p>}
      {certificates.roles.map((role) => <section key={role.role} className="certificate-summary"><h3>{role.role === 'doh' ? 'DoH' : role.role === 'dot' ? 'DoT' : 'DoQ'}</h3>
        {role.role === 'doh' && <p className="small muted">{t('dohRoleHelp')}</p>}
        <p>{translate(Date.now() >= role.not_after_ms ? 'reliability.expired' : 'reliability.expires', language, { time: date(role.not_after_ms) })}</p>
        <p className="small muted">{t('validFrom')}: {date(role.not_before_ms)}</p>
        <dl><dt className="small muted">{t('fingerprint')}</dt><dd className="fingerprint"><code>{role.leaf_sha256}</code></dd></dl>
      </section>)}
      {last && <section><h3>{t('lastReload')}</h3><p>{translate('reliability.attempt', language, { id: last.attempt_id, source: t(`source_${last.source}`) })}</p><p>{t(last.outcome)}{last.error_code && <> · <code>{last.error_code}</code></>}</p><p className="small muted">{translate('reliability.started', language, { time: date(last.started_at_ms) })}{last.completed_at_ms !== null && <> · {translate('reliability.completed', language, { time: date(last.completed_at_ms) })}</>}</p></section>}
    </>}
    <p className="small muted">{t('publicAccess')}</p>
  </section>;
}
