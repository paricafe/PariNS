import { useState } from 'react';
import { Link } from 'react-router-dom';
import { Button, Drawer } from '../../components/beui';
import { useConfirm } from '../../components/ConfirmProvider';
import { useConfig } from '../../config/context';
import { formatNumber, hasTranslation, translate, type Language } from '../../i18n';
import { useUpdates } from './context';
import { canApply, type UpdateOperation } from './types';
import './updates.css';

function time(value: string | number | null | undefined, language: Language): string {
  if (!value || !Number.isFinite(new Date(value).getTime())) return translate('updates.none', language);
  return new Intl.DateTimeFormat(language, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(value));
}
function reasonText(reason: string, language: Language): string {
  const key = `updates.${reason}`;
  return hasTranslation(key) ? translate(key, language) : translate('updates.reasonUnknown', language, { code: reason });
}
function phaseText(phase: string, language: Language) { return reasonText(phase === 'failed' ? 'operationFailed' : phase, language); }

function Operation({ operation, language }: { operation: UpdateOperation; language: Language }) {
  const t = (key: string) => translate(`updates.${key}`, language);
  const total = operation.total_bytes;
  const knownTotal = typeof total === 'number' && Number.isFinite(total) && total > 0;
  return <div className="update-operation" aria-label={t('progress')}>
    <p role="status"><strong>{phaseText(operation.phase, language)}</strong> · {operation.version}</p>
    {operation.phase === 'downloading' && <>
      {knownTotal && <progress aria-label={t('progress')} max={total} value={Math.max(0, Math.min(operation.downloaded_bytes, total))} />}
      <p className="small muted">{translate('updates.downloaded', language, { bytes: `${formatNumber(operation.downloaded_bytes, language)} B` })}{knownTotal ? ` / ${formatNumber(total, language)} B` : ''}</p>
    </>}
    {operation.reason && <p className="small">{reasonText(operation.reason, language)}</p>}
  </div>;
}

export function UpdateHint({ language }: { language: Language }) {
  const { view } = useUpdates();
  if (!view?.candidate) return null;
  return <aside className="update-hint"><span>{translate('updates.newVersion', language, { version: view.candidate.version })}</span>
    <Link to="/runtime">{translate('updates.open', language)}</Link></aside>;
}

export function UpdatePanel({ language }: { language: Language }) {
  const updates = useUpdates();
  const config = useConfig();
  const confirm = useConfirm();
  const [draftPrompt, setDraftPrompt] = useState(false);
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
      await updates.apply(candidate, revision);
    }
  };
  const savedAutoCheck = Boolean(config.draft?.settings.updates && (config.draft.settings.updates as { auto_check?: boolean }).auto_check);
  return <section className="panel update-panel" aria-labelledby="software-updates-title">
    <div className="group-head"><div><h2 id="software-updates-title">{t('title')}</h2><p className="muted small">{t('intro')}</p></div></div>
    {!view ? <p role="status">{updates.readFailed ? t('readFailed') : translate('app.connecting', language)}</p> : <>
      <dl className="update-facts">
        <div><dt>{t('current')}</dt><dd>{view.current.version}</dd></div>
        <div><dt>{t('target')}</dt><dd>{view.current.target}</dd></div>
        {candidate && <><div><dt>{t('available')}</dt><dd>{candidate.version}</dd></div><div><dt>{t('published')}</dt><dd>{time(candidate.published_at, language)}</dd></div></>}
        <div><dt>{t('lastCheck')}</dt><dd>{time(view.check.last_check_at_ms, language)}</dd></div>
        <div><dt>{t('nextCheck')}</dt><dd>{savedAutoCheck ? time(view.check.next_check_at_ms, language) : t('autoOff')}</dd></div>
      </dl>
      <p role="status" className={view.check.state === 'failed' ? 'error' : 'muted'}>{view.check.state === 'available' ? translate('updates.newVersion', language, { version: candidate?.version ?? '—' }) : t(view.check.state)}</p>
      {view.check.error && <p className="small error">{reasonText(view.check.error, language)}</p>}
      {view.check.state === 'failed' && view.check.last_success_at_ms && <p className="small muted">{t('lastSuccess')}: {time(view.check.last_success_at_ms, language)}</p>}
      {retry > Date.now() && <p className="small muted">{translate('updates.retryAt', language, { time: time(retry, language) })}</p>}
      {(!view.capability.available || candidate?.manual_reason) && <div className="notice"><strong>{t('manual')}</strong>
        <p className="small">{reasonText(candidate?.manual_reason ?? view.capability.reason ?? 'unsupported_installation', language)}</p><p className="small">{t('manualHelp')}</p></div>}
      {candidate && <details className="update-notes"><summary>{t('notes')}</summary><pre>{candidate.notes ? candidate.notes.slice(0, 32_768) : t('noNotes')}</pre>
        {/^v\d+\.\d+\.\d+$/.test(candidate.tag) && <a href={`https://github.com/paricafe/PariNS/releases/tag/${candidate.tag}`} target="_blank" rel="noopener noreferrer">{t('officialPage')}</a>}</details>}
    </>}
    {updates.error && <p className="error" role="alert">{reasonText(updates.error, language)}</p>}
    {updates.disconnected ? <p className="notice" role="status">{t('reconnecting')}</p> : updates.readFailed && view && <p className="notice" role="status">{t('readFailed')}</p>}
    {updates.unknown && <p className="notice" role="status">{t('unknown')}</p>}
    {installBusy && <p className="muted small">{t('locked')}</p>}
    {view?.active_operation ? <Operation operation={view.active_operation} language={language} /> : updates.pending ? <p role="status">{t('accepted')}</p> : view?.last_operation && <div><h3>{t('lastOperation')}</h3><Operation operation={view.last_operation} language={language} /></div>}
    <div className="button-group update-actions">
      <Button variant="secondary" disabled={Boolean(updates.action || installBusy || view?.check.state === 'checking' || retry > Date.now())} onClick={() => void updates.check()}>{t('checkNow')}</Button>
      <Button variant="primary" disabled={!canApply(view) || Boolean(updates.action) || installBusy || !config.draft || config.busy || config.locked} onClick={(event) => void begin(event.currentTarget)}>{t('updateNow')}</Button>
      <button type="button" className="button quiet" onClick={() => void updates.refresh()}>{t('checkResult')}</button>
    </div>
    <Drawer open={draftPrompt} title={t('drafts')} closeLabel={translate('ui.closeDialog', language)} onClose={() => setDraftPrompt(false)}>
      <p>{t('drafts')}</p>{draftError && <p role="alert" className="error">{t('readFailed')}</p>}
      <div className="button-group"><Button variant="secondary" onClick={() => { setDraftPrompt(false); window.setTimeout(() => document.querySelector<HTMLElement>('.save-bar button.primary')?.focus(), 0); }}>{t('keepDraft')}</Button>
        <Button variant="primary" disabled={config.busy} onClick={() => { setDraftError(false); void config.reload().then(() => setDraftPrompt(false)).catch(() => setDraftError(true)); }}>{t('discardDraft')}</Button></div>
    </Drawer>
  </section>;
}
