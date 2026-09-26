//! Process-owned subscription work. Manager owns the final mutation/freeze gate;
//! this module never calls Manager while holding its worker or storage locks.
mod material;
mod operations;

use super::{
    Source,
    download::SubscriptionReader,
    handle::PolicyHandle,
    settings::{Format, Settings},
    store::{SourceRecord, Store, WorkPin},
};
use crate::policy::Policy;
use anyhow::{Context, Result, ensure};
use hickory_proto::rr::Name;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub use material::{Material, read_only};
pub use operations::{Begin, Work, WorkCandidate, WorkRequest};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub line: Option<u64>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.code)
    }
}
impl std::error::Error for Failure {}
impl Failure {
    fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            line: None,
        }
    }
    pub fn from_error(error: &anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<Self>() {
            return failure.clone();
        }
        if let Some(parse) = error.downcast_ref::<crate::policy::canonical::Error>() {
            use crate::policy::canonical::ErrorKind;
            return Self {
                code: match parse.kind {
                    ErrorKind::MemoryLimit | ErrorKind::Allocation => "subscription_memory_limit",
                    ErrorKind::Cancelled => "subscription_cancelled",
                    _ => "subscription_parse",
                }
                .into(),
                line: (parse.line != 0).then_some(parse.line),
            };
        }
        if error
            .downcast_ref::<super::download::DownloadError>()
            .is_some()
        {
            return Self::new("subscription_download");
        }
        // All remaining errors are local storage/IO failures. Never return raw
        // paths, configured URL queries, TLS errors, or remote response text.
        Self::new("subscription_storage_unavailable")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DraftSource {
    pub id: String,
    pub url: String,
    pub format: Format,
}

#[derive(Clone, Debug, Serialize)]
pub struct Operation {
    pub id: String,
    pub kind: String,
    pub source_id: Option<String>,
    pub fingerprint: Option<String>,
    pub status: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub error: Option<Failure>,
    pub sha256: Option<String>,
    pub rules: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SourceStatus {
    pub id: String,
    pub fingerprint: String,
    pub ready: bool,
    pub active: bool,
    pub input_rules: u64,
    pub bytes: u64,
    pub last_success: Option<u64>,
    pub last_attempt: Option<u64>,
    pub next_update: Option<u64>,
    pub failures: u32,
    pub error: Option<Failure>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SubscriptionState {
    pub config_revision: u64,
    pub enabled: bool,
    pub sources: Vec<SourceStatus>,
    pub generation: u64,
    pub content_revision: u64,
    pub input_rules: usize,
    pub index_rules: usize,
    pub index_bytes: usize,
    pub retained_bytes: usize,
    pub disk_bytes: u64,
    pub operation: Option<Operation>,
    pub recent_operation: Option<Operation>,
    pub unavailable_reason: Option<String>,
}
#[derive(Serialize)]
pub struct CheckResult {
    pub generation: u64,
    #[serde(flatten)]
    pub explanation: crate::policy::Explanation,
}

struct State {
    settings: Settings,
    local: Policy,
    revision: u64,
    previous: Vec<String>,
    input_digest: Option<[u8; 32]>,
    records: Vec<SourceRecord>,
    ready: Vec<String>,
    content_revision: u64,
    input_rules: usize,
    ready_policy: bool,
    disk_bytes: u64,
    unavailable: Option<String>,
    operation: Option<Operation>,
    recent: Option<Operation>,
    active_request: Option<WorkRequest>,
    index_pin: Option<WorkPin>,
}

pub struct Service {
    path: PathBuf,
    handle: Arc<PolicyHandle>,
    state: Mutex<State>,
    store: Mutex<Option<Store>>,
    worker: Arc<Semaphore>,
    reader: SubscriptionReader,
    closed: AtomicBool,
    cancel: tokio::sync::Notify,
    #[cfg(test)]
    responses: Mutex<Option<std::collections::VecDeque<Vec<crate::https_reader::WireResponse>>>>,
}

// A blocking builder retains this permit until its real buffers are released,
// even if the async owner goes away. No aborted handle pretends memory is free.
struct Lease {
    _permit: OwnedSemaphorePermit,
}

pub struct ConfigCandidate {
    service: Arc<Service>,
    lease: Arc<Lease>,
    settings: Settings,
    local: Policy,
    expected_revision: u64,
    input_rules: usize,
    material: Material,
    pins: Vec<WorkPin>,
}

impl Service {
    pub async fn open(
        path: PathBuf,
        local: Policy,
        settings: Settings,
        revision: u64,
        previous: Vec<String>,
    ) -> Result<Arc<Self>> {
        Self::open_inner(path, local, settings, revision, previous, false).await
    }
    pub async fn open_frozen(
        path: PathBuf,
        local: Policy,
        settings: Settings,
        revision: u64,
        previous: Vec<String>,
    ) -> Result<Arc<Self>> {
        Self::open_inner(path, local, settings, revision, previous, true).await
    }
    async fn open_inner(
        path: PathBuf,
        local: Policy,
        settings: Settings,
        revision: u64,
        previous: Vec<String>,
        frozen: bool,
    ) -> Result<Arc<Self>> {
        settings.validate()?;
        let current = settings.fingerprints()?;
        let quota = settings.max_disk_bytes;
        let previous_open = previous.clone();
        let store_path = path.clone();
        let opened = tokio::task::spawn_blocking(move || {
            if frozen {
                return Ok(None);
            }
            let mut store = Store::open(&store_path, quota, now())?;
            store.set_references(current, previous_open, now())?;
            Ok::<_, anyhow::Error>(Some(store))
        })
        .await?;
        let (store, unavailable) = match opened {
            Ok(store) => (store, None),
            Err(_) => (None, Some("subscription_storage_unavailable".into())),
        };
        let service = Arc::new(Self {
            path,
            handle: PolicyHandle::new(local.clone()),
            state: Mutex::new(State {
                settings: settings.clone(),
                local: local.clone(),
                revision,
                previous,
                input_digest: None,
                records: vec![],
                ready: vec![],
                content_revision: 0,
                input_rules: local.input_rules(),
                ready_policy: false,
                disk_bytes: 0,
                unavailable,
                operation: None,
                recent: None,
                active_request: None,
                index_pin: None,
            }),
            store: Mutex::new(store),
            worker: Arc::new(Semaphore::new(1)),
            reader: SubscriptionReader::new(),
            closed: AtomicBool::new(false),
            cancel: tokio::sync::Notify::new(),
            #[cfg(test)]
            responses: Mutex::new(None),
        });
        let owner = service.clone();
        let initial = tokio::task::spawn_blocking(move || {
            if frozen {
                let material = material::read_only(&owner.path, &settings, &local)?;
                if settings.effective().next().is_some() {
                    let view = Store::read_only(&owner.path)?;
                    let records = view.records().to_vec();
                    let ready = records
                        .iter()
                        .filter(|record| view.verified_content(&record.fingerprint).is_ok())
                        .map(|r| r.fingerprint.clone())
                        .collect();
                    let mut state = owner.state.lock().unwrap();
                    state.records = records;
                    state.ready = ready;
                    state.content_revision = view.revision();
                }
                return Ok(material);
            }
            let mut locked = owner.store.lock().unwrap();
            let Some(store) = locked.as_mut() else {
                ensure!(
                    settings.effective().next().is_none(),
                    Failure::new("subscription_storage_unavailable")
                );
                return material::local_material(&local, &settings);
            };
            let material = material::compile(
                store,
                &settings,
                &local,
                &[],
                local.owned_bytes(),
                true,
                || Ok(()),
            )?;
            owner.observe_store(store);
            Ok::<_, anyhow::Error>(material)
        })
        .await?;
        match initial {
            Ok(material) => {
                let mut state = service.state.lock().unwrap();
                state.input_rules = material.policy.input_rules();
                state.ready_policy = true;
                service.handle.publish(material.policy);
                state.input_digest = Some(material.material_digest);
                state.index_pin = material.index_pin;
                state.unavailable = None;
            }
            Err(error) => {
                service.state.lock().unwrap().unavailable = Some(Failure::from_error(&error).code);
            }
        }
        // Startup jitter is bounded and cached; no request/GET starts work.
        let owner = service.clone();
        if !frozen {
            tokio::task::spawn_blocking(move || owner.seed_schedule()).await??;
        }
        Ok(service)
    }

    pub fn handle(&self) -> Arc<PolicyHandle> {
        self.handle.clone()
    }
    pub fn config_revision(&self) -> u64 {
        self.state.lock().unwrap().revision
    }
    pub fn validate_candidate(&self, candidate: &ConfigCandidate) -> Result<()> {
        ensure!(
            std::ptr::eq(self, Arc::as_ptr(&candidate.service)),
            Failure::new("revision_conflict")
        );
        self.check_revision(candidate.expected_revision)
    }
    pub fn ready(&self) -> bool {
        self.state.lock().unwrap().ready_policy
    }
    pub fn set_revision(&self, revision: u64) {
        self.state.lock().unwrap().revision = revision;
    }
    pub fn request_close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.notify_waiters();
    }
    pub fn close(&self) {
        self.request_close();
        let _state = self.state.lock().unwrap();
    }
    pub fn explain(&self, name: &Name) -> CheckResult {
        let generation = self.handle.snapshot();
        CheckResult {
            generation: generation.number,
            explanation: generation.policy.explain(name),
        }
    }
    pub fn snapshot(&self) -> SubscriptionState {
        let state = self.state.lock().unwrap();
        let generation = self.handle.snapshot();
        let retained_bytes = self.handle.retained_bytes_with_locals(&[&state.local]);
        SubscriptionState {
            config_revision: state.revision,
            enabled: state.settings.enabled,
            sources: state
                .settings
                .sources
                .iter()
                .map(|source| {
                    let fingerprint = source.identity().expect("validated settings").fingerprint;
                    let record = state
                        .records
                        .iter()
                        .find(|record| record.fingerprint == fingerprint);
                    let ready = state.ready.contains(&fingerprint);
                    SourceStatus {
                        id: source.id.clone(),
                        fingerprint,
                        ready,
                        active: ready
                            && state.settings.enabled
                            && source.enabled
                            && state.ready_policy,
                        input_rules: record.map_or(0, |r| r.rules),
                        bytes: record.map_or(0, |r| r.bytes),
                        last_success: record.map(|r| r.prepared_at),
                        last_attempt: record.and_then(|r| r.status.last_attempt),
                        next_update: record.and_then(|r| r.status.next_update),
                        failures: record.map_or(0, |r| r.status.failures),
                        error: record.and_then(|r| {
                            r.status.last_error.as_ref().map(|code| Failure {
                                code: code.clone(),
                                line: r.status.error_line,
                            })
                        }),
                    }
                })
                .collect(),
            generation: generation.number,
            content_revision: state.content_revision,
            input_rules: state.input_rules,
            index_rules: generation.policy.index_rules(),
            index_bytes: generation.policy.index_bytes(),
            retained_bytes,
            disk_bytes: state.disk_bytes,
            operation: state.operation.clone(),
            recent_operation: state.recent.clone(),
            unavailable_reason: state.unavailable.clone(),
        }
    }

    pub async fn prepare_config(
        self: &Arc<Self>,
        settings: Settings,
        local: Policy,
        expected_revision: u64,
    ) -> Result<ConfigCandidate> {
        settings.validate()?;
        let lease = self.acquire()?;
        self.check_revision(expected_revision)?;
        self.prepare_config_owned(settings, local, expected_revision, lease)
            .await
    }

    /// Raw local rules/files must enter the same worker before allocating their
    /// replacement index; a caller must not compile a parallel local candidate.
    pub async fn prepare_config_from_source(
        self: &Arc<Self>,
        settings: Settings,
        source: crate::policy::LocalPolicySource,
        expected_revision: u64,
    ) -> Result<ConfigCandidate> {
        settings.validate()?;
        let lease = self.acquire()?;
        self.check_revision(expected_revision)?;
        let owner = self.clone();
        let keep = lease.clone();
        let old_local = self.state.lock().unwrap().local.clone();
        if let Some(local) = source.reuse_policy(&old_local) {
            drop(source);
            return self
                .prepare_config_owned(settings, local, expected_revision, lease)
                .await;
        }
        let limits = crate::policy::canonical::Limits {
            max_rules: settings.max_rules,
            max_memory_bytes: settings.max_memory_bytes,
            retained_bytes: self
                .handle
                .retained_bytes_with_locals(&[&old_local])
                .checked_add(material::COORDINATOR_BYTES)
                .context(Failure::new("subscription_memory_limit"))?,
        };
        let local = tokio::task::spawn_blocking(move || {
            let _keep = keep;
            owner.check_revision(expected_revision)?;
            source.compile(limits, || owner.check_open())
        })
        .await??;
        self.prepare_config_owned(settings, local, expected_revision, lease)
            .await
    }

    async fn prepare_config_owned(
        self: &Arc<Self>,
        settings: Settings,
        local: Policy,
        expected_revision: u64,
        lease: Arc<Lease>,
    ) -> Result<ConfigCandidate> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || {
            let _keep = lease.clone();
            owner.check_revision(expected_revision)?;
            let mut locked = owner.store.lock().unwrap();
            let mut pins = Vec::new();
            let mut preserved_input_rules = None;
            let material = if let Some(store) = locked.as_mut() {
                ensure!(
                    store.disk_bytes()? <= settings.max_disk_bytes,
                    Failure::new("subscription_disk_limit")
                );
                for source in settings.effective() {
                    let (record, _) = store
                        .verified_content(&source.identity()?.fingerprint)
                        .map_err(|_| Failure::new("subscription_material_missing"))?;
                    pins.push(store.pin(&record.sha256)?);
                }
                let identity = material::identity(store, &settings, &local)?;
                let same = owner.state.lock().unwrap().input_digest == Some(identity);
                if same {
                    preserved_input_rules = Some(owner.state.lock().unwrap().input_rules);
                    let current = owner.handle.snapshot();
                    ensure!(
                        owner.live_bytes(&local) <= settings.max_memory_bytes,
                        Failure::new("subscription_memory_limit")
                    );
                    Material {
                        policy: current.policy.clone(),
                        material_digest: identity,
                        index_pin: None,
                        content_revision: store.revision(),
                    }
                } else {
                    owner
                        .handle
                        .ensure_available()
                        .map_err(|_| Failure::new("busy"))?;
                    material::compile(
                        store,
                        &settings,
                        &local,
                        &[],
                        owner.live_bytes(&local),
                        false,
                        || owner.check_open(),
                    )?
                }
            } else {
                ensure!(
                    settings.effective().next().is_none(),
                    Failure::new("subscription_storage_unavailable")
                );
                ensure!(
                    owner.live_bytes(&local) <= settings.max_memory_bytes,
                    Failure::new("subscription_memory_limit")
                );
                material::local_material(&local, &settings)?
            };
            drop(locked);
            let input_rules =
                preserved_input_rules.unwrap_or_else(|| material.policy.input_rules());
            Ok(ConfigCandidate {
                service: owner,
                lease,
                settings,
                local,
                expected_revision,
                input_rules,
                material,
                pins,
            })
        })
        .await?
    }

    /// Caller saved Config under its mutation permit. Follow-up disk maintenance
    /// is deferred to the next worker; it cannot roll back that committed Config.
    pub fn publish_config(&self, candidate: ConfigCandidate, new_revision: u64) {
        assert!(std::ptr::eq(self, Arc::as_ptr(&candidate.service)));
        let mut state = self.state.lock().unwrap();
        debug_assert_eq!(state.revision, candidate.expected_revision);
        state.previous = state.settings.fingerprints().expect("validated settings");
        let old_settings = state.settings.clone();
        state.settings = candidate.settings;
        state.local = candidate.local;
        state.revision = new_revision;
        state.input_rules = candidate.input_rules;
        state.ready_policy = true;
        if self.handle.snapshot().policy.publication_digest()
            != candidate.material.policy.publication_digest()
        {
            self.handle.publish(candidate.material.policy);
        }
        state.input_digest = Some(candidate.material.material_digest);
        state.unavailable = None;
        let settings = state.settings.clone();
        for source in settings
            .sources
            .iter()
            .filter(|s| settings.enabled && s.enabled && s.auto_update)
        {
            let fp = source.identity().expect("validated settings").fingerprint;
            let changed = !old_settings.enabled
                || old_settings
                    .sources
                    .iter()
                    .find(|old| old.identity().is_ok_and(|s| s.fingerprint == fp))
                    .is_none_or(|old| {
                        !old.enabled
                            || !old.auto_update
                            || old.update_interval_hours != source.update_interval_hours
                    });
            if changed
                && let Some(record) = state.records.iter_mut().find(|r| r.fingerprint == fp)
                && record.status.failures == 0
            {
                let due = operations::periodic(record.prepared_at, source.update_interval_hours);
                record.status.next_update = Some(if due <= now() {
                    now() + rand::random_range(60..=300)
                } else {
                    due
                });
            }
        }
        if candidate.material.index_pin.is_some() {
            state.index_pin = candidate.material.index_pin;
        }
        drop((candidate.lease, candidate.pins));
    }

    fn check_open(&self) -> Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            Failure::new("subscription_cancelled")
        );
        Ok(())
    }
    fn live_bytes(&self, local: &Policy) -> usize {
        let old_local = self.state.lock().unwrap().local.clone();
        self.handle.retained_bytes_with_locals(&[&old_local, local])
    }
    fn check_revision(&self, revision: u64) -> Result<()> {
        self.check_open()?;
        ensure!(
            self.state.lock().unwrap().revision == revision,
            Failure::new("revision_conflict")
        );
        Ok(())
    }
    fn acquire(&self) -> Result<Arc<Lease>> {
        self.check_open()?;
        let permit = self
            .worker
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::new("busy"))?;
        Ok(Arc::new(Lease { _permit: permit }))
    }
    fn sync_roots(&self, store: &mut Store) -> Result<()> {
        let (current, previous, quota, statuses) = {
            let state = self.state.lock().unwrap();
            (
                state.settings.fingerprints()?,
                state.previous.clone(),
                state.settings.max_disk_bytes,
                state.records.clone(),
            )
        };
        store.reconcile_sync()?;
        store.set_references(current, previous, now())?;
        store.set_quota(quota)?;
        // Failed pre-publication work and schedule-only config changes update
        // cached status immediately, but persist only inside the next gate.
        for record in statuses {
            if store
                .record(&record.fingerprint)
                .is_some_and(|r| r.sha256 == record.sha256 && r.status != record.status)
            {
                store.update_status(&record.fingerprint, record.status)?;
            }
        }
        Ok(())
    }
    fn observe_store(&self, store: &Store) {
        let records = store.records().to_vec();
        let ready = records
            .iter()
            .filter(|r| store.verified_content(&r.fingerprint).is_ok())
            .map(|r| r.fingerprint.clone())
            .collect();
        let disk_bytes = store.disk_bytes();
        let mut state = self.state.lock().unwrap();
        state.records = records;
        state.ready = ready;
        match disk_bytes {
            Ok(bytes) => state.disk_bytes = bytes,
            Err(_) => state.unavailable = Some("subscription_storage_unavailable".into()),
        };
        state.content_revision = store.revision();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn error_code(error: &anyhow::Error) -> &'static str {
    match Failure::from_error(error).code.as_str() {
        "busy" => "busy",
        "revision_conflict" => "revision_conflict",
        "subscription_material_missing" => "subscription_material_missing",
        "subscription_active_source" => "subscription_active_source",
        "subscription_rate_limited" => "subscription_rate_limited",
        "subscription_download" => "subscription_download",
        "subscription_parse" => "subscription_parse",
        "subscription_memory_limit" => "subscription_memory_limit",
        "subscription_disk_limit" => "subscription_disk_limit",
        "subscription_cancelled" => "subscription_cancelled",
        _ => "subscription_storage_unavailable",
    }
}

#[cfg(test)]
mod tests;
