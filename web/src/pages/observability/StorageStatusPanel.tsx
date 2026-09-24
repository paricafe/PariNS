import { motion, useReducedMotion } from 'motion/react';
import { formatDate, formatNumber, hasTranslation, translate, type Language } from '../../i18n';
import { EASE_OUT } from '../../components/beui/motion';
import type { StorageStatus } from './storage';
import { StorageCoverage } from './StorageCoverage';

export function StorageStatusPanel({ status, language, compact = false }: { status: StorageStatus; language: Language; compact?: boolean }) {
  const reduced = useReducedMotion();
  const t = (key: string) => translate(`storage.${key}`, language);
  const state = status.capacity_pending ? t('pending') : hasTranslation(`storage.${status.health}`) ? t(status.health) : status.health;
  const rows: [string, number, string?][] = compact
    ? [['queue', status.pending_entries], ['lag', status.persist_lag_ms, 'ms'], ['dropped', status.dropped_logs]]
    : [['database', status.database_bytes, 'B'], ['journal', status.journal_bytes, 'B'], ['snapshot', status.cache_snapshot_bytes, 'B'],
      ['logEntries', status.query_log_entries], ['logBytes', status.query_log_bytes, 'B'], ['historySamples', status.history_samples],
      ['queue', status.pending_entries], ['queueBytes', status.pending_bytes, 'B'], ['pendingPoints', status.pending_points],
      ['lag', status.persist_lag_ms, 'ms'], ['dropped', status.dropped_logs], ['droppedPoints', status.dropped_points]];
  return <motion.section className="panel storage-status" aria-label={t('status')} data-health={status.health}
    initial={reduced ? false : { opacity: 0, y: 6 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: reduced ? 0 : 0.16, ease: EASE_OUT }}>
    <div className="heading-row"><h2>{t('status')}</h2><strong role="status">{state}</strong></div>
    {status.error && <p className="notice error" role="alert">{status.error}</p>}
    {status.clock_rollback && <p className="notice">{t('clockRollback')}</p>}
    <dl className="storage-grid">{rows.map(([key, amount, unit]) => <div key={key}><dt>{t(key)}</dt><dd>{formatNumber(amount, language)}{unit ? ` ${unit}` : ''}</dd></div>)}</dl>
    <p className="small muted">{t('lastCommit')}: {status.last_commit_at_ms === null ? t('noCommit') : formatDate(status.last_commit_at_ms, language, { dateStyle: 'short', timeStyle: 'medium' })}</p>
    {!compact && <><p className="small muted">{translate('storage.revisions', language, { configured: status.configured_revision, applied: status.applied_revision })}</p><p className="small muted">{t('budgetHelp')}</p></>}
    <StorageCoverage status={status} language={language} compact={compact} />
  </motion.section>;
}
