import { convertFieldValue } from './values';

export type SubscriptionFormat = 'domain_list' | 'hosts_blocklist';
export interface SubscriptionSource {
  id: string; name: string; url: string; format: SubscriptionFormat;
  enabled: boolean; auto_update: boolean; update_interval_hours: number;
}
export interface SubscriptionSettings {
  enabled: boolean; max_rules: number; max_memory_bytes: number; max_disk_bytes: number;
  sources: SubscriptionSource[];
}
export type SubscriptionDraft = Omit<SubscriptionSource, 'update_interval_hours'> & { update_interval_hours: string; key: string };
export const defaultSubscriptions: SubscriptionSettings = {
  enabled: true, max_rules: 1000000, max_memory_bytes: 134217728, max_disk_bytes: 268435456, sources: [],
};
export function subscriptionDraft(source: SubscriptionSource, key = source.id): SubscriptionDraft {
  return { ...source, key, update_interval_hours: String(source.update_interval_hours) };
}
export function decodeSubscription(source: SubscriptionDraft, index: number): SubscriptionSource {
  const { key: _key, update_interval_hours, ...values } = source;
  return { ...values, update_interval_hours: convertFieldValue({ path: `filter_subscriptions.sources.${index}.update_interval_hours`, type: 'number', labelKey: 'subscriptions.interval', min: 1, max: 168 }, update_interval_hours) as number };
}
