import { useRef, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { Button, Drawer } from '../../components/beui';
import { useConfirm } from '../../components/ConfirmProvider';
import { useConfig } from '../../config/context';
import { formatBytes, hasTranslation, translate, type Language } from '../../i18n';
import { InfoTip } from '../../components/InfoTip';
import { ExpandRow } from '../../components/ExpandRow';
import { useUpdates } from './context';
import { canApply, type UpdateOperation } from './types';
import './updates.css';

function time(value: string | number | null | undefined, language: Language): string {
  if (!value || !Number.isFinite(new Date(value).getTime())) return translate('updates.none', language);
  return new Intl.DateTimeFormat(language, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(value));
}
const RECOVERY = ['manual_recovery_required', 'installation_pending'];
/** Release target triple as a readable platform; the raw triple stays behind ⓘ. */
function platformName(target: string, language: Language): string {
  const os = target.includes('apple-darwin') ? 'macOS' : target.includes('linux') ? 'Linux' : target.includes('windows') ? 'Windows' : null;
  const arch = target.startsWith('aarch64') ? os === 'macOS' ? translate('updates.appleSilicon', language) : 'ARM64' : target.startsWith('x86_64') ? os === 'macOS' ? 'Intel' : 'x86_64' : null;
  return os && arch ? `${os} · ${arch}` : target;
}
function reasonText(reason: string, language: Language): string {
  const key = `updates.${reason}`;
  return hasTranslation(key) ? translate(key, language) : translate('updates.reasonUnknown', language, { code: reason });
}
function phaseText(phase: string, language: Language) { return reasonText(phase === 'failed' ? 'operationFailed' : phase === 'aborted' ? 'operationAborted' : phase, language); }

function Operation({ operation, language }: { operation: UpdateOperation; language: Language }) {
  const t = (key: string) => translate(`updates.${key}`, language);
  const total = operation.total_bytes;
  const knownTotal = typeof total === 'number' && Number.isFinite(total) && total > 0;
  return <div className="update-operation" aria-label={t('progress')}>
    <p role="status"><strong>{phaseText(operation.phase, language)}</strong> · {operation.version}</p>
    {operation.phase === 'downloading' && <>
      {knownTotal && <progress aria-label={t('progress')} max={total} value={Math.max(0, Math.min(operation.downloaded_bytes, total))} />}
      <p className="small muted">{translate('updates.downloaded', language, { bytes: formatBytes(operation.downloaded_bytes, language) })}{knownTotal ? ` / ${formatBytes(total, language)}` : ''}</p>
    </>}
    {operation.reason && <p className="small">{reasonText(operation.reason, language)}</p>}
  </div>;
}

export function UpdateHint({ language }: { language: Language }) {
  const { view } = useUpdates();
  if (!view?.candidate) return null;
  return <aside className="update-hint"><span>{translate('updates.newVersion', language, { version: view.candidate.version })}</span>
    <Link to="/updates">{translate('updates.open', language)}</Link></aside>;
}

export function UpdatePanel({ language }: { language: Language }) {
  const updates = useUpdates();
  const config = useConfig();
  const confirm = useConfirm();
  const [draftPrompt, setDraftPrompt] = useState(false);
  // Actions are shown only while they can run, so the pressed button disappears; keep focus on the status line.
  const statusRow = useRef<HTMLSpanElement>(null);
  const holdFocus = () => statusRow.current?.focus({ preventScroll: true });
  const [draftError, setDraftError] = useState(false);
  const t = (key: string) => translate(`updates.${key}`, language);
  const view = updates.view;
  const candidate = view?.candidate;
  const retry = view?.check.retry_at_ms ?? 0;
  const installBusy = Boolean(updates.pending || view?.active_operation || view?.frozen);
  const begin = async (trigger: HTMLElement) => {
    if (config.dirty || config.draft?.unknownApply) { setDraftPrompt(true); return; }
    if (!candidate || !config.draft || config.busy || config.locked) return;
    const revision = config.draft.revision;
    if (await confirm(translate('updates.confirm', language, { current: view.current.version, next: candidate.version }), 'updates.updateNow', trigger)) {
      holdFocus();
      await updates.apply(candidate, revision);
    }
  };
  // Recovery states need the installer, not a different release.
  const recovery = view && RECOVERY.includes(view.capability.reason ?? '') ? view.capability.reason : null;
  const savedAutoCheck = Boolean(config.draft?.settings.updates && (config.draft.settings.updates as { auto_check?: boolean }).auto_check);
  const checkable = Boolean(view && !updates.action && !installBusy && view.check.state !== 'checking' && retry <= Date.now());
  const applicable = canApply(view) && !updates.action && !installBusy && Boolean(config.draft) && !config.busy && !config.locked;
  const statusText = !view ? '' : view.check.state === 'available' ? translate('updates.newVersion', language, { version: candidate?.version ?? '—' }) : t(view.check.state);
  const row = (label: string, value: ReactNode) => <div className="row" data-layout="inline"><div className="row-label"><span>{label}</span></div><div className="row-control"><span className="row-value">{value}</span></div></div>;
  return <section className="settings-group update-panel" aria-labelledby="software-updates-title">
    <h2 id="software-updates-title" className="group-title">{t('versionGroup')}</h2>
    {!view ? <div className="grouped"><div className="row"><span role="status">{updates.readFailed ? t('readFailed') : translate('app.connecting', language)}</span></div></div> : <div className="grouped">
      {row(t('current'), <>{view.current.version} <span className="muted">· {platformName(view.current.target, language)}</span> <InfoTip text={view.current.target} label={t('target')} /></>)}
      <div className="row" data-layout="inline"><div className="row-label"><span ref={statusRow} tabIndex={-1} role="status" className={view.check.state === 'failed' ? 'error' : undefined}>{statusText}</span></div>
        {view.check.last_check_at_ms ? <div className="row-control"><span className="row-value muted small">{t('lastCheck')} {time(view.check.last_check_at_ms, language)}</span></div> : null}</div>
      {view.check.error && <div className="row"><p className="row-note error">{reasonText(view.check.error, language)}</p></div>}
      {view.check.state === 'failed' && view.check.last_success_at_ms && row(t('lastSuccess'), time(view.check.last_success_at_ms, language))}
      {candidate && row(t('published'), time(candidate.published_at, language))}
      {savedAutoCheck && view.check.next_check_at_ms ? row(t('nextCheck'), time(view.check.next_check_at_ms, language)) : null}
      {candidate && <ExpandRow label={t('notes')}><div className="row update-notes"><pre>{candidate.notes ? candidate.notes.slice(0, 32_768) : t('noNotes')}</pre>
        {/^v\d+\.\d+\.\d+$/.test(candidate.tag) && <a href={`https://github.com/paricafe/PariNS/releases/tag/${candidate.tag}`} target="_blank" rel="noopener noreferrer">{t('officialPage')}</a>}</div></ExpandRow>}
    </div>}
    {view && (!view.capability.available || candidate?.manual_reason) && <p className="group-footer notice-footer">{reasonText(recovery ?? candidate?.manual_reason ?? view.capability.reason ?? 'unsupported_installation', language)}{!recovery && <> <a href="https://github.com/paricafe/PariNS/blob/main/guide/deployment.md" target="_blank" rel="noopener noreferrer">{t('howToUpdate')}</a></>}</p>}
    {retry > Date.now() && <p className="group-footer">{translate('updates.retryAt', language, { time: time(retry, language) })}</p>}
    {updates.error && <p className="error" role="alert">{reasonText(updates.error, language)}</p>}
    {updates.disconnected ? <p className="notice" role="status">{t('reconnecting')}</p> : updates.readFailed && view && <p className="notice" role="status">{t('readFailed')}</p>}
    {updates.unknown && <p className="notice" role="status">{t('unknown')}</p>}
    {installBusy && <p className="muted small">{t('locked')}</p>}
    {view?.active_operation ? <Operation operation={view.active_operation} language={language} /> : updates.pending ? <p role="status">{t('accepted')}</p> : view?.last_operation && <div><h3>{t('lastOperation')}</h3><Operation operation={view.last_operation} language={language} /></div>}
    {(checkable || applicable || updates.unknown || updates.disconnected || updates.pending || view?.active_operation) && <div className="button-group update-actions">
      {checkable && <Button variant="secondary" onClick={() => { holdFocus(); void updates.check(); }}>{t('checkNow')}</Button>}
      {applicable && <Button variant="primary" onClick={(event) => void begin(event.currentTarget)}>{t('updateNow')}</Button>}
      {(updates.unknown || updates.disconnected || updates.pending || view?.active_operation) && <button type="button" className="button quiet" onClick={() => void updates.refresh()}>{t('checkResult')}</button>}
    </div>}
    <Drawer open={draftPrompt} variant="sheet" title={t('drafts')} closeLabel={translate('ui.closeDialog', language)} onClose={() => setDraftPrompt(false)}>
      <p>{t('drafts')}</p>{draftError && <p role="alert" className="error">{t('readFailed')}</p>}
      <div className="button-group"><Button variant="secondary" onClick={() => { setDraftPrompt(false); window.setTimeout(() => document.querySelector<HTMLElement>('.save-bar button.primary')?.focus(), 0); }}>{t('keepDraft')}</Button>
        <Button variant="primary" disabled={config.busy} onClick={() => { setDraftError(false); void config.reload().then(() => setDraftPrompt(false)).catch(() => setDraftError(true)); }}>{t('discardDraft')}</Button></div>
    </Drawer>
  </section>;
}
