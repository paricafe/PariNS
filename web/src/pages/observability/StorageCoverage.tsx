import { formatDate, formatNumber, translate, type Language } from '../../i18n';
import { diagnosticLabel } from './diagnostics';
import type { StorageStatus } from './storage';

export function StorageCoverage({ status, language, compact }: { status: StorageStatus; language: Language; compact: boolean }) {
  const t = (key: string) => translate(`reliability.${key}`, language);
  const date = (value: number) => formatDate(value, language, { dateStyle: 'short', timeStyle: 'medium' });
  const coverage = status.query_log_coverage;
  const disk = status.filesystem;
  const bytes = (value: number | null) => value === null ? '—' : `${formatNumber(value, language)} B`;
  return <>
    <h3>{t('coverage')}</h3>
    <p>{coverage.earliest_time_ms === null || coverage.latest_time_ms === null || coverage.span_ms === null ? t('coverageEmpty') : translate('reliability.coverageRange', language, { from: date(coverage.earliest_time_ms), to: date(coverage.latest_time_ms), seconds: formatNumber(coverage.span_ms / 1000, language) })}</p>
    <p className="small muted">{t('coverageHelp')}</p>
    {disk.low_space && <p className="notice error" role="alert">{t('lowSpace')}</p>}
    {disk.stale && <p className="notice">{t('staleFilesystem')}</p>}
    {!compact && <>
      <details><summary>{t('cleanup')}</summary>
        {status.cleanup.last_reason && status.cleanup.last_at_ms !== null && <p>{translate('reliability.lastCleanup', language, { reason: diagnosticLabel(`cleanup_${status.cleanup.last_reason}`, language), time: date(status.cleanup.last_at_ms) })}</p>}
        <dl className="diagnostic-counts">{Object.entries(status.cleanup.removed).map(([reason, count]) => <div key={reason}><dt>{diagnosticLabel(`cleanup_${reason}`, language)}</dt><dd>{formatNumber(count, language)}</dd></div>)}</dl>
      </details>
      <details><summary>{t('drops')}</summary><dl className="diagnostic-counts">{Object.entries(status.dropped_by_reason).map(([reason, count]) => <div key={reason}><dt>{diagnosticLabel(`drop_${reason}`, language)}</dt><dd>{formatNumber(count, language)}</dd></div>)}</dl></details>
      <h3>{t('filesystem')}</h3>
      {disk.error && <p className="notice">{t('filesystemUnavailable')} · <code>{disk.error}</code></p>}
      <dl className="storage-grid"><div><dt>{t('available')}</dt><dd>{bytes(disk.available_bytes)}</dd></div><div><dt>{t('total')}</dt><dd>{bytes(disk.total_bytes)}</dd></div><div><dt>{t('threshold')}</dt><dd>{bytes(disk.warning_threshold_bytes)}</dd></div></dl>
      {disk.sampled_at_ms !== null && <p className="small muted">{translate('reliability.sampled', language, { time: date(disk.sampled_at_ms) })}</p>}
      <p className="small muted">{t('filesystemHelp')}</p>
    </>}
  </>;
}
