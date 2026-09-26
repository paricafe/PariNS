//! Process-owned update coordination. GitHub checks never run in an API GET or
//! a DNS generation; accepted operations outlive their initiating HTTP request.
pub(super) mod operation;
mod state;
use super::store;
use crate::{
    config::UpdatesConfig,
    update::{
        build_info::BuildInfo,
        contract::{Manifest, Version},
        ipc,
        reader::{DownloadTuple, GithubReader, Latest, ReaderError},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use state::{AppState, Candidate, Plan};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

pub(super) struct Coordinator {
    dir: PathBuf,
    state: Mutex<AppState>,
    writing: tokio::sync::Mutex<()>,
    root: Mutex<Option<ipc::PublicStatus>>,
    unavailable: Option<&'static str>,
    pub frozen: AtomicBool,
    checking: AtomicBool,
    pub notify: Notify,
    pub identity: Option<ipc::InstalledIdentity>,
    pub invocation: Option<String>,
    helper_scoped: bool,
    reader: GithubReader,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn id() -> String {
    rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn jitter(ms: u64) -> u64 {
    ms.saturating_mul(90 + rand::random::<u64>() % 21) / 100
}

struct CheckingGuard<'a>(&'a AtomicBool);
impl Drop for CheckingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn failed_check(state: &mut AppState, error: &ReaderError) {
    state.check.failures = state.check.failures.saturating_add(1);
    let delay = match state.check.failures {
        1 => 300_000,
        2 => 900_000,
        _ => 3_600_000,
    };
    state.check.retry_at_ms = now()
        .saturating_add(jitter(delay))
        .max(error.retry_after_unix.unwrap_or(0).saturating_mul(1000));
    state.check.next_check_at_ms = state.check.retry_at_ms;
    state.check.error = Some(error.code.into());
    state.check.manual = false;
}

fn revalidate_cached_candidate(
    candidate: Option<Candidate>,
    current: &BuildInfo,
    helper_protocol: u32,
) -> Result<Option<Candidate>, ReaderError> {
    let Some(mut candidate) = candidate else {
        return Ok(None);
    };
    let version = Version::parse(&candidate.version)?;
    if version != Version::parse_tag(&candidate.tag)? {
        return Err(ReaderError {
            code: "invalid_cached_candidate",
            retry_after_unix: None,
        });
    }
    if version <= Version::parse(&current.version)? {
        return Ok(None);
    }
    candidate.manual_reason = match candidate.manifest.as_ref() {
        None => Some("manual_upgrade_required".into()),
        Some(manifest) => {
            manifest.validate()?;
            if manifest.version != candidate.version || manifest.tag != candidate.tag {
                return Err(ReaderError {
                    code: "invalid_cached_candidate",
                    retry_after_unix: None,
                });
            }
            manifest
                .check_compatible(current, helper_protocol)
                .err()
                .map(|error| error.to_string())
        }
    };
    // A cached release may still be readable after helper replacement, but a
    // complete binding to this architecture is mandatory for a new apply plan.
    if candidate.manual_reason.is_none() {
        let artifact = candidate
            .manifest
            .as_ref()
            .expect("validated manifest")
            .artifact_for(&current.target)?;
        if !candidate.download.as_ref().is_some_and(|download| {
            download.tag == candidate.tag
                && download.asset_name == artifact.name
                && download.size == artifact.size
                && download.sha256 == artifact.sha256
        }) {
            candidate.manual_reason = Some("release_incomplete".into());
        }
    }
    Ok(Some(candidate))
}

impl Coordinator {
    pub fn open(dir: &Path) -> Self {
        let (mut state, unavailable) = match state::read(dir) {
            Ok(value) => (value.unwrap_or_default(), None),
            Err(_) => (AppState::default(), Some("update_state_unavailable")),
        };
        if state.check.next_check_at_ms <= now() {
            state.check.next_check_at_ms =
                now().saturating_add((60 + rand::random::<u64>() % 241) * 1000);
        }
        let invocation = std::env::var("INVOCATION_ID")
            .ok()
            .filter(|v| ipc::valid_id(v));
        // Store has already canonicalized its directory. Respect DynamicUser's
        // legitimate /var/lib/private mapping, but never bind another managed
        // instance to this machine's global installation transaction.
        let helper_scoped = cfg!(target_os = "linux")
            && invocation.is_some()
            && Path::new("/var/lib/parins-managed")
                .canonicalize()
                .ok()
                .as_deref()
                == Some(dir);
        let root = helper_scoped
            .then(ipc::read_public_status)
            .and_then(Result::ok);
        let frozen = state.commit_intent.is_some()
            || root.as_ref().is_some_and(|s| s.pending_launch.is_some());
        let identity = state::own_identity().ok();
        Self {
            dir: dir.into(),
            state: Mutex::new(state),
            writing: tokio::sync::Mutex::new(()),
            root: Mutex::new(root),
            unavailable,
            frozen: AtomicBool::new(frozen),
            checking: AtomicBool::new(false),
            notify: Notify::new(),
            identity,
            invocation,
            helper_scoped,
            reader: GithubReader::new(),
        }
    }

    pub fn skip_restore(&self) -> bool {
        // Never hold root and app-state locks together. Runtime reconciliation
        // can replace either snapshot independently of read-only API callers.
        let launch = self
            .root
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|r| r.pending_launch.clone());
        if let Some(launch) = launch {
            return launch.skip_cache_restore || self.identity.as_ref() != Some(&launch.identity);
        }
        self.state.lock().unwrap().commit_intent.is_some()
    }

    fn supported(&self) -> Result<(), &'static str> {
        if let Some(reason) = self.unavailable {
            return Err(reason);
        }
        let Some(identity) = &self.identity else {
            return Err("unsupported_installation");
        };
        if !cfg!(target_os = "linux")
            || !identity.build.official_release
            || self.invocation.is_none()
        {
            return Err("unsupported_installation");
        }
        let root = self.root.lock().unwrap();
        let root = root.as_ref().ok_or("helper_unavailable")?;
        if !root.capability.available || root.installed != *identity {
            return Err("helper_unavailable");
        }
        Ok(())
    }

    pub fn view(&self) -> Value {
        let reason = self.supported().err();
        let root = self.root.lock().unwrap().clone();
        let state = self.state.lock().unwrap().clone();
        let candidate = state.candidate.as_ref().map(|candidate| {
            let plan = state.plan.as_ref();
            json!({"version":candidate.version,"tag":candidate.tag,"published_at":candidate.published_at,
                "notes":candidate.notes,"manual_reason":candidate.manual_reason,
                "plan_id":plan.map(|p| &p.plan_id),"expires_at_ms":plan.map(|p|p.expires_at_ms)})
        });
        let active = state.operation.as_ref().filter(|o| !o.finished).map(|op| {
            root.as_ref().and_then(|r|r.active_operation.as_ref()).filter(|o|o.operation_id==op.operation_id)
                .map(|o| {
                    let mut view=serde_json::to_value(o).expect("operation status");
                    if o.phase == ipc::Phase::Staged && matches!(op.phase.as_str(), "preflight" | "committing" | "aborting") {
                        view["phase"]=json!(op.phase);
                    }
                    view
                })
                .unwrap_or_else(||json!({"operation_id":op.operation_id,"phase":op.phase,"version":op.expected_version,
                    "reason":op.reason,"downloaded_bytes":0,"total_bytes":op.download.size}))
        }).or_else(||root.as_ref().and_then(|r|r.active_operation.as_ref()).map(|o|json!(o)));
        let last = state.operation.as_ref().filter(|o| o.finished).map(|op| {
            root.as_ref().and_then(|r| r.last_operation.as_ref())
                .filter(|r| r.operation_id == op.operation_id && r.phase_nonce == op.phase_nonce)
                .map(|r| json!(r))
                .unwrap_or_else(|| json!({"operation_id":op.operation_id,"phase":op.phase,"version":op.expected_version,"reason":op.reason,"downloaded_bytes":0,"total_bytes":op.download.size}))
        }).or_else(|| root.as_ref().and_then(|r|r.last_operation.as_ref()).map(|o|json!(o)));
        json!({"current":BuildInfo::current(),"capability":{"available":reason.is_none(),"reason":reason},
            "check":{"state":if self.checking.load(Ordering::Acquire){"checking"}else if state.check.error.is_some(){"failed"}else if !state.check.validated{"unchecked"}else if state.candidate.is_some(){"available"}else{"up_to_date"},
                "last_check_at_ms":state.check.last_check_at_ms,"last_success_at_ms":state.check.last_success_at_ms,
                "next_check_at_ms":state.check.next_check_at_ms,"retry_at_ms":state.check.retry_at_ms,"error":state.check.error},
            "candidate":candidate,"active_operation":active,"last_operation":last,
            "frozen":self.frozen.load(Ordering::Acquire)})
    }

    // Caller holds `writing` through persistence and publication. No state/root
    // MutexGuard crosses an await. Shutdown must await this bounded write rather
    // than aborting its uninterruptible filesystem worker.
    async fn save_locked(&self, next: AppState) -> anyhow::Result<()> {
        let dir = self.dir.clone();
        let save = next.clone();
        let committed = tokio::task::spawn_blocking(move || state::write(&dir, &save))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
        if let Err(error) = committed {
            // rename may already be visible: retain the durable intent in memory,
            // fail closed and reconcile rather than replaying an uncertain write.
            if let Ok(Some(observed)) = state::read(&self.dir) {
                if observed.commit_intent.is_some() {
                    self.frozen.store(true, Ordering::Release);
                }
                *self.state.lock().unwrap() = observed;
            } else if next.commit_intent.is_some() {
                self.frozen.store(true, Ordering::Release);
            }
            return Err(error);
        }
        *self.state.lock().unwrap() = next;
        Ok(())
    }

    fn persistence_failed_locked(&self) {
        // Disk failure cannot make a stale due time spin. Keep old validated
        // results, expose the error, and back off in memory even if disk is full.
        failed_check(
            &mut self.state.lock().unwrap(),
            &ReaderError {
                code: "update_state_unavailable",
                retry_after_unix: None,
            },
        );
    }

    pub fn due(&self, auto_check: bool) -> bool {
        if self.unavailable.is_some() || self.checking.load(Ordering::Acquire) {
            return false;
        }
        let state = self.state.lock().unwrap();
        (auto_check || state.check.manual)
            && state.check.retry_at_ms <= now()
            && state.check.next_check_at_ms <= now()
    }

    pub async fn request_check(&self) -> Result<(), (&'static str, u64)> {
        if let Some(reason) = self.unavailable {
            return Err((reason, 0));
        }
        let _writer = self.writing.lock().await;
        if self.checking.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut next = self.state.lock().unwrap().clone();
        let retry = next.check.retry_at_ms;
        if retry > now() {
            return Err(("rate_limited", retry));
        }
        next.check.next_check_at_ms = now();
        next.check.manual = true;
        if self.save_locked(next).await.is_err() {
            self.persistence_failed_locked();
            return Err((
                "update_state_unavailable",
                self.state.lock().unwrap().check.retry_at_ms,
            ));
        }
        self.notify.notify_one();
        Ok(())
    }

    pub(super) async fn perform_check(&self, revision: u64, settings: &UpdatesConfig) {
        self.perform_check_with(revision, settings, |old| async move {
            self.fetch_candidate(
                old.check.etag.as_deref(),
                old.check.validated,
                old.candidate.clone(),
            )
            .await
        })
        .await;
    }

    async fn perform_check_with<F, Fut>(&self, revision: u64, settings: &UpdatesConfig, fetch: F)
    where
        F: FnOnce(AppState) -> Fut,
        Fut: std::future::Future<Output = Result<(Option<Candidate>, Option<String>), ReaderError>>,
    {
        let old = {
            let _writer = self.writing.lock().await;
            if !self.due(settings.auto_check) {
                return;
            }
            let old = self.state.lock().unwrap().clone();
            let mut next = old.clone();
            let began = now();
            next.check.last_check_at_ms = Some(began);
            next.check.retry_at_ms = began.saturating_add(60_000);
            next.check.manual = false;
            if self.save_locked(next).await.is_err() {
                self.persistence_failed_locked();
                return;
            }
            self.checking.store(true, Ordering::Release);
            old
        };
        let _checking = CheckingGuard(&self.checking);
        let result = fetch(old).await;
        let supported = self.supported().is_ok();
        let current_sha = self.identity.as_ref().map(|i| i.sha256.clone());
        let _writer = self.writing.lock().await;
        let mut next = self.state.lock().unwrap().clone();
        {
            let s = &mut next;
            match result {
                Ok((candidate, etag)) => {
                    s.candidate = candidate;
                    s.check.etag = etag;
                    s.check.validated = true;
                    s.check.error = None;
                    s.check.failures = 0;
                    s.check.last_success_at_ms = Some(now());
                    s.check.next_check_at_ms = now().saturating_add(jitter(
                        u64::from(settings.check_interval_hours) * 3_600_000,
                    ));
                    if !s.operation.as_ref().is_some_and(|o| !o.finished) {
                        s.plan = if supported
                            && s.candidate
                                .as_ref()
                                .is_some_and(|c| c.manual_reason.is_none())
                        {
                            current_sha.map(|current_sha| Plan {
                                plan_id: id(),
                                expires_at_ms: now() + 1_800_000,
                                config_revision: revision,
                                current_sha256: current_sha,
                            })
                        } else {
                            None
                        };
                    }
                }
                Err(error) => {
                    failed_check(s, &error);
                }
            }
        }
        if self.save_locked(next).await.is_err() {
            self.persistence_failed_locked();
        }
    }

    async fn fetch_candidate(
        &self,
        etag: Option<&str>,
        validated: bool,
        old: Option<Candidate>,
    ) -> Result<(Option<Candidate>, Option<String>), ReaderError> {
        let modified = self.reader.latest(etag).await?;
        let Latest::Modified { release, etag } = modified else {
            return if validated {
                let helper = self
                    .root
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or(0, |r| r.capability.helper_protocol);
                Ok((
                    revalidate_cached_candidate(old, &BuildInfo::current(), helper)?,
                    etag.map(str::to_owned),
                ))
            } else {
                Err(ReaderError {
                    code: "invalid_not_modified",
                    retry_after_unix: None,
                })
            };
        };
        if Version::parse_tag(&release.tag_name)? <= Version::parse(&BuildInfo::current().version)?
        {
            return Ok((None, etag));
        }
        let mut candidate = Candidate {
            version: Version::parse_tag(&release.tag_name)?.to_string(),
            tag: release.tag_name.clone(),
            published_at: release.published_at.clone(),
            notes: release.body.clone().unwrap_or_default(),
            manual_reason: None,
            manifest: None,
            download: None,
        };
        if !release
            .assets
            .iter()
            .any(|a| a.name == "parins-update.json")
        {
            candidate.manual_reason = Some("manual_upgrade_required".into());
            return Ok((Some(candidate), etag));
        }
        let document = self.reader.manifest(&release.tag_name).await?;
        document.validate_release(&release)?;
        let current = BuildInfo::current();
        let helper = self
            .root
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |r| r.capability.helper_protocol);
        if let Err(error) = document.manifest.check_compatible(&current, helper) {
            candidate.manual_reason = Some(error.to_string());
        }
        if let Ok(artifact) = document.manifest.artifact_for(&current.target) {
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == artifact.name)
                .expect("validated release asset");
            candidate.download = Some(DownloadTuple {
                release_id: release.id,
                tag: release.tag_name,
                manifest_sha256: document.sha256,
                asset_id: asset.id,
                asset_name: artifact.name.clone(),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
            });
        }
        candidate.manifest = Some(document.manifest);
        Ok((Some(candidate), etag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicUsize};

    pub(super) fn coordinator(dir: &Path) -> Coordinator {
        Coordinator {
            dir: dir.into(),
            state: Mutex::new(AppState::default()),
            writing: tokio::sync::Mutex::new(()),
            root: Mutex::new(None),
            unavailable: None,
            frozen: AtomicBool::new(false),
            checking: AtomicBool::new(false),
            notify: Notify::new(),
            identity: None,
            invocation: None,
            helper_scoped: false,
            reader: GithubReader::new(),
        }
    }

    #[test]
    fn independent_managed_directory_does_not_read_global_helper_state() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Coordinator::open(dir.path());
        assert!(!coordinator.helper_scoped);
        assert!(coordinator.root.lock().unwrap().is_none());
        assert!(!coordinator.frozen.load(Ordering::Acquire));
        assert!(!coordinator.skip_restore());
    }

    pub(super) fn candidate() -> (Candidate, BuildInfo) {
        use crate::update::contract::{
            Artifact, INSTALL_CONTRACT, REPOSITORY, SUPPORTED_TARGETS, UpgradeMode,
        };
        let mut build = BuildInfo::current();
        build.version = "0.1.4".into();
        build.target = SUPPORTED_TARGETS[0].into();
        build.official_release = true;
        let manifest = Manifest {
            schema: 1,
            repository: REPOSITORY.into(),
            version: "0.1.5".into(),
            tag: "v0.1.5".into(),
            source_commit: "a".repeat(40),
            update_protocol: build.update_protocol,
            install_contract: INSTALL_CONTRACT.into(),
            min_helper_protocol: 1,
            durable_contract_epoch: build.durable_contract_epoch,
            runtime_database_format: build.runtime_database_format,
            cache_snapshot_format: build.cache_snapshot_format,
            cache_semantics: build.cache_semantics,
            upgrade_mode: UpgradeMode::InPlace,
            artifacts: SUPPORTED_TARGETS
                .iter()
                .map(|target| Artifact {
                    target: (*target).into(),
                    name: format!(
                        "parins-v0.1.5-linux-{}.bin",
                        target.split('-').next().unwrap()
                    ),
                    size: 100,
                    sha256: "b".repeat(64),
                })
                .collect(),
        };
        let artifact = &manifest.artifacts[0];
        (
            Candidate {
                version: manifest.version.clone(),
                tag: manifest.tag.clone(),
                published_at: None,
                notes: "notes".into(),
                manual_reason: None,
                download: Some(DownloadTuple {
                    release_id: 1,
                    tag: manifest.tag.clone(),
                    manifest_sha256: "c".repeat(64),
                    asset_id: 2,
                    asset_name: artifact.name.clone(),
                    size: artifact.size,
                    sha256: artifact.sha256.clone(),
                }),
                manifest: Some(manifest),
            },
            build,
        )
    }

    #[test]
    fn not_modified_rechecks_current_identity_and_helper() {
        let (candidate, mut build) = candidate();
        assert!(
            revalidate_cached_candidate(Some(candidate.clone()), &build, 1)
                .unwrap()
                .unwrap()
                .manual_reason
                .is_none()
        );
        assert_eq!(
            revalidate_cached_candidate(Some(candidate.clone()), &build, 0)
                .unwrap()
                .unwrap()
                .manual_reason
                .as_deref(),
            Some("manual_upgrade_required")
        );
        build.durable_contract_epoch += 1;
        assert_eq!(
            revalidate_cached_candidate(Some(candidate.clone()), &build, 1)
                .unwrap()
                .unwrap()
                .manual_reason
                .as_deref(),
            Some("manual_upgrade_required")
        );
        build.version = "0.1.5".into();
        assert!(
            revalidate_cached_candidate(Some(candidate.clone()), &build, 1)
                .unwrap()
                .is_none()
        );
        build.version = "0.1.6".into();
        assert!(
            revalidate_cached_candidate(Some(candidate), &build, 1)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn modified_without_etag_clears_old_validator_durably() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator(dir.path());
        coordinator.state.lock().unwrap().check.etag = Some("old".into());
        coordinator
            .perform_check_with(1, &UpdatesConfig::default(), |_| async { Ok((None, None)) })
            .await;
        assert!(coordinator.state.lock().unwrap().check.etag.is_none());
        assert!(
            state::read(dir.path())
                .unwrap()
                .unwrap()
                .check
                .etag
                .is_none()
        );
        assert!(!coordinator.checking.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn initial_write_failure_never_fetches_and_exposes_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let coordinator = coordinator(&missing);
        let calls = AtomicUsize::new(0);
        coordinator
            .perform_check_with(1, &UpdatesConfig::default(), |_| async {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok((None, None))
            })
            .await;
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            coordinator.view()["check"]["error"],
            "update_state_unavailable"
        );
        assert!(!coordinator.due(true));
        assert!(!coordinator.due(false));
        assert!(!coordinator.checking.load(Ordering::Acquire));
        assert_eq!(
            coordinator.request_check().await.unwrap_err().0,
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn final_write_failure_retains_result_and_does_not_spin() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator(dir.path());
        coordinator.state.lock().unwrap().candidate = Some(candidate().0);
        coordinator.state.lock().unwrap().check.validated = true;
        coordinator
            .perform_check_with(1, &UpdatesConfig::default(), |_| async {
                // Atomic rename onto a directory fails without using permission
                // tricks that accidentally succeed in root-run test environments.
                std::fs::remove_file(dir.path().join("update-state.json")).unwrap();
                std::fs::create_dir(dir.path().join("update-state.json")).unwrap();
                Ok((None, None))
            })
            .await;
        assert!(coordinator.state.lock().unwrap().candidate.is_some());
        assert_eq!(coordinator.view()["check"]["state"], "failed");
        assert!(!coordinator.due(true));
        assert!(!coordinator.checking.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn waiting_manual_request_rechecks_cooldown_and_checking_under_writer() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Arc::new(coordinator(dir.path()));
        let guard = coordinator.writing.lock().await;
        let waiting = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.request_check().await })
        };
        tokio::task::yield_now().await;
        coordinator.state.lock().unwrap().check.retry_at_ms = now() + 60_000;
        drop(guard);
        assert_eq!(waiting.await.unwrap().unwrap_err().0, "rate_limited");
        assert!(!coordinator.state.lock().unwrap().check.manual);
        let guard = coordinator.writing.lock().await;
        let waiting = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.request_check().await })
        };
        tokio::task::yield_now().await;
        coordinator.checking.store(true, Ordering::Release);
        drop(guard);
        assert!(waiting.await.unwrap().is_ok());
        assert!(!coordinator.state.lock().unwrap().check.manual);
    }

    #[tokio::test]
    async fn concurrent_checks_coalesce_and_cancellation_clears_busy() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = Arc::new(coordinator(dir.path()));
        let started = Arc::new(Notify::new());
        let task = {
            let coordinator = coordinator.clone();
            let started = started.clone();
            tokio::spawn(async move {
                coordinator
                    .perform_check_with(1, &UpdatesConfig::default(), |_| async {
                        started.notify_one();
                        std::future::pending::<
                            Result<(Option<Candidate>, Option<String>), ReaderError>,
                        >()
                        .await
                    })
                    .await;
            })
        };
        started.notified().await;
        let calls = AtomicUsize::new(0);
        coordinator
            .perform_check_with(1, &UpdatesConfig::default(), |_| async {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok((None, None))
            })
            .await;
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!coordinator.checking.load(Ordering::Acquire));
        assert!(!coordinator.due(true));
    }
}
