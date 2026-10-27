import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ApiError, StaleRequest, type ApiClient } from '../session/client';
import type { TransportChange } from '../session/client';
import { decodeSubscription, type SubscriptionDraft, type SubscriptionSource } from '../model/subscriptions';
import { downloadNewSources, newSources } from '../features/subscriptions/download';
import { configList, decodeCacheRule, diffSettings, fieldDisplayValue, getPath, setPath, settingPages, convertFieldValue, ModelError, type CacheRuleDraft, type RawFieldValue, type SettingsObject } from '../model';

export interface ConfigResponse { toml: string; revision: number; has_backup: boolean }
interface ParsedResponse { toml: string; settings: SettingsObject; transport_change?: TransportChange | null }
interface ValidationResponse { restart_required: boolean; transport_change: TransportChange | null }

/**
 * The saved configuration changed elsewhere and both sides edited the same
 * settings (or the draft edits only TOML text). The draft is kept until the
 * administrator chooses which side to keep.
 */
export interface ConfigConflict {
  /** Settings of the saved configuration the draft started from. */
  base: SettingsObject;
  remote: ConfigResponse;
  remoteSettings: SettingsObject;
  /** Settings paths changed on both sides; empty when the draft edits only TOML text. */
  paths: string[];
}

export interface ConfigDraft {
  original: string;
  toml: string;
  /** Internal optimistic-concurrency token for saves; never shown. */
  revision: number;
  hasBackup: boolean;
  /** Parsed settings of `original`, the baseline for merging with a change made elsewhere. */
  base: SettingsObject;
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
  conflict: ConfigConflict | null;
  /** The draft was merged onto a configuration saved elsewhere and needs another save. */
  merged: boolean;
}

export interface PreparedSave { toml: string; settings: SettingsObject; base: SettingsObject; revision: number; version: number; restart_required: boolean; transportChange: TransportChange | null }
export interface PreparedRollback { revision: number; version: number; transportChange: TransportChange | null }
/** `reconciled` means the save met a change made elsewhere: the draft was merged or awaits a choice. */
export interface CommitResult { refreshed: boolean; transportChange: TransportChange | null; restartRequired: boolean; reconciled: Reconciled | null }
/** How a draft met a configuration saved elsewhere: reloaded (no edits), merged, or awaiting a choice. */
export type Reconciled = 'reloaded' | 'merged' | 'conflict';
export type ConfigIssue = string | ModelError | ApiError;

export const toConfigIssue = (reason: unknown): ConfigIssue => reason instanceof ModelError || reason instanceof ApiError ? reason : reason instanceof Error ? reason.message : String(reason);

interface ConfigContextValue {
  draft: ConfigDraft | null;
  busy: boolean;
  locked: boolean;
  error: ConfigIssue | null;
  /** Translation key of the step a save is waiting on, such as downloading new subscription lists. */
  progress: string | null;
  dirty: boolean;
  changes: number;
  setError(error: ConfigIssue | null): void;
  updateField(path: string, value: RawFieldValue): void;
  setOptional(protocol: OptionalListener, enabled: boolean): void;
  setRules(rules: CacheRuleDraft[]): void;
  setSources(sources: SubscriptionDraft[]): void;
  setToml(toml: string): void;
  importCertificate(target: OptionalListener, certificate: string, privateKey: string): Promise<void>;
  preview(): Promise<string>;
  validate(): Promise<ValidationResponse>;
  prepareSave(): Promise<PreparedSave>;
  commitPrepared(prepared: PreparedSave): Promise<CommitResult>;
  ensureParsed(): Promise<void>;
  reload(): Promise<void>;
  /** Null when the configuration changed elsewhere and the draft was brought up to date instead. */
  previewRollback(): Promise<PreparedRollback | null>;
  rollback(prepared: PreparedRollback): Promise<CommitResult>;
  resolveUnknown(): Promise<'applied' | 'pending' | 'changed'>;
  resolveConflict(keep: 'mine' | 'theirs'): Promise<void>;
  exportDraft(): Promise<void>;
  discard(): void;
  setUpdateLocked(locked: boolean): void;
}

type OptionalListener = 'dot' | 'doh' | 'doq';
const ConfigContext = createContext<ConfigContextValue | null>(null);
const fieldByPath = new Map(Object.values(settingPages).flatMap((page) => page.groups.flatMap((group) => group.fields)).map((field) => [field.path, field]));
const conditionalFields = [...fieldByPath.values()].filter((field) => field.when);
const EMPTY_LISTENER = { listen: '', cert_file: '', key_file: '' };

/** Number of unsaved changes shown to the user; zero means the draft is clean. */
function changeCount(draft: ConfigDraft | null): number {
  if (!draft) return 0;
  const fields = Object.entries(draft.fields).filter(([path, raw]) => {
    const field = fieldByPath.get(path);
    return field && JSON.stringify(raw) !== JSON.stringify(fieldDisplayValue(getPath(draft.settings, path), field));
  }).length;
  const total = fields + Object.keys(draft.optional).length + (draft.rules !== null ? 1 : 0) + (draft.sources !== null ? 1 : 0);
  return total || (draft.toml !== draft.original ? 1 : 0);
}

function createDraft(config: ConfigResponse, parsed: ParsedResponse): ConfigDraft {
  return { original: config.toml, toml: config.toml, revision: config.revision,
    hasBackup: config.has_backup, base: parsed.settings, settings: parsed.settings, fields: {}, optional: {},
    rules: null, sources: null, savedSources: configList(parsed.settings, 'filter_subscriptions.sources'), stale: false, unknownApply: false, pendingApply: null, pendingRollback: null,
    previewTransportChange: null, pendingTransportChange: null, conflict: null, merged: false };
}

function isObject(value: unknown): value is SettingsObject {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/** Dotted paths of the leaves in a `diffSettings` result; arrays and optional sections are leaves. */
function changedPaths(changes: SettingsObject, prefix = ''): string[] {
  return Object.entries(changes).flatMap(([key, value]) => {
    const path = prefix ? `${prefix}.${key}` : key;
    return isObject(value) && Object.keys(value).length ? changedPaths(value, path) : [path];
  });
}

/** Paths edited on both sides, where one path equals or contains the other. */
export function overlappingPaths(mine: SettingsObject, theirs: SettingsObject): string[] {
  const other = changedPaths(theirs);
  const within = (inner: string, outer: string) => inner === outer || inner.startsWith(`${outer}.`);
  return changedPaths(mine).filter((path) => other.some((candidate) => within(path, candidate) || within(candidate, path)));
}

export function ConfigProvider({ api, active, refreshSession, children }: { api: ApiClient; active: boolean; refreshSession: () => Promise<void>; children: ReactNode }) {
  const [draft, setDraft] = useState<ConfigDraft | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ConfigIssue | null>(null);
  const [progress, setProgress] = useState<string | null>(null);
  const wasActive = useRef(false);
  const version = useRef(0);
  const writeLock = useRef(false);
  const updateLock = useRef(false);
  const [updateLocked, setUpdateLockedState] = useState(false);
  const setUpdateLocked = useCallback((locked: boolean) => { updateLock.current = locked; setUpdateLockedState(locked); }, []);
  const assertVersion = (expected: number) => { if (expected !== version.current) throw new Error('app.draftChanged'); };

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

  /**
   * Brings a draft onto a configuration saved elsewhere. Non-overlapping edits
   * are merged by Rust preview onto the new configuration; otherwise the draft
   * is kept and the administrator chooses. Revisions stay internal.
   */
  const reconcile = useCallback(async (local: { base: SettingsObject; settings: SettingsObject }): Promise<'merged' | 'conflict'> => {
    const owner = version.current;
    const remote = await api.request<ConfigResponse>('config');
    const parsed = await api.request<ParsedResponse>('config/parse', 'POST', { toml: remote.toml });
    assertVersion(owner);
    const mine = diffSettings(local.base, local.settings);
    const paths = overlappingPaths(mine, diffSettings(local.base, parsed.settings));
    if (Object.keys(mine).length && !paths.length) {
      const merged = await api.request<ParsedResponse>('config/preview', 'POST', { toml: remote.toml, changes: mine });
      assertVersion(owner);
      version.current += 1;
      setDraft({ ...createDraft(remote, parsed), toml: merged.toml, settings: merged.settings, merged: true });
      writeLock.current = false;
      return 'merged';
    }
    version.current += 1;
    setDraft((current) => current && ({ ...current, conflict: { base: local.base, remote, remoteSettings: parsed.settings, paths } }));
    return 'conflict';
  }, [api]);


  const discard = useCallback(() => { version.current += 1; writeLock.current = false; setDraft(null); setError(null); }, []);

  const updateField = useCallback((path: string, value: RawFieldValue) => {
    if (writeLock.current || updateLock.current) return;
    version.current += 1;
    setDraft((current) => current && ({ ...current, previewTransportChange: null, fields: { ...current.fields, [path]: value } }));
  }, []);
  const setOptional = useCallback((protocol: OptionalListener, enabled: boolean) => {
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

  const importCertificate = useCallback(async (target: OptionalListener, certificate: string, privateKey: string) => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('api.BUSY');
    const enabled = draft.optional[target] ?? getPath(draft.settings, target) !== null;
    if (!enabled) throw new Error('app.enableListener');
    writeLock.current = true;
    setBusy(true);
    try {
      const result = await api.request<{ identity: { cert_file: string; key_file: string } }>('certificates/import', 'POST', {
        certificate_pem: certificate, private_key_pem: privateKey,
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

  const flush = useCallback(async (): Promise<{ toml: string; settings: SettingsObject }> => {
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
    // A field hidden by its condition, such as a custom block address under another block response, is removed.
    for (const field of conditionalFields) {
      if (getPath(next, field.when!.path) !== field.when!.value && getPath(next, field.path) != null) next = setPath(next, field.path, null);
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
      return { toml: candidate?.toml ?? draft.toml, settings: candidate?.settings ?? baseline };
    }
    const preview = await api.request<ParsedResponse>('config/preview', 'POST', { toml: draft.toml, changes });
    assertVersion(owner);
    version.current += 1;
    setDraft((current) => current && ({ ...current, toml: preview.toml, settings: preview.settings, fields: {}, optional: {}, rules: null, sources: null, stale: false,
      previewTransportChange: preview.transport_change ?? null }));
    return { toml: preview.toml, settings: preview.settings };
  }, [api, draft]);

  /** Brings the draft up to date after a save elsewhere, keeping unsaved edits. */
  const syncWithServer = useCallback(async (): Promise<Reconciled | null> => {
    if (!draft || draft.unknownApply) return null;
    if (!changeCount(draft)) { await reload(); setError('app.reloadedElsewhere'); return 'reloaded'; }
    const before = version.current;
    const flushed = await flush();
    assertVersion(before + 1);
    return reconcile({ base: draft.base, settings: flushed.settings });
  }, [draft, flush, reconcile, reload]);

  useEffect(() => {
    if (active && !wasActive.current) {
      if (!draft) void reload().catch(() => {});
      else void api.request<ConfigResponse>('config').then(async (current) => {
        if (current.revision !== draft.revision) await syncWithServer();
      }).catch((reason) => setError(toConfigIssue(reason)));
    }
    wasActive.current = active;
  }, [active, api, draft, reload, syncWithServer]);

  const preview = useCallback(async () => {
    setBusy(true); setError(null);
    const before = version.current;
    try { const { toml } = await flush(); assertVersion(before + 1); return toml; }
    catch (reason) { setError(toConfigIssue(reason)); throw reason; }
    finally { setBusy(false); }
  }, [flush]);

  const validate = useCallback(async () => {
    setBusy(true); setError(null);
    const before = version.current;
    try {
      const { toml } = await flush();
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
    if (!draft || draft.unknownApply || draft.conflict || writeLock.current || updateLock.current) throw new Error('api.BUSY');
    setBusy(true); setError(null);
    const before = version.current;
    try {
      const { toml, settings } = await flush();
      assertVersion(before + 1);
      const owner = version.current;
      // New subscription lists are downloaded and checked first; Rust accepts them only then.
      if (newSources(settings, draft.savedSources).length) {
        setProgress('subscriptions.downloading');
        try { await downloadNewSources(api, settings, draft.savedSources); } finally { setProgress(null); }
        assertVersion(owner);
      }
      const impact = await api.request<ValidationResponse>('config/validate', 'POST', { toml });
      assertVersion(owner);
      return { toml, settings, base: draft.base, revision: draft.revision, version: owner, restart_required: impact.restart_required, transportChange: impact.transport_change };
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
      const acknowledged = await api.request<{ revision: number; restart_required: boolean; transport_change: TransportChange | null }>('config', 'PUT', {
        toml: prepared.toml, revision: prepared.revision,
        ...(prepared.transportChange?.requires_http_confirmation ? { allow_http_downgrade: true } : {}),
      });
      sent = false;
      assertVersion(prepared.version);
      version.current += 1;
      setDraft((current) => current && ({ ...current, original: prepared.toml, toml: prepared.toml, revision: acknowledged.revision,
        hasBackup: true, base: prepared.settings, fields: {}, optional: {}, rules: null, sources: null, savedSources: configList(current.settings, 'filter_subscriptions.sources'), stale: false, unknownApply: false, pendingApply: null, pendingRollback: null, merged: false }));
      const restartRequired = acknowledged.restart_required;
      if (acknowledged.transport_change) return { refreshed: false, transportChange: acknowledged.transport_change, restartRequired, reconciled: null };
      try { await reload(); return { refreshed: true, transportChange: null, restartRequired, reconciled: null }; }
      catch { setError('app.savedRefreshFailed'); return { refreshed: false, transportChange: null, restartRequired, reconciled: null }; }
      finally { void refreshSession(); }
    } catch (reason) {
      if (reason instanceof ApiError && reason.code === 'REVISION') {
        // Rust refused the save because the configuration was saved elsewhere; nothing was applied.
        writeLock.current = false;
        const reconciled = await reconcile(prepared);
        return { refreshed: false, transportChange: null, restartRequired: false, reconciled };
      }
      if (sent && (reason instanceof StaleRequest || reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code))) {
        unknown = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: { toml: prepared.toml, revision: prepared.revision },
          pendingTransportChange: prepared.transportChange }));
      }
      setError(toConfigIssue(reason));
      throw reason;
    } finally { writeLock.current = unknown; setBusy(false); }
  }, [api, reload, reconcile, refreshSession]);

  const resolveConflict = useCallback(async (keep: 'mine' | 'theirs') => {
    const conflict = draft?.conflict;
    if (!conflict || writeLock.current || updateLock.current) return;
    if (keep === 'theirs') { await reload(); return; }
    setBusy(true); setError(null);
    const before = version.current;
    try {
      const flushed = await flush();
      assertVersion(before + 1);
      const owner = version.current;
      const mine = diffSettings(conflict.base, flushed.settings);
      // A draft that edits only TOML text replaces the other document as written.
      const kept = Object.keys(mine).length
        ? await api.request<ParsedResponse>('config/preview', 'POST', { toml: conflict.remote.toml, changes: mine })
        : flushed;
      assertVersion(owner);
      version.current += 1;
      setDraft({ ...createDraft(conflict.remote, { toml: conflict.remote.toml, settings: conflict.remoteSettings }), toml: kept.toml, settings: kept.settings, merged: true });
    } catch (reason) { setError(toConfigIssue(reason)); throw reason; }
    finally { setBusy(false); }
  }, [api, draft, flush, reload]);

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

  const previewRollback = useCallback(async (): Promise<PreparedRollback | null> => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('api.BUSY');
    const owner = version.current;
    try {
      const result = await api.request<{ transport_change: TransportChange | null }>('config/rollback/preview', 'POST', { revision: draft.revision });
      assertVersion(owner);
      return { revision: draft.revision, version: owner, transportChange: result.transport_change };
    } catch (reason) {
      if (!(reason instanceof ApiError && reason.code === 'REVISION')) throw reason;
      // "Undo last save" would now undo a save made elsewhere; show the new configuration instead.
      await syncWithServer();
      return null;
    }
  }, [api, busy, draft, syncWithServer]);

  const rollback = useCallback(async (prepared: PreparedRollback): Promise<CommitResult> => {
    if (!draft || writeLock.current || updateLock.current || busy) throw new Error('api.BUSY');
    assertVersion(prepared.version);
    writeLock.current = true;
    setBusy(true); setError(null);
    let sent = false;
    let keepLocked = false;
    try {
      sent = true;
      const response = await api.request<{ revision: number; restart_required: boolean; transport_change: TransportChange | null }>('config/rollback', 'POST', {
        revision: prepared.revision,
        ...(prepared.transportChange?.requires_http_confirmation ? { allow_http_downgrade: true } : {}),
      });
      sent = false;
      const restartRequired = response.restart_required;
      if (response.transport_change) return { refreshed: false, transportChange: response.transport_change, restartRequired, reconciled: null };
      try { await reload(); return { refreshed: true, transportChange: null, restartRequired, reconciled: null }; }
      catch {
        keepLocked = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: null,
          pendingRollback: { revision: draft.revision, acknowledgedRevision: response.revision } }));
        setError('app.rollbackRefreshFailed');
        return { refreshed: false, transportChange: null, restartRequired, reconciled: null };
      }
      finally { void refreshSession(); }
    } catch (reason) {
      if (reason instanceof ApiError && reason.code === 'REVISION') {
        writeLock.current = false;
        return { refreshed: false, transportChange: null, restartRequired: false, reconciled: await syncWithServer() };
      }
      if (sent && (reason instanceof StaleRequest || reason instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(reason.code))) {
        keepLocked = true;
        setDraft((current) => current && ({ ...current, unknownApply: true, pendingApply: null,
          pendingRollback: { revision: draft.revision }, pendingTransportChange: prepared.transportChange }));
      }
      setError(toConfigIssue(reason));
      throw reason;
    } finally { writeLock.current = keepLocked; setBusy(false); }
  }, [api, busy, draft, reload, refreshSession, syncWithServer]);

  const exportDraft = useCallback(async () => {
    const before = version.current;
    const { toml } = await flush();
    assertVersion(before + 1);
    const url = URL.createObjectURL(new Blob([toml], { type: 'text/plain;charset=utf-8' }));
    const link = document.createElement('a');
    link.href = url; link.download = 'parins.toml'; document.body.append(link); link.click(); link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
  }, [flush]);

  const value = useMemo<ConfigContextValue>(() => ({ draft, busy: busy || updateLocked, locked: writeLock.current || updateLocked || Boolean(draft?.unknownApply), error, progress, dirty: changeCount(draft) > 0, changes: changeCount(draft), setError, updateField,
    setOptional, setRules, setSources, setToml, importCertificate, preview, validate, prepareSave, commitPrepared, ensureParsed, reload, previewRollback, rollback, resolveUnknown, resolveConflict, exportDraft, discard, setUpdateLocked }),
  [draft, busy, updateLocked, error, progress, updateField, setOptional, setRules, setSources, setToml, importCertificate, preview, validate, prepareSave, commitPrepared, ensureParsed, reload, previewRollback, rollback, resolveUnknown, resolveConflict, exportDraft, discard, setUpdateLocked]);
  return <ConfigContext.Provider value={value}>{children}</ConfigContext.Provider>;
}

export function useConfig() {
  const value = useContext(ConfigContext);
  if (!value) throw new Error('ConfigProvider is required');
  return value;
}
