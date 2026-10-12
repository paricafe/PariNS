import { formatBytes, formatDate, formatNumber, hasTranslation, translate, type Language } from '../../i18n';
import { ExpandRow } from '../../components/ExpandRow';
import { diagnosticLabel } from './diagnostics';
import type { StorageStatus } from './storage';

/**
 * Key storage facts first (space used, disk free, record range, losses);
 * every raw counter stays available under “Detailed counts”.
 */
export function StorageStatusPanel({ status, language, compact = false }: { status: StorageStatus; language: Language; compact?: boolean }) {
  const t = (key: string) => translate(`storage.${key}`, language);
  const r = (key: string) => translate(`reliability.${key}`, language);
  const date = (value: number) => formatDate(value, language, { dateStyle: 'short', timeStyle: 'short' });
  const state = status.capacity_pending ? t('pending') : hasTranslation(`storage.${status.health}`) ? t(status.health) : status.health;
  const coverage = status.query_log_coverage;
  const disk = status.filesystem;
  const used = status.database_bytes + status.journal_bytes + status.cache_snapshot_bytes;
  const dropped = status.dropped_logs + status.dropped_points;
  const bytes = (value: number | null) => value === null ? '—' : formatBytes(value, language);
  const counts: [string, string][] = [
    [t('database'), bytes(status.database_bytes)], [t('journal'), bytes(status.journal_bytes)], [t('snapshot'), bytes(status.cache_snapshot_bytes)],
    [t('logEntries'), formatNumber(status.query_log_entries, language)], [t('logBytes'), bytes(status.query_log_bytes)], [t('historySamples'), formatNumber(status.history_samples, language)],
    [t('queue'), formatNumber(status.pending_entries, language)], [t('queueBytes'), bytes(status.pending_bytes)], [t('pendingPoints'), formatNumber(status.pending_points, language)],
    [t('lag'), `${formatNumber(status.persist_lag_ms, language)} ms`], [t('dropped'), formatNumber(status.dropped_logs, language)], [t('droppedPoints'), formatNumber(status.dropped_points, language)],
    [t('lastCommit'), status.last_commit_at_ms === null ? t('noCommit') : date(status.last_commit_at_ms)],
    [r('threshold'), bytes(disk.warning_threshold_bytes)],
    ...Object.entries(status.cleanup.removed).map(([reason, count]) => [diagnosticLabel(`cleanup_${reason}`, language), formatNumber(count, language)] as [string, string]),
    ...Object.entries(status.dropped_by_reason).map(([reason, count]) => [diagnosticLabel(`drop_${reason}`, language), formatNumber(count, language)] as [string, string]),
  ];
  return <section className="settings-group storage-status" aria-labelledby="storage-status-title" data-health={status.health}>
    <h2 id="storage-status-title" className="group-title">{t('status')}</h2>
    <div className="grouped">
      <div className="row" data-layout="inline"><div className="row-label"><span>{t('health')}</span></div><div className="row-control"><strong role="status" className="health-badge">{state}</strong></div></div>
      <div className="row" data-layout="inline"><div className="row-label"><span>{t('usedSpace')}</span></div><div className="row-control"><span className="row-value num">{bytes(used)}</span></div></div>
      <div className="row" data-layout="inline"><div className="row-label"><span>{r('available')}</span></div><div className="row-control"><span className="row-value num">
        {disk.available_bytes === null ? '—' : disk.total_bytes === null ? bytes(disk.available_bytes) : translate('storage.diskFree', language, { free: bytes(disk.available_bytes), total: bytes(disk.total_bytes) })}</span></div></div>
      <div className="row" data-layout="inline"><div className="row-label"><span>{r('coverage')}</span></div><div className="row-control"><span className="row-value">
        {coverage.earliest_time_ms === null || coverage.latest_time_ms === null ? r('coverageEmpty') : `${date(coverage.earliest_time_ms)} – ${date(coverage.latest_time_ms)}`}</span></div></div>
      <div className="row" data-layout="inline"><div className="row-label"><span>{t('losses')}</span></div><div className="row-control"><span className="row-value" data-tone={dropped ? 'warn' : undefined}>
        {dropped ? translate('storage.lossesSome', language, { count: formatNumber(dropped, language) }) : t('lossesNone')}</span></div></div>
      {!compact && <ExpandRow label={t('detailedCounts')}>
        {counts.map(([label, value]) => <div className="row" data-layout="inline" key={label}><div className="row-label"><span>{label}</span></div><div className="row-control"><span className="row-value num">{value}</span></div></div>)}
        {status.cleanup.last_reason && status.cleanup.last_at_ms !== null && <p className="row-note">{translate('reliability.lastCleanup', language, { reason: diagnosticLabel(`cleanup_${status.cleanup.last_reason}`, language), time: date(status.cleanup.last_at_ms) })}</p>}
        {disk.sampled_at_ms !== null && <p className="row-note">{translate('reliability.sampled', language, { time: date(disk.sampled_at_ms) })}</p>}
      </ExpandRow>}
    </div>
    {status.error && <p className="notice error" role="alert">{status.error}</p>}
    {status.clock_rollback && <p className="notice">{t('clockRollback')}</p>}
    {disk.low_space && <p className="notice error" role="alert">{r('lowSpace')}</p>}
    {disk.stale && <p className="notice">{r('staleFilesystem')}</p>}
    {disk.error && <p className="notice">{r('filesystemUnavailable')} · <code>{disk.error}</code></p>}
    {status.configured_revision !== status.applied_revision && <p className="group-footer">{translate('storage.revisions', language, { configured: status.configured_revision, applied: status.applied_revision })}</p>}
  </section>;
}
