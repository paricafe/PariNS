import { ApiError, type ApiClient } from '../../session/client';
import { ModelError, configList, getPath, type SettingsObject } from '../../model';
import { hasTranslation } from '../../i18n';
import type { SubscriptionSource } from '../../model/subscriptions';
import type { SubscriptionState } from './types';

const POLL_MS = 1000;
const POLL_LIMIT = 600;
const delay = (ms: number) => new Promise<void>((resolve) => { window.setTimeout(resolve, ms); });

/**
 * Enabled sources whose list the saved configuration does not use yet. Rust accepts a
 * configuration only once their rules are downloaded and checked.
 */
export function newSources(settings: SettingsObject, saved: readonly SubscriptionSource[]): { source: SubscriptionSource; index: number }[] {
  if (getPath(settings, 'filter_subscriptions.enabled') !== true) return [];
  return configList(settings, 'filter_subscriptions.sources').map((source, index) => ({ source, index })).filter(({ source }) => source.enabled
    && !saved.some((item) => item.url === source.url && item.format === source.format));
}

function failure(code: string, index: number): ModelError {
  return new ModelError(hasTranslation(`subscriptions.${code}`) ? `subscriptions.${code}` : 'subscriptions.error', `filter_subscriptions.sources.${index}.url`);
}

/**
 * Downloads and checks each new source through the existing prepare operation, one at a
 * time, and waits for its result. A request is sent once; an unconfirmed one is reported,
 * never repeated.
 */
export async function downloadNewSources(api: ApiClient, settings: SettingsObject, saved: readonly SubscriptionSource[], wait = delay): Promise<void> {
  for (const { source, index } of newSources(settings, saved)) {
    let operationId: string;
    try {
      ({ operation_id: operationId } = await api.request<{ operation_id: string }>('filter/subscriptions/prepare', 'POST',
        { source: { id: source.id, url: source.url, format: source.format } }));
    } catch (reason) {
      // The same list is already in use under another source; its rules are present.
      if (reason instanceof ApiError && reason.code === 'subscription_active_source') continue;
      if (reason instanceof ApiError && !['NETWORK', 'BAD_RESPONSE'].includes(reason.code)) throw failure(reason.code, index);
      throw reason;
    }
    for (let attempt = 0; ; attempt += 1) {
      if (attempt >= POLL_LIMIT) throw failure('error', index);
      const state = await api.request<SubscriptionState>('filter/subscriptions');
      const operation = [state.operation, state.recent_operation].find((item) => item?.id === operationId);
      if (!operation) throw failure('error', index);
      if (operation.status === 'succeeded') break;
      if (operation.status !== 'running') throw failure(operation.error?.code ?? 'error', index);
      await wait(POLL_MS);
    }
  }
}
