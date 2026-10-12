import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, StaleRequest } from '../../session/client';
import { useSession } from '../../session/context';
import { useConfig } from '../../config/context';
import { useConfirm } from '../../components/ConfirmProvider';
import { useBanner } from '../../components/Banner';
import { Button } from '../../components/beui';
import { formatDate, presentIssue, translate, type Language } from '../../i18n';
import { StorageStatusPanel } from '../observability/StorageStatusPanel';
import { isUnknownStorageMutation, storageActions, type StorageAction, type StorageStatus } from '../observability/storage';
import type { StatsView } from '../observability/metrics';

export function StorageTools({ language }: { language: Language }) {
  const { api } = useSession();
  const { locked, busy: configBusy } = useConfig();
  const confirm = useConfirm();
  const banner = useBanner();
  const [view, setView] = useState<{ revision: number; storage: StorageStatus } | null>(null);
  const [stats, setStats] = useState<StatsView | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | ApiError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [unknown, setUnknown] = useState<{ action: StorageAction; epoch: number } | null>(null);
  const requestId = useRef(0);
  const t = (key: string) => translate(`storage.${key}`, language);
  const fail = (reason: unknown) => { if (!(reason instanceof StaleRequest)) setError(reason instanceof ApiError ? reason : String(reason)); };
  // Statistics totals change only through the totals reset, so they are read on mount and after that reset rather than polled.
  const load = useCallback(async (includeStats: boolean) => {
    const owner = ++requestId.current;
    const current = await api.request<{ revision: number; storage: StorageStatus }>('status');
    if (owner !== requestId.current) throw new StaleRequest();
    setView(current);
    if (!includeStats) return current;
    try {
      const totals = await api.request<StatsView>('stats', 'GET', undefined, undefined, { query: { range: '1h' } });
      if (owner === requestId.current) { setStats(totals); setError(null); }
    } catch (reason) { if (owner === requestId.current && reason instanceof ApiError) setError(reason); }
    return current;
  }, [api]);
  useEffect(() => {
    void load(true).catch(fail);
    return () => { requestId.current += 1; };
  }, [load]);
  const act = async (action: StorageAction, trigger: HTMLElement) => {
    if (!view || busy || unknown || locked || configBusy) return;
    const spec = storageActions[action];
    const epoch = view.storage[spec.epoch];
    const revision = view.revision;
    if (!await confirm(`storage.${spec.confirm}`, `storage.${spec.label}`, trigger)) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.request(spec.path, 'POST', { revision, [spec.epoch]: epoch });
      setNotice('actionDone'); banner(t('actionDone'));
      void load(action === 'totals').catch(fail);
    } catch (reason) {
      if (!(reason instanceof StaleRequest)) {
        if (isUnknownStorageMutation(reason)) { setUnknown({ action, epoch }); setNotice('actionUnknown'); }
        else fail(reason);
      }
    } finally { setBusy(false); }
  };
  const reconcile = async () => {
    if (!unknown) return;
    setBusy(true);
    try {
      const current = await load(unknown.action === 'totals');
      if (current.storage[storageActions[unknown.action].epoch] > unknown.epoch) { setUnknown(null); setNotice('actionDone'); banner(t('actionDone')); }
      else setNotice('stillUnknown');
    } catch (reason) { fail(reason); }
    finally { setBusy(false); }
  };
  return <div className="settings-stack">
    {view && <StorageStatusPanel status={view.storage} language={language} />}
    <section className="settings-group" aria-labelledby="storage-clear-title"><h2 id="storage-clear-title" className="group-title">{t('clearTitle')}</h2>
      <div className="grouped">{(Object.keys(storageActions) as StorageAction[]).map((action) => <button type="button" key={action} className="row row-action" data-destructive=""
        disabled={!view || busy || Boolean(unknown) || locked || configBusy || view.storage.health === 'unavailable'} onClick={(event) => void act(action, event.currentTarget)}>{t(storageActions[action].label)}</button>)}</div>
      {stats && <p className="group-footer">{translate('storage.totalsSince', language, { time: formatDate(stats.totals.since_ms, language, { dateStyle: 'medium', timeStyle: 'short' }) })}</p>}
      {unknown && <Button variant="secondary" disabled={busy} onClick={() => void reconcile()}>{t('checkResult')}</Button>}
      {notice && notice !== 'actionDone' && <p className="notice" role="status">{t(notice)}</p>}
      {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    </section>
  </div>;
}
