import { convertFieldValue } from './values';

export type SubscriptionFormat = 'domain_list' | 'hosts_blocklist';
export interface SubscriptionSource {
  id: string; name: string; url: string; format: SubscriptionFormat;
  enabled: boolean; auto_update: boolean; update_interval_hours: number;
}
/** `autoId`: the ID still follows the name; it stops once the ID is edited, and saved sources keep theirs. */
export type SubscriptionDraft = Omit<SubscriptionSource, 'update_interval_hours'> & { update_interval_hours: string; key: string; autoId?: boolean };
export function subscriptionDraft(source: SubscriptionSource, key = source.id): SubscriptionDraft {
  return { ...source, key, update_interval_hours: String(source.update_interval_hours) };
}
export function decodeSubscription(source: SubscriptionDraft, index: number): SubscriptionSource {
  const { key: _key, autoId: _autoId, update_interval_hours, ...values } = source;
  return { ...values, update_interval_hours: convertFieldValue({ path: `filter_subscriptions.sources.${index}.update_interval_hours`, type: 'number', labelKey: 'subscriptions.interval', min: 1, max: 168 }, update_interval_hours) as number };
}

const ID_LIMIT = 32;
export const SUBSCRIPTION_ID = /^[a-z0-9-]{1,32}$/;

/** A valid, unique subscription ID from a name: lowercase ASCII, digits and hyphens, 1–32 characters. */
export function subscriptionId(name: string, taken: readonly string[]): string {
  const base = name.normalize('NFKD').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, ID_LIMIT).replace(/-+$/, '') || 'source';
  if (!taken.includes(base)) return base;
  for (let number = 2; ; number += 1) {
    const suffix = `-${number}`;
    const candidate = `${base.slice(0, ID_LIMIT - suffix.length).replace(/-+$/, '')}${suffix}`;
    if (!taken.includes(candidate)) return candidate;
  }
}
