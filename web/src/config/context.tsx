import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ApiError, StaleRequest, type ApiClient } from '../session/client';
import type { TransportChange } from '../session/client';
import { decodeSubscription, type SubscriptionDraft, type SubscriptionSource } from '../model/subscriptions';
import { decodeCacheRule, diffSettings, fieldDisplayValue, getPath, setPath, settingPages, convertFieldValue, ModelError, type CacheRuleDraft, type RawFieldValue, type SettingsObject } from '../model';

interface ConfigResponse { toml: string; revision: number; has_backup: boolean }
interface ParsedResponse { toml: string; settings: SettingsObject; transport_change?: TransportChange | null }
interface ValidationResponse { restart_required: boolean; transport_change: TransportChange | null }

export interface ConfigDraft {
  original: string;
  toml: string;
  revision: number;
  hasBackup: boolean;
  settings: SettingsObject;
  fields: Record<string, RawFieldValue>;
  optional: Record<string, boolean>;
  rules: CacheRuleDraft[] | null;
  sources: SubscriptionDraft[] | null;
  savedSources: SubscriptionSource[];
  stale: boolean;
  unknownApply: boolean;
  pendingApply: { toml: string; revision: number } | null;
  pendingRollback: { revision: number; acknowledgedRevision?: number } | null;
  previewTransportChange: TransportChange | null;
  pendingTransportChange: TransportChange | null;
}

export interface PreparedSave { toml: string; revision: number; version: number; restart_required: boolean; transportChange: TransportChange | null }
export interface PreparedRollback { revision: number; version: number; transportChange: TransportChange | null }
export interface CommitResult { refreshed: boolean; transportChange: TransportChange | null }
export type ConfigIssue = string | ModelError | ApiError;

export const toConfigIssue = (reason: unknown): ConfigIssue => reason instanceof ModelError || reason instanceof ApiError ? reason : reason instanceof Error ? reason.message : String(reason);

interface ConfigContextValue {
  draft: ConfigDraft | null;
  busy: boolean;
  locked: boolean;
  error: ConfigIssue | null;
  dirty: boolean;
  setError(error: ConfigIssue | null): void;
  updateField(path: string, value: RawFieldValue): void;
  setOptional(protocol: string, enabled: boolean): void;
  setRules(rules: CacheRuleDraft[]): void;
  setSources(sources: SubscriptionDraft[]): void;
  setToml(toml: string): void;
  importCertificate(target: 'dot' | 'doh' | 'doq', certificate: string, privateKey: string): Promise<void>;
  preview(): Promise<string>;
  validate(): Promise<ValidationResponse>;
  prepareSave(): Promise<PreparedSave>;
  commitPrepared(prepared: PreparedSave): Promise<CommitResult>;
  ensureParsed(): Promise<void>;
  reload(): Promise<void>;
  previewRollback(): Promise<PreparedRollback>;
  rollback(prepared: PreparedRollback): Promise<CommitResult>;
  resolveUnknown(): Promise<'applied' | 'pending' | 'changed'>;
  exportDraft(): Promise<void>;
  discard(): void;
  setUpdateLocked(locked: boolean): void;
}

const ConfigContext = createContext<ConfigContextValue | null>(null);
const fieldByPath = new Map(Object.values(settingPages).flatMap((page) => page.groups.flatMap((group) => group.fields)).map((field) => [field.path, field]));
const EMPTY_LISTENER = { listen: '', cert_file: '', key_file: '' };

function dirty(draft: ConfigDraft | null): boolean {
  if (!draft) return false;
  if (draft.toml !== draft.original || draft.rules !== null || draft.sources !== null || Object.keys(draft.optional).length) return true;
  return Object.entries(draft.fields).some(([path, raw]) => {
    const field = fieldByPath.get(path);
    return field && JSON.stringify(raw) !== JSON.stringify(fieldDisplayValue(getPath(draft.settings, path), field));
  });
}

function createDraft(config: ConfigResponse, parsed: ParsedResponse): ConfigDraft {
  return { original: config.toml, toml: config.toml, revision: config.revision,
    hasBackup: config.has_backup, settings: parsed.settings, fields: {}, optional: {},
    rules: null, sources: null, savedSources: (getPath(parsed.settings, 'filter_subscriptions.sources') as SubscriptionSource[] | undefined) ?? [], stale: false, unknownApply: false, pendingApply: null, pendingRollback: null,
    previewTransportChange: null, pendingTransportChange: null };
}

export function ConfigProvider({ api, active, refreshSession, children }: { api: ApiClient; active: boolean; refreshSession: () => Promise<void>; children: ReactNode }) {
  const [draft, setDraft] = useState<ConfigDraft | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ConfigIssue | null>(null);
  const wasActive = useRef(false);
  const version = useRef(0);
  const writeLock = useRef(false);
  const updateLock = useRef(false);
  const [updateLocked, setUpdateLockedState] = useState(false);
  const setUpdateLocked = useCallback((locked: boolean) => { updateLock.current = locked; setUpdateLockedState(locked); }, []);
  const assertVersion = (expected: number) => { if (expected !== version.current) throw new Error('Draft changed during this operation; preview again'); };

  const reload = useCallback(async () => {
    const owner = version.current;
    setBusy(true); setError(null);
    try {
      const config = await api.request<ConfigResponse>('config');
      const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: config.toml });
      assertVersion(owner);
      version.current += 1;
      setDraft(createDraft(config, parsed));
      writeLock.current = false;
    } catch (reason) {
      setError(toConfigIssue(reason));
      throw reason;
    } finally { setBusy(false); }
  }, [api]);

  useEffect(() => {
    if (active && !wasActive.current) {
      if (!draft) void reload().catch(() => {});
      else void api.request<ConfigResponse>('config').then((current) => {
        if (current.revision !== draft.revision) setError('api.REVISION');
      }).catch((reason) => setError(toConfigIssue(reason)));
    }
    wasActive.current = active;
  }, [active, api, draft, reload]);

  const discard = useCallback(() => { version.current += 1; writeLock.current = false; setDraft(null); setError(null); }, []);

  const updateField = useCallback((path: string, value: RawFieldValue) => {
    if (!fieldByPath.has(path)) throw new Error(`Unknown field ${path}`);
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, previewTransportChange: null, fields: { ...current.fields, [path]: value } }));
  }, []);
  const setOptional = useCallback((protocol: string, enabled: boolean) => {
    if (!['dot', 'doh', 'doq'].includes(protocol)) throw new Error('Unknown listener');
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, previewTransportChange: null, optional: { ...current.optional, [protocol]: enabled } }));
  }, []);
  const setRules = useCallback((rules: CacheRuleDraft[]) => {
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, previewTransportChange: null, rules }));
  }, []);
  const setToml = useCallback((toml: string) => {
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, toml, stale: true, previewTransportChange: null }));
  }, []);

  const setSources = useCallback((sources: SubscriptionDraft[]) => {
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, previewTransportChange: null, sources }));
  }, []);

  const importCertificate = useCallback(async (target: 'dot' | 'doh' | 'doq', certificate: string, privateKey: string) => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('Configuration is busy');
    const enabled = draft.optional[target] ?? getPath(draft.settings, target) !== null;
    if (!enabled) throw new Error('app.enableListener');
    writeLock.current = true;
    setBusy(true);
    try {
      const result = await api.request<{ identity: { cert_file: string; key_file: string } }>('certificates/import', 'POST', {
        revision: draft.revision, certificate_pem: certificate, private_key_pem: privateKey,
      });
      version.current += 1;
      setDraft((current) => current && ({ ...current, previewTransportChange: null, fields: { ...current.fields,
        [`${target}.cert_file`]: result.identity.cert_file, [`${target}.key_file`]: result.identity.key_file,
      } }));
    } finally {
      writeLock.current = false;
      setBusy(false);
    }
  }, [api, busy, draft]);

  const flush = useCallback(async (): Promise<string> => {
    if (!draft) throw new Error('Configuration not loaded');
    const owner = version.current;
    let baseline = draft.settings;
    if (draft.stale) {
      const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: draft.toml });
      assertVersion(owner);
      baseline = parsed.settings;
    }
    let next = baseline;
    for (const [protocol, enabled] of Object.entries(draft.optional)) {
      next = setPath(next, protocol, enabled ? (getPath(next, protocol) ?? { ...EMPTY_LISTENER }) : null);
    }
    for (const [path, raw] of Object.entries(draft.fields)) {
      if (path === 'web.public_host') {
        if (typeof raw !== 'string') throw new ModelError('settings.validation.check', path);
        next = setPath(next, 'web', raw === '' ? null : { public_host: raw });
        continue;
      }
      const protocol = path.split('.')[0];
      const enabled = draft.optional[protocol] ?? getPath(next, protocol) !== null;
      if (['dot', 'doh', 'doq'].includes(protocol) && !enabled) continue;
      const field = fieldByPath.get(path)!;
      next = setPath(next, path, convertFieldValue(field, raw));
    }
    if (draft.rules) {
      next = setPath(next, 'cache.rules', draft.rules.map((rule, index) => {
        try { return decodeCacheRule(rule); }
        catch (reason) {
          if (reason instanceof ModelError) throw new ModelError(reason.key, `cache.rules.${index}.${reason.path ?? 'name'}`, reason.params);
          throw reason;
        }
      }));
    }
    if (draft.sources !== null) next = setPath(next, 'filter_subscriptions.sources', draft.sources.map(decodeSubscription));
    const changes = diffSettings(baseline, next);
    if (!Object.keys(changes).length) {
      const candidate = draft.toml !== draft.original
        ? await api.request<ParsedResponse>('config/preview', 'POST', { toml: draft.toml, changes: {} })
        : null;
      assertVersion(owner);
      version.current += 1;
      setDraft((current) => current && ({ ...current, toml: candidate?.toml ?? draft.toml, settings: candidate?.settings ?? baseline,
        stale: false, fields: {}, optional: {}, rules: null, sources: null, previewTransportChange: candidate?.transport_change ?? null }));
      return candidate?.toml ?? draft.toml;
    }
    const preview = await api.request<ParsedResponse>('config/preview', 'POST', { toml: draft.toml, changes });
    assertVersion(owner);
    version.current += 1;
    setDraft((current) => current && ({ ...current, toml: preview.toml, settings: preview.settings, fields: {}, optional: {}, rules: null, sources: null, stale: false,
      previewTransportChange: preview.transport_change ?? null }));
    return preview.toml;
  }, [api, draft]);

  const preview = useCallback(async () => {
    setBusy(true); setError(null);
    const before = version.current;
    try { const toml = await flush(); assertVersion(before + 1); return toml; }
    catch (reason) { setError(toConfigIssue(reason)); throw reason; }
    finally { setBusy(false); }
  }, [flush]);

  const validate = useCallback(async () => {
    setBusy(true); setError(null);
    const before = version.current;
    try {
      const toml = await flush();
      assertVersion(before + 1);
      const owner = version.current;
      const result = await api.request<ValidationResponse>('config/validate', 'POST', { toml });
      assertVersion(owner);
      return result;
    } catch (reason) { setError(toConfigIssue(reason)); throw reason; }
    finally { setBusy(false); }
  }, [api, flush]);

  const ensureParsed = useCallback(async () => {
    if (!draft?.stale) return;
    const owner = version.current;
    const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: draft.toml });
    assertVersion(owner);
    version.current += 1;
    setDraft((current) => current && ({ ...current, settings: parsed.settings, stale: false }));
  }, [api, draft]);

  const prepareSave = useCallback(async (): Promise<PreparedSave> => {
    if (!draft || draft.unknownApply || writeLock.current || updateLock.current) throw new Error('Resolve the previous operation before saving');
    setBusy(true); setError(null);
    const before = version.current;
    try {
      const toml = await flush();
      assertVersion(before + 1);
      const owner = version.current;
      const impact = await api.request<ValidationResponse>('config/validate', 'POST', { toml });
      assertVersion(owner);
      return { toml, revision: draft.revision, version: owner, restart_required: impact.restart_required, transportChange: impact.transport_change };
    } catch (reason) { setError(toConfigIssue(reason)); throw reason; }
    finally { setBusy(false); }
  }, [api, draft, flush]);

  const commitPrepared = useCallback(async (prepared: PreparedSave) => {
    if (updateLock.current) throw new Error('updates.locked');
    assertVersion(prepared.version);
    writeLock.current = true;
    setBusy(true); setError(null);
    let sent = false;
    let unknown = false;
    try {
      sent = true;
      const acknowledged = await api.request<{ revision: number; transport_change: TransportChange | null }>('config', 'PUT', {
        toml: prepared.toml, revision: prepared.revision,
        ...(prepared.transportChange?.requires_http_confirmation ? { allow_http_downgrade: true } : {}),
      });
      sent = false;
      assertVersion(prepared.version);
      version.current += 1;
      setDraft((current) => current && ({ ...current, original: prepared.toml, toml: prepared.toml, revision: acknowledged.revision,
        hasBackup: true, fields: {}, optional: {}, rules: null, sources: null, savedSources: (getPath(current.settings, 'filter_subscriptions.sources') as SubscriptionSource[] | undefined) ?? [], stale: false, unknownApply: false, pendingApply: null, pendingRollback: null }));
      if (acknowledged.transport_change) return { refreshed: false, transportChange: acknowledged.transport_change };
      try { await reload(); return { refreshed: true, transportChange: null }; }
      catch { setError('app.savedRefreshFailed'); return { refreshed: false, transportChange: null }; }
      finally { void refreshSession(); }
    } catch (reason) {
      if (sent && (reason instanceof StaleRequest || reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code))) {
        unknown = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: { toml: prepared.toml, revision: prepared.revision },
          pendingTransportChange: prepared.transportChange }));
      }
      setError(toConfigIssue(reason));
      throw reason;
    } finally { writeLock.current = unknown; setBusy(false); }
  }, [api, reload, refreshSession]);

  const resolveUnknown = useCallback(async (): Promise<'applied' | 'pending' | 'changed'> => {
    if (!draft?.unknownApply || (!draft.pendingApply && !draft.pendingRollback)) throw new Error('No unknown operation');
    const owner = version.current;
    setBusy(true);
    try {
      const remote = await api.request<ConfigResponse>('config');
      assertVersion(owner);
      if (draft.pendingRollback) {
        if (remote.revision === draft.pendingRollback.acknowledgedRevision) {
          const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: remote.toml });
          assertVersion(owner);
          version.current += 1;
          setDraft(createDraft(remote, parsed));
          writeLock.current = false;
          void refreshSession();
          return 'applied';
        }
        return remote.revision === draft.pendingRollback.revision ? 'pending' : 'changed';
      }
      if (!draft.pendingApply) throw new Error('No unknown save');
      if (remote.revision === draft.pendingApply.revision) {
        // A disconnected PUT can be admitted after this read; old revision is not proof of failure.
        return 'pending';
      }
      if (remote.toml === draft.pendingApply.toml) {
        const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: remote.toml });
        assertVersion(owner);
        version.current += 1;
        setDraft(createDraft(remote, parsed));
        writeLock.current = false;
        void refreshSession();
        return 'applied';
      }
      return 'changed';
    } finally { setBusy(false); }
  }, [api, draft, refreshSession]);

  const previewRollback = useCallback(async (): Promise<PreparedRollback> => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('Configuration is busy');
    const owner = version.current;
    const result = await api.request<{ transport_change: TransportChange | null }>('config/rollback/preview', 'POST', { revision: draft.revision });
    assertVersion(owner);
    return { revision: draft.revision, version: owner, transportChange: result.transport_change };
  }, [api, busy, draft]);

  const rollback = useCallback(async (prepared: PreparedRollback): Promise<CommitResult> => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('Configuration is busy');
    assertVersion(prepared.version);
    writeLock.current = true;
    setBusy(true); setError(null);
    let sent = false;
    let keepLocked = false;
    try {
      sent = true;
      const response = await api.request<{ revision: number; transport_change: TransportChange | null }>('config/rollback', 'POST', {
        revision: prepared.revision,
        ...(prepared.transportChange?.requires_http_confirmation ? { allow_http_downgrade: true } : {}),
      });
      sent = false;
      if (response.transport_change) return { refreshed: false, transportChange: response.transport_change };
      try { await reload(); return { refreshed: true, transportChange: null }; }
      catch {
        keepLocked = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: null,
          pendingRollback: { revision: draft.revision, acknowledgedRevision: response.revision } }));
        setError('app.rollbackRefreshFailed');
        return { refreshed: false, transportChange: null };
      }
      finally { void refreshSession(); }
    } catch (reason) {
      if (sent && (reason instanceof StaleRequest || reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code))) {
        keepLocked = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: null,
          pendingRollback: { revision: draft.revision } }));
        setDraft((current) => current && ({ ...current, pendingTransportChange: prepared.transportChange }));
      }
      setError(toConfigIssue(reason));
      throw reason;
    } finally { writeLock.current = keepLocked; setBusy(false); }
  }, [api, busy, draft, reload, refreshSession]);

  const exportDraft = useCallback(async () => {
    const before = version.current;
    const toml = await flush();
    assertVersion(before + 1);
    const url = URL.createObjectURL(new Blob([toml], { type: 'text/plain;charset=utf-8' }));
    const link = document.createElement('a');
    link.href = url; link.download = 'parins.toml'; document.body.append(link); link.click(); link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  }, [flush]);

  const value = useMemo<ConfigContextValue>(() => ({ draft, busy: busy || updateLocked, locked: writeLock.current || updateLocked || Boolean(draft?.unknownApply), error, dirty: dirty(draft), setError, updateField,
    setOptional, setRules, setSources, setToml, importCertificate, preview, validate, prepareSave, commitPrepared, ensureParsed, reload, previewRollback, rollback, resolveUnknown, exportDraft, discard, setUpdateLocked }),
  [draft, busy, updateLocked, error, updateField, setOptional, setRules, setSources, setToml, importCertificate, preview, validate, prepareSave, commitPrepared, ensureParsed, reload, previewRollback, rollback, resolveUnknown, exportDraft, discard, setUpdateLocked]);
  return <ConfigContext.Provider value={value}>{children}</ConfigContext.Provider>;
}

export function useConfig() {
  const value = useContext(ConfigContext);
  if (!value) throw new Error('ConfigProvider is required');
  return value;
}
