import { useEffect, useRef, useState } from 'react';
import { useConfig } from '../../config/context';
import { useSession } from '../../session/context';
import { ApiError, StaleRequest } from '../../session/client';
import { Button } from '../../components/beui';
import { InfoTip } from '../../components/InfoTip';
import { useAutoConfirm } from '../../components/useAutoConfirm';
import { getPath } from '../../model';
import { formatDate, presentIssue, translate, type Language } from '../../i18n';

export interface CertificateStatus {
  certificate_generation: number;
  roles: { role: 'dot' | 'doh' | 'doq'; leaf_sha256: string; not_before_ms: number; not_after_ms: number }[];
  last_reload: null | { attempt_id: number; source: 'api' | 'signal'; started_at_ms: number; completed_at_ms: number; outcome: 'applied' | 'unchanged' | 'failed'; error_code: string | null };
}
export interface CertificateView { certificates: CertificateStatus }
interface ReloadResult { outcome: 'applied' | 'unchanged'; certificate_generation: number; certificates: CertificateStatus }

interface CertificateToolsProps {
  language: Language;
  view: CertificateView | null;
  readFailure: ApiError | string | null;
  read(): Promise<CertificateView>;
  hold(active: boolean): void;
  publish(view: CertificateView): void;
}

const CERTIFICATE_PATHS = ['dot', 'doh', 'doq'].flatMap((protocol) => [`${protocol}.cert_file`, `${protocol}.key_file`]);

export function CertificateTools({ language, view, readFailure, read, hold, publish }: CertificateToolsProps) {
  const { api } = useSession();
  const { draft, busy: configBusy, locked } = useConfig();
  const [busy, setBusy] = useState(false);
  /** The last reload attempt seen before a reload whose response was lost. */
  const [unknown, setUnknown] = useState<number | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setError] = useState<ApiError | string | null>(null);
  const error = actionError ?? readFailure;
  const mounted = useRef(false);
  const mutation = useRef(false);
  const t = (key: string) => translate(`reliability.${key}`, language);
  const date = (time: number) => formatDate(time, language, { dateStyle: 'medium', timeStyle: 'medium' });
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  // A reload holds the Manager lock, so a status read that follows a lost response shows a newer
  // attempt if the reload ran; the reload itself is never sent again.
  const confirmation = useAutoConfirm(unknown !== null, async () => {
    const last = (await read()).certificates.last_reload;
    return last && last.attempt_id > (unknown ?? 0) ? last.outcome : 'notRun';
  }, (outcome) => { setUnknown(null); setNotice(outcome); });
  const reload = async () => {
    if (!view || mutation.current || unknown !== null || locked || configBusy) return;
    const before = view.certificates.last_reload?.attempt_id ?? 0;
    mutation.current = true; setBusy(true); setError(null); setNotice(null); hold(true);
    try {
      const result = await api.request<ReloadResult>('certificates/reload', 'POST', {});
      if (mounted.current) { publish({ certificates: result.certificates }); setNotice(result.outcome); }
    } catch (reason) {
      if (mounted.current && !(reason instanceof StaleRequest)) {
        if (reason instanceof ApiError && (reason.status === 0 || reason.code === 'BAD_RESPONSE' && reason.status >= 200 && reason.status < 300)) setUnknown(before);
        else { setError(reason instanceof ApiError ? reason : String(reason)); setNotice(reason instanceof ApiError && reason.status === 422 ? 'failed' : null); }
      }
    } finally { mutation.current = false; hold(false); if (mounted.current) setBusy(false); }
  };
  // Only edits to certificate paths are affected by reading the saved paths.
  const pathsEdited = Boolean(draft && CERTIFICATE_PATHS.some((path) => (draft.fields[path] ?? getPath(draft.settings, path) ?? '') !== (getPath(draft.base, path) ?? '')));
  const certificates = view?.certificates;
  const last = certificates?.last_reload;
  return <section className="settings-group" aria-labelledby="certificates-title">
    <div className="group-title-row"><h2 id="certificates-title" className="group-title">{t('certificates')}</h2>{Boolean(certificates?.roles.length) && <Button variant="secondary" disabled={!view || busy || unknown !== null || locked || configBusy} onClick={() => void reload()}>{t('reload')}</Button>}</div>
    <div className="grouped padded">{Boolean(certificates?.roles.length) && <p className="small muted">{t('reloadHelp')}</p>}
    {pathsEdited && <p className="notice">{t('draftHelp')}</p>}
    {unknown !== null && <p className="notice" role="status">{confirmation.state === 'failed' ? t('unknown') : translate('shell.confirming', language)}</p>}
    {notice && <p className="notice" role="status">{t(notice)}</p>}
    {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    {unknown !== null && confirmation.state === 'failed' && <div className="button-group"><Button variant="secondary" onClick={confirmation.check}>{translate('storage.checkResult', language)}</Button></div>}
    {!certificates ? <p className="muted">{translate('app.reading', language)}</p> : <>
      {!certificates.roles.length && <p className="muted">{t('noCertificates')}</p>}
      {certificates.roles.map((role) => <section key={role.role} className="certificate-summary"><h3>{role.role === 'doh' ? 'DoH' : role.role === 'dot' ? 'DoT' : 'DoQ'}</h3>
        <p>{translate(Date.now() >= role.not_after_ms ? 'reliability.expired' : 'reliability.expires', language, { time: date(role.not_after_ms) })}
          <InfoTip text={`${t('fingerprint')}: ${role.leaf_sha256}`} label={t('fingerprint')} /></p>
        <p className="small muted">{t('validFrom')}: {date(role.not_before_ms)}</p>
      </section>)}
      {last && <p className="small muted">{translate('reliability.lastReload', language, { time: date(last.completed_at_ms), source: t(`source_${last.source}`), result: t(`result_${last.outcome}`) })}
        {last.error_code && <> · <code>{last.error_code}</code></>}</p>}
    </>}
  </div></section>;
}
