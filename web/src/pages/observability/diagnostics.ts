import { hasTranslation, translate, type Language } from '../../i18n';

export interface DnsHealth { state: 'unconfigured' | 'starting' | 'running' | 'stopped' | 'failed'; ready: boolean; generation: number; changed_at_ms: number; code: string | null }
export interface CacheDecision { outcome: string; reason: string | null }
export interface UpstreamResult { protocol: string | null; stage: string; outcome: string; reason: string | null }
export interface UpstreamAttempt extends UpstreamResult { pool_generation: number; slot: number; code: number | null; elapsed_ms: number }
export interface UpstreamTrace { attempts: UpstreamAttempt[]; omitted: number }
export interface UpstreamDiagnostics { scope: string; pool_generation: number; since_ms: number | null; slots: { slot: number; counts: (UpstreamResult & { count: number })[] }[] }
export interface QuicDiagnostics { scope: string; doq: Record<string, number>; doh3: Record<string, number> }
export interface CacheDiagnostics {
  scope: string;
  since_ms: number | null;
  lookup: { fresh: number; stale: number; miss: number; bypass: number; disabled: number; reasons: Record<string, number> };
  store: { admitted: number; replaced: number; skipped: number; superseded_only: number; reasons: Record<string, number> };
}
export function diagnosticLabel(code: string, language: Language): string {
  const key = `reliability.${code}`;
  return hasTranslation(key) ? translate(key, language) : code.replace(/^(?:upstream_reason_|reason_|lookup_|store_|stage_|outcome_|cleanup_|drop_)/, '');
}
export function protocolLabel(protocol: string | null, language: Language): string {
  if (protocol === null) return translate('reliability.pendingProtocol', language);
  return ({ doh2: 'DoH / HTTP/2', doh3: 'DoH / HTTP/3', dot: 'DoT', doq: 'DoQ' } as Record<string, string>)[protocol] ?? protocol.toUpperCase();
}
export function decisionLabel(kind: 'lookup' | 'store', decision: CacheDecision | null, language: Language): string {
  if (!decision) return translate('reliability.noDecision', language);
  return `${diagnosticLabel(`${kind}_${decision.outcome}`, language)}${decision.reason ? ` · ${diagnosticLabel(`reason_${decision.reason}`, language)}` : ''}`;
}
