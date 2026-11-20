import { ApiError, type ApiClient } from '../../session/client';
import { ModelError, configList, getPath, type SettingsObject } from '../../model';
import { hasTranslation } from '../../i18n';
import type { SubscriptionSource } from '../../model/subscriptions';
import type { SubscriptionState } from './types';

const POLL_MS = 1000;
const POLL_LIMIT = 600;
const delay = (ms: number) => new Promise<void>((resolve) => { window.setTimeout(resolve, ms); });

export function newSources(settings: SettingsObject, base: SettingsObject): { source: SubscriptionSource; index: number }[] {
  if (getPath(settings, 'filter_subscriptions.enabled') !== true) return [];
  const saved = getPath(base, 'filter_subscriptions.enabled') === true ? configList(base, 'filter_subscriptions.sources') : [];
  return configList(settings, 'filter_subscriptions.sources').map((source, index) => ({ source, index })).filter(({ source }) => source.enabled
    && !saved.some((item) => item.enabled && item.url === source.url && item.format === source.format));
}

function failure(code: string, index: number): ModelError {
  const key = `subscriptions.${code}`;
  const path = code.startsWith('subscription_') ? `filter_subscriptions.sources.${index}.url` : undefined;
  return new ModelError(hasTranslation(key) ? key : 'subscriptions.error', path);
}

export async function downloadNewSources(api: ApiClient, settings: SettingsObject, base: SettingsObject, assertCurrent: () => void): Promise<void> {
  for (const { source, index } of newSources(settings, base)) {
    assertCurrent();
    let operationId: string;
    try {
      ({ operation_id: operationId } = await api.request<{ operation_id: string }>('filter/subscriptions/prepare', 'POST',
        { source: { id: source.id, url: source.url, format: source.format } }));
    } catch (reason) {
      if (reason instanceof ApiError && reason.code === 'subscription_active_source') continue;
      if (reason instanceof ApiError && reason.code.startsWith('subscription_')) throw failure(reason.code, index);
      throw reason;
    }
    for (let attempt = 0; ; attempt += 1) {
      assertCurrent();
      if (attempt >= POLL_LIMIT) throw failure('error', index);
      const state = await api.request<SubscriptionState>('filter/subscriptions');
      assertCurrent();
      const operation = [state.operation, state.recent_operation].find((item) => item?.id === operationId);
      if (!operation) throw failure('error', index);
      if (operation.status === 'succeeded') break;
      if (operation.status !== 'running') throw failure(operation.error?.code ?? 'error', index);
      await delay(POLL_MS);
    }
  }
}
