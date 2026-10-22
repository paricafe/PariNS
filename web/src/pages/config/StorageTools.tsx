import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, StaleRequest } from '../../session/client';
import { useSession } from '../../session/context';
import { useConfig } from '../../config/context';
import { useConfirm } from '../../components/ConfirmProvider';
import { useBanner } from '../../components/Banner';
import { Button } from '../../components/beui';
import { useAutoConfirm } from '../../components/useAutoConfirm';
import { formatDate, presentIssue, translate, type Language } from '../../i18n';
import { StorageStatusPanel } from '../observability/StorageStatusPanel';
import { isUnknownStorageMutation, storageActions, type StorageAction } from '../observability/storage';
import type { StatsView, StatusView } from '../observability/metrics';

export function StorageTools({ language }: { language: Language }) {
  const { api } = useSession();
  const { locked, busy: configBusy } = useConfig();
  const confirm = useConfirm();
  const banner = useBanner();
  const [view, setView] = useState<StatusView | null>(null);
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
    const current = await api.request<StatusView>('status');
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
    if (!await confirm(t(spec.confirm), `storage.${spec.label}`, trigger)) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.request(spec.path, 'POST', { [spec.epoch]: epoch });
      setNotice('actionDone'); banner(t('actionDone'));
      void load(action === 'totals').catch(fail);
    } catch (reason) {
      if (!(reason instanceof StaleRequest)) {
        if (isUnknownStorageMutation(reason)) setUnknown({ action, epoch });
        else fail(reason);
      }
    } finally { setBusy(false); }
  };
  // A lost response is settled by the epoch on later status reads; the action is never sent again.
  const confirmation = useAutoConfirm(unknown !== null, async () => {
    if (!unknown) return undefined;
    const current = await load(unknown.action === 'totals');
    return current.storage[storageActions[unknown.action].epoch] > unknown.epoch ? true : undefined;
  }, () => { setUnknown(null); setNotice('actionDone'); banner(t('actionDone')); });
  return <div className="settings-stack">
    {view && <StorageStatusPanel status={view.storage} language={language} />}
    <section className="settings-group" aria-labelledby="storage-clear-title"><h2 id="storage-clear-title" className="group-title">{t('clearTitle')}</h2>
      <div className="grouped">{(Object.keys(storageActions) as StorageAction[]).map((action) => <button type="button" key={action} className="row row-action" data-destructive=""
        disabled={!view || busy || Boolean(unknown) || locked || configBusy || view.storage.health === 'unavailable'} onClick={(event) => void act(action, event.currentTarget)}>{t(storageActions[action].label)}</button>)}</div>
      {stats && <p className="group-footer">{translate('storage.totalsSince', language, { time: formatDate(stats.totals.since_ms, language, { dateStyle: 'medium', timeStyle: 'short' }) })}</p>}
      {unknown && <p className="notice" role="status">{confirmation.state === 'failed' ? t('actionUnknown') : confirmation.state === 'unresolved' ? t('stillUnknown') : translate('shell.confirming', language)}</p>}
      {unknown && (confirmation.state === 'failed' || confirmation.state === 'unresolved') && <Button variant="secondary" disabled={busy} onClick={confirmation.check}>{t('checkResult')}</Button>}
      {notice && notice !== 'actionDone' && <p className="notice" role="status">{t(notice)}</p>}
      {error && <p className="notice error" role="alert">{presentIssue(error, language)}</p>}
    </section>
  </div>;
}
