//! Management half of the fixed updater transaction. Root owns installation.
use super::state::Operation;
use super::{Coordinator, id, now, state};
use crate::{
    manage::{Shared, check},
    update::ipc,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::manage) struct Apply {
    plan_id: String,
    expected_version: String,
    config_revision: u64,
}

impl Coordinator {
    async fn change<R>(&self, change: impl FnOnce(&mut state::AppState) -> Result<R>) -> Result<R> {
        ensure!(self.unavailable.is_none(), "update_state_unavailable");
        let _writer = self.writing.lock().await;
        let mut next = self.state.lock().unwrap().clone();
        let result = change(&mut next)?;
        self.save_locked(next).await?;
        Ok(result)
    }

    /// The caller owns the existing mutation permit while accepting this plan.
    pub(in crate::manage) async fn accept(
        &self,
        input: Apply,
        revision: u64,
        ready: bool,
    ) -> Result<String> {
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("unsupported_installation"))?;
        let invocation = self
            .invocation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("unsupported_installation"))?;
        let operation = self
            .change(|s| {
                if let Some(op) = &s.operation {
                    if op.plan_id == input.plan_id {
                        ensure!(
                            op.expected_version == input.expected_version
                                && op.config_revision == input.config_revision,
                            "plan_conflict"
                        );
                        return Ok(op.operation_id.clone());
                    }
                    ensure!(op.finished, "update_in_progress");
                }
                self.supported().map_err(anyhow::Error::msg)?;
                ensure!(!self.frozen.load(Ordering::Acquire), "update_in_progress");
                ensure!(ready, "readiness_failed");
                ensure!(revision == input.config_revision, "config_changed");
                let plan = s
                    .plan
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("plan_expired"))?;
                ensure!(
                    plan.plan_id == input.plan_id && plan.expires_at_ms > now(),
                    "plan_expired"
                );
                ensure!(
                    plan.config_revision == revision && plan.current_sha256 == identity.sha256,
                    "config_changed"
                );
                let candidate = s
                    .candidate
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("release_incomplete"))?;
                ensure!(
                    candidate.version == input.expected_version
                        && candidate.manual_reason.is_none(),
                    "manual_upgrade_required"
                );
                let manifest = candidate
                    .manifest
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("release_incomplete"))?;
                let download = candidate
                    .download
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("release_incomplete"))?;
                let operation_id = id();
                s.operation = Some(Operation {
                    plan_id: input.plan_id,
                    expected_version: input.expected_version,
                    config_revision: revision,
                    operation_id: operation_id.clone(),
                    phase_nonce: id(),
                    invocation_id: invocation.clone(),
                    download,
                    manifest,
                    phase: "accepted".into(),
                    reason: None,
                    finished: false,
                });
                s.plan = None;
                Ok(operation_id)
            })
            .await?;
        self.notify.notify_waiters();
        Ok(operation)
    }

    async fn inbox(&self, operation: &Operation, request: ipc::Request) -> Result<()> {
        let request = ipc::Inbox {
            schema: 1,
            operation_id: operation.operation_id.clone(),
            phase_nonce: operation.phase_nonce.clone(),
            request,
        };
        let bytes = serde_json::to_vec(&request)?;
        ipc::Inbox::parse(&bytes)?;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || state::atomic(&dir, ipc::INBOX_NAME, &bytes)).await??;
        Ok(())
    }

    async fn set_phase(
        &self,
        operation: &Operation,
        phase: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.change(|s| {
            let op = s
                .operation
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("operation_missing"))?;
            ensure!(
                op.operation_id == operation.operation_id,
                "operation_changed"
            );
            op.phase = phase.into();
            op.reason = reason.map(str::to_owned);
            Ok(())
        })
        .await
    }

    async fn abort_before_commit(&self, op: &Operation, reason: &'static str) {
        // Do not clear a durable intent without root's irreversible abort fence.
        let _ = self.set_phase(op, "aborting", Some(reason)).await;
        let _ = self.inbox(op, ipc::Request::Abort {}).await;
    }
}

fn readiness(manager: &crate::manage::runtime::Manager) -> bool {
    let health = manager.services.health.snapshot();
    let storage = manager.services.status();
    manager.resolver().is_some()
        && health.ready
        && storage.health == "healthy"
        && !storage.capacity_pending
        && storage.configured_revision == storage.applied_revision
}

pub(in crate::manage) async fn accept(shared: &Shared, input: Apply) -> Result<String> {
    let manager = shared.manager.lock().await;
    let revision = manager.saved.as_ref().map_or(0, |s| s.revision);
    manager
        .updates
        .accept(input, revision, readiness(&manager))
        .await
}

/// Root status polling is separate from network checks, so a slow GitHub check
/// cannot stall the one-second launch health receipt.
pub(in crate::manage) async fn run(
    shared: Arc<Shared>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let coordinator = shared.manager.lock().await.updates.clone();
    let mut launched = None;
    let mut sequence = 0u64;
    let mut last_receipt = None;
    let mut job: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        if *stop.borrow() {
            break;
        }
        let status = if coordinator.helper_scoped {
            tokio::task::spawn_blocking(ipc::read_public_status)
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        *coordinator.root.lock().unwrap() = status.clone();
        if let Ok(_permit) = shared.mutation.clone().try_acquire_owned() {
            let _ = reconcile(&coordinator, status.as_ref()).await;
        }
        if let Some(status) = &status
            && last_receipt
                .is_none_or(|at: std::time::Instant| at.elapsed() >= Duration::from_secs(1))
        {
            let _ = heartbeat(&shared, &coordinator, status, &mut sequence).await;
            last_receipt = Some(std::time::Instant::now());
        }
        if job.as_ref().is_some_and(|task| task.is_finished())
            && let Some(task) = job.take()
        {
            let _ = task.await;
        }
        let op = coordinator
            .state
            .lock()
            .unwrap()
            .operation
            .clone()
            .filter(|o| !o.finished);
        if job.is_none()
            && let Some(op) = op
            && launched.as_ref() != Some(&op.operation_id)
        {
            launched = Some(op.operation_id.clone());
            if op.invocation_id == coordinator.invocation.as_deref().unwrap_or("")
                && op.phase == "accepted"
            {
                let shared = shared.clone();
                let coordinator = coordinator.clone();
                let stopping = stop.clone();
                job = Some(tokio::spawn(async move {
                    execute(shared, coordinator, op, stopping).await;
                }));
            } else if op.invocation_id != coordinator.invocation.as_deref().unwrap_or("")
                || !coordinator.frozen.load(Ordering::Acquire)
            {
                // A prior invocation may have persisted intent before writing
                // Commit. Ask root to fence precommit work; Abort leaves an
                // already committed operation alone. Only reconcile may thaw.
                coordinator.abort_before_commit(&op, "interrupted").await;
            }
        }
        tokio::select! {
            _ = stop.changed() => {},
            _ = coordinator.notify.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
    // The worker observes stop before final commit. Do not abort a blocking
    // persistence call and release its transaction permit prematurely.
    if let Some(job) = job {
        let _ = job.await;
    }
}

async fn reconcile(coordinator: &Coordinator, root: Option<&ipc::PublicStatus>) -> Result<()> {
    let Some(root) = root else {
        return Ok(());
    };
    let op = coordinator.state.lock().unwrap().operation.clone();
    if op.as_ref().is_none_or(|op| op.finished)
        && coordinator.state.lock().unwrap().commit_intent.is_none()
    {
        // Initial trusted installation has no app-side operation mapping.
        // Only the root anchor can complete that pending-launch freeze.
        if root.pending_launch.is_none()
            && root.active_operation.is_none()
            && (root.capability.available
                || root.capability.reason.as_deref() == Some("unsupported_installation"))
            && coordinator.identity.as_ref() == Some(&root.installed)
        {
            coordinator.frozen.store(false, Ordering::Release);
        }
        return Ok(());
    }
    let Some(op) = op else {
        return Ok(());
    };
    // A manually installed process can supersede a plan accepted by the old
    // invocation before root ever staged it. Root rejects that old invocation;
    // without a commit intent there is no installation outcome to recover.
    if coordinator
        .invocation
        .as_ref()
        .is_some_and(|id| id != &op.invocation_id)
        && root.active_operation.is_none()
        && root.pending_launch.is_none()
        && root.capability.reason.as_deref() != Some("installation_pending")
        && coordinator.identity.as_ref() == Some(&root.installed)
        && coordinator.state.lock().unwrap().commit_intent.is_none()
        && !root.last_operation.as_ref().is_some_and(|last| {
            last.operation_id == op.operation_id && last.phase_nonce == op.phase_nonce
        })
    {
        coordinator
            .change(|state| {
                ensure!(state.commit_intent.is_none(), "update_in_progress");
                let current = state
                    .operation
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("operation_missing"))?;
                ensure!(current.operation_id == op.operation_id, "operation_changed");
                current.finished = true;
                current.phase = "failed".into();
                current.reason = Some("interrupted".into());
                Ok(())
            })
            .await?;
        coordinator.frozen.store(false, Ordering::Release);
        return Ok(());
    }
    let Some(terminal) = root.last_operation.as_ref().filter(|t| {
        t.operation_id == op.operation_id && t.phase_nonce == op.phase_nonce && t.phase.terminal()
    }) else {
        return Ok(());
    };
    if terminal.phase == ipc::Phase::ManualRequired
        || root.pending_launch.is_some()
        || root.active_operation.is_some()
        || coordinator.identity.as_ref() != Some(&root.installed)
    {
        return Ok(());
    }
    let cleared = coordinator
        .change(|s| {
            if let Some(current) = s
                .operation
                .as_mut()
                .filter(|o| o.operation_id == op.operation_id)
            {
                current.finished = true;
                current.phase = serde_json::to_value(terminal.phase)?
                    .as_str()
                    .unwrap_or("failed")
                    .into();
                current.reason = terminal.reason.clone();
                s.commit_intent = None;
                return Ok(true);
            }
            Ok(false)
        })
        .await?;
    if cleared {
        coordinator.frozen.store(false, Ordering::Release);
    }
    Ok(())
}

async fn heartbeat(
    shared: &Shared,
    coordinator: &Coordinator,
    root: &ipc::PublicStatus,
    sequence: &mut u64,
) -> Result<()> {
    let Some(pending) = &root.pending_launch else {
        return Ok(());
    };
    coordinator.frozen.store(true, Ordering::Release);
    let identity = coordinator
        .identity
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("identity unavailable"))?;
    let invocation = coordinator
        .invocation
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("invocation unavailable"))?;
    ensure!(identity == &pending.identity, "launch_identity_mismatch");
    let manager = shared.manager.lock().await;
    let health = manager.services.health.snapshot();
    let storage = manager.services.status();
    *sequence = sequence
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("health sequence exhausted"))?;
    let receipt = ipc::Health {
        operation_id: pending.operation_id.clone(),
        launch_nonce: pending.launch_nonce.clone(),
        identity: identity.clone(),
        config_revision: manager.saved.as_ref().map_or(0, |s| s.revision),
        dns_generation: health.generation,
        dns_ready: manager.resolver().is_some() && health.ready,
        storage_healthy: storage.health == "healthy",
        storage_settings_applied: !storage.capacity_pending
            && storage.configured_revision == storage.applied_revision,
        sample_seq: *sequence,
        pid: std::process::id(),
        invocation_id: invocation.clone(),
    };
    drop(manager);
    let bytes = serde_json::to_vec(&receipt)?;
    tokio::task::spawn_blocking(move || {
        state::atomic(
            std::path::Path::new("/run/parins-managed"),
            "update-health.json",
            &bytes,
        )
    })
    .await??;
    if pending.phase == ipc::Phase::AwaitingReadiness
        && root.capability.reason.as_deref() != Some("installation_pending")
    {
        let inbox = ipc::Inbox {
            schema: 1,
            operation_id: pending.operation_id.clone(),
            phase_nonce: pending.phase_nonce.clone(),
            request: ipc::Request::VerifyRecovery {},
        };
        let bytes = serde_json::to_vec(&inbox)?;
        let dir = coordinator.dir.clone();
        // Do not overwrite a request already awaiting root consumption.
        tokio::task::spawn_blocking(move || {
            if super::store::checked_open(&dir.join(ipc::INBOX_NAME), false)?.is_none() {
                state::atomic(&dir, ipc::INBOX_NAME, &bytes)?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }
    Ok(())
}

async fn execute(
    shared: Arc<Shared>,
    coordinator: Arc<Coordinator>,
    op: Operation,
    stop: tokio::sync::watch::Receiver<bool>,
) {
    if let Err(reason) = stage_and_commit(&shared, &coordinator, &op, &stop).await {
        coordinator.abort_before_commit(&op, reason).await;
    }
}

async fn stage_and_commit(
    shared: &Shared,
    coordinator: &Coordinator,
    op: &Operation,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> Result<(), &'static str> {
    let identity = coordinator
        .identity
        .as_ref()
        .ok_or("unsupported_installation")?;
    coordinator
        .inbox(
            op,
            ipc::Request::Stage {
                download: op.download.clone(),
                current_sha256: identity.sha256.clone(),
                invocation_id: op.invocation_id.clone(),
                config_revision: op.config_revision,
            },
        )
        .await
        .map_err(|_| "helper_unavailable")?;
    coordinator
        .set_phase(op, "downloading", None)
        .await
        .map_err(|_| "update_state_unavailable")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(420);
    loop {
        if *stop.borrow() {
            return Err("interrupted");
        }
        let root = coordinator.root.lock().unwrap().clone();
        if let Some(root) = root {
            if let Some(status) = root
                .active_operation
                .filter(|s| s.operation_id == op.operation_id && s.phase_nonce == op.phase_nonce)
                && status.phase == ipc::Phase::Staged
            {
                break;
            }
            if root
                .last_operation
                .is_some_and(|s| s.operation_id == op.operation_id && s.phase.terminal())
            {
                return Err("verification_failed");
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("helper_unavailable");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    coordinator
        .set_phase(op, "preflight", None)
        .await
        .map_err(|_| "update_state_unavailable")?;
    let baseline = read_material(coordinator, shared.address)
        .await
        .map_err(|_| "preflight_failed")?;
    let checked = candidate_check(coordinator, shared.address, op)
        .await
        .map_err(|_| "preflight_failed")?;
    if checked.check != baseline || !op.manifest.matches_build(&checked.build_info) {
        return Err("preflight_failed");
    }
    let _permit = shared
        .mutation
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "interrupted")?;
    if *stop.borrow() {
        return Err("interrupted");
    }
    let manager = shared.manager.lock().await;
    if manager.saved.as_ref().map(|s| s.revision) != Some(op.config_revision)
        || !readiness(&manager)
    {
        return Err("config_changed");
    }
    let root = tokio::task::spawn_blocking(ipc::read_public_status)
        .await
        .map_err(|_| "helper_unavailable")?
        .map_err(|_| "helper_unavailable")?;
    if root.installed != *identity
        || !root.active_operation.is_some_and(|s| {
            s.operation_id == op.operation_id
                && s.phase_nonce == op.phase_nonce
                && s.phase == ipc::Phase::Staged
        })
    {
        return Err("verification_failed");
    }
    if read_material(coordinator, shared.address)
        .await
        .map_err(|_| "preflight_failed")?
        != baseline
    {
        return Err("config_changed");
    }
    coordinator
        .change(|s| {
            let current = s
                .operation
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("operation_missing"))?;
            ensure!(
                current.operation_id == op.operation_id && !current.finished,
                "operation_changed"
            );
            s.commit_intent = Some(op.operation_id.clone());
            current.phase = "committing".into();
            Ok(())
        })
        .await
        .map_err(|_| "update_state_unavailable")?;
    coordinator.frozen.store(true, Ordering::Release);
    coordinator
        .inbox(
            op,
            ipc::Request::Commit {
                invocation_id: op.invocation_id.clone(),
                config_revision: op.config_revision,
            },
        )
        .await
        .map_err(|_| "helper_unavailable")?;
    // Return drops Manager and original permit before helper waits for SIGTERM.
    Ok(())
}

async fn read_material(
    coordinator: &Coordinator,
    address: std::net::SocketAddr,
) -> Result<check::CheckReport> {
    #[cfg(target_os = "linux")]
    let executable = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe()?;
    Ok(run_preflight(coordinator, address, executable).await?.check)
}

async fn candidate_check(
    coordinator: &Coordinator,
    address: std::net::SocketAddr,
    op: &Operation,
) -> Result<ipc::ManagedCheckOutput> {
    run_preflight(coordinator, address, ipc::candidate_path(&op.operation_id)?).await
}

async fn run_preflight(
    coordinator: &Coordinator,
    address: std::net::SocketAddr,
    candidate: std::path::PathBuf,
) -> Result<ipc::ManagedCheckOutput> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(candidate)
        .env_clear()
        .arg("--manage")
        .arg("--check")
        .arg("--state-dir")
        .arg(&coordinator.dir)
        .arg("--web-listen")
        .arg(address.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("preflight stdout"))?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut output = Vec::new();
        (&mut stdout).take(65537).read_to_end(&mut output).await?;
        ensure!(output.len() <= 65536, "preflight output exceeded");
        ensure!(child.wait().await?.success(), "preflight failed");
        Ok::<_, anyhow::Error>(serde_json::from_slice(&output)?)
    })
    .await;
    match result {
        Ok(Ok(report)) => Ok(report),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("preflight failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &std::path::Path) -> (Coordinator, Operation, ipc::PublicStatus) {
        let mut coordinator = super::super::tests::coordinator(dir);
        let (candidate, build) = super::super::tests::candidate();
        let identity = ipc::InstalledIdentity {
            build,
            sha256: "d".repeat(64),
        };
        coordinator.identity = Some(identity.clone());
        coordinator.invocation = Some("e".repeat(32));
        let operation = Operation {
            plan_id: "a".repeat(32),
            expected_version: candidate.version.clone(),
            config_revision: 2,
            operation_id: "b".repeat(32),
            phase_nonce: "c".repeat(32),
            invocation_id: "f".repeat(32),
            download: candidate.download.unwrap(),
            manifest: candidate.manifest.unwrap(),
            phase: "committing".into(),
            reason: None,
            finished: false,
        };
        coordinator.state.lock().unwrap().operation = Some(operation.clone());
        coordinator.state.lock().unwrap().commit_intent = Some(operation.operation_id.clone());
        coordinator.frozen.store(true, Ordering::Release);
        let terminal = ipc::OperationStatus {
            operation_id: operation.operation_id.clone(),
            phase_nonce: operation.phase_nonce.clone(),
            phase: ipc::Phase::Aborted,
            version: operation.expected_version.clone(),
            reason: Some("interrupted".into()),
            updated_at_ms: now(),
            downloaded_bytes: 0,
            total_bytes: operation.download.size,
        };
        let root = ipc::PublicStatus {
            schema: 1,
            installed: identity,
            capability: ipc::Capability {
                available: true,
                reason: None,
                checked_at_ms: now(),
                helper_protocol: 1,
                install_contract: crate::update::contract::INSTALL_CONTRACT.into(),
            },
            active_operation: None,
            last_operation: Some(terminal),
            pending_launch: None,
        };
        (coordinator, operation, root)
    }

    #[tokio::test]
    async fn post_rename_error_retains_accepted_operation_for_get_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, op, _) = fixture(dir.path());
        let mut next = coordinator.state.lock().unwrap().clone();
        next.commit_intent = None;
        next.operation.as_mut().unwrap().phase = "accepted".into();
        coordinator.state.lock().unwrap().operation = None;
        coordinator.state.lock().unwrap().commit_intent = None;
        *state::FAIL_DIRECTORY_SYNC.lock().unwrap() = Some(dir.path().into());
        let result = {
            let _writer = coordinator.writing.lock().await;
            coordinator.save_locked(next).await
        };
        *state::FAIL_DIRECTORY_SYNC.lock().unwrap() = None;
        let error = result.unwrap_err();
        assert_eq!(
            crate::manage::update_apply_error(error).0,
            http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            coordinator.view()["active_operation"]["operation_id"],
            op.operation_id
        );
        assert_eq!(coordinator.view()["active_operation"]["phase"], "accepted");
        assert_eq!(
            state::read(dir.path())
                .unwrap()
                .unwrap()
                .operation
                .unwrap()
                .operation_id,
            op.operation_id
        );
    }

    #[tokio::test]
    async fn completed_operation_history_does_not_keep_installer_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, _, mut root) = fixture(dir.path());
        coordinator.state.lock().unwrap().commit_intent = None;
        coordinator
            .state
            .lock()
            .unwrap()
            .operation
            .as_mut()
            .unwrap()
            .finished = true;
        root.capability.available = false;
        root.capability.reason = Some("installation_pending".into());
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.capability.available = true;
        root.capability.reason = None;
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(!coordinator.frozen.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn superseded_uncommitted_plan_finishes_only_after_installation_settles() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, op, mut root) = fixture(dir.path());
        coordinator.state.lock().unwrap().commit_intent = None;
        root.last_operation = None;
        root.capability.reason = Some("installation_pending".into());
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(
            !coordinator
                .state
                .lock()
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .finished
        );
        root.capability.reason = None;
        root.installed.sha256 = "0".repeat(64);
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(
            !coordinator
                .state
                .lock()
                .unwrap()
                .operation
                .as_ref()
                .unwrap()
                .finished
        );
        root.installed = coordinator.identity.clone().unwrap();
        reconcile(&coordinator, Some(&root)).await.unwrap();
        let saved = state::read(dir.path()).unwrap().unwrap();
        let done = saved.operation.unwrap();
        assert_eq!(done.operation_id, op.operation_id);
        assert!(done.finished);
        assert_eq!(done.reason.as_deref(), Some("interrupted"));
        assert!(!coordinator.frozen.load(Ordering::Acquire));
        assert!(!dir.path().join(ipc::INBOX_NAME).exists());
        // A root result from an earlier attempt must not hide this local
        // precommit interruption, or a waiting UI could never reconcile it.
        let (_, _, mut old_root) = fixture(dir.path());
        old_root.last_operation.as_mut().unwrap().operation_id = "0".repeat(32);
        *coordinator.root.lock().unwrap() = Some(old_root);
        assert_eq!(
            coordinator.view()["last_operation"]["operation_id"],
            done.operation_id
        );
        assert_eq!(
            coordinator.view()["last_operation"]["reason"],
            "interrupted"
        );
    }

    #[tokio::test]
    async fn consumed_plan_is_idempotent_even_during_freeze_and_conflicts_do_not_reinstall() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, op, _) = fixture(dir.path());
        let request = || Apply {
            plan_id: op.plan_id.clone(),
            expected_version: op.expected_version.clone(),
            config_revision: op.config_revision,
        };
        assert_eq!(
            coordinator.accept(request(), 999, false).await.unwrap(),
            op.operation_id
        );
        assert_eq!(
            coordinator.accept(request(), 999, false).await.unwrap(),
            op.operation_id
        );
        let mut conflict = request();
        conflict.expected_version = "9.0.0".into();
        assert_eq!(
            coordinator
                .accept(conflict, 2, true)
                .await
                .unwrap_err()
                .to_string(),
            "plan_conflict"
        );
        assert!(coordinator.frozen.load(Ordering::Acquire));
        assert!(!dir.path().join(ipc::INBOX_NAME).exists());
        let saved = state::read(dir.path()).unwrap().unwrap();
        assert_eq!(saved.operation.unwrap().operation_id, op.operation_id);
        assert_eq!(saved.commit_intent, Some(op.operation_id));
    }

    #[tokio::test]
    async fn restarted_intent_drives_root_abort_without_replaying_commit() {
        use crate::manage::{Active, Snapshot, auth_budget, runtime::Manager, store::Store};
        use std::sync::Mutex;
        for phase in [ipc::Phase::Staged, ipc::Phase::Committing] {
            let dir = tempfile::tempdir().unwrap();
            let (original, op, mut root) = fixture(dir.path());
            original.change(|_| Ok(())).await.unwrap();
            let mut restarted = Coordinator::open(dir.path());
            restarted.identity = original.identity.clone();
            restarted.invocation = original.invocation.clone();
            assert!(restarted.frozen.load(Ordering::Acquire));
            root.active_operation = root.last_operation.take();
            root.active_operation.as_mut().unwrap().phase = phase;
            assert!(!dir.path().join(ipc::INBOX_NAME).exists());
            let coordinator = Arc::new(restarted);
            let active = Arc::new(Mutex::new(Active {
                snapshot: Arc::new(Snapshot::initial()),
                sessions: vec![],
            }));
            let address = "127.0.0.1:3000".parse().unwrap();
            let mut manager = Manager::open(
                Store::open(&dir.path().join("managed")).unwrap(),
                address,
                active.clone(),
            )
            .await
            .unwrap();
            manager.updates = coordinator.clone();
            let shared = Arc::new(Shared {
                filter_tasks: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
                manager: Arc::new(tokio::sync::Mutex::new(manager)),
                active,
                auth: auth_budget::Budget::new(),
                mutation: Arc::new(tokio::sync::Semaphore::new(1)),
                address,
            });
            let (stop, stopping) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(run(shared.clone(), stopping));
            let generated = tokio::time::timeout(Duration::from_secs(2), async {
                while !dir.path().join(ipc::INBOX_NAME).exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            stop.send(true).unwrap();
            task.await.unwrap();
            shared.manager.lock().await.stop().await;
            assert!(
                generated.is_ok(),
                "restarted frozen intent never requested root arbitration"
            );
            let inbox = std::fs::read(dir.path().join(ipc::INBOX_NAME)).unwrap();
            let request = ipc::Inbox::parse(&inbox).unwrap();
            assert!(matches!(request.request, ipc::Request::Abort {}));
            assert_eq!(request.operation_id, op.operation_id);
            assert_eq!(request.phase_nonce, op.phase_nonce);
            assert!(coordinator.frozen.load(Ordering::Acquire));
            assert!(
                state::read(dir.path())
                    .unwrap()
                    .unwrap()
                    .commit_intent
                    .is_some()
            );

            let root_dir = tempfile::tempdir().unwrap();
            let mut fence =
                crate::update::executor::consume_abort_fixture(root_dir.path(), &root, &inbox)
                    .unwrap();
            if phase == ipc::Phase::Committing {
                assert_eq!(fence.active_operation.as_ref().unwrap().phase, phase);
                assert!(fence.last_operation.is_none());
                reconcile(&coordinator, Some(&fence)).await.unwrap();
                assert!(coordinator.frozen.load(Ordering::Acquire));
                assert!(
                    state::read(dir.path())
                        .unwrap()
                        .unwrap()
                        .commit_intent
                        .is_some()
                );
            } else {
                assert_eq!(
                    fence.last_operation.as_ref().unwrap().phase,
                    ipc::Phase::Aborted
                );
                fence.installed.sha256 = "0".repeat(64);
                reconcile(&coordinator, Some(&fence)).await.unwrap();
                assert!(coordinator.frozen.load(Ordering::Acquire));
                fence.installed = coordinator.identity.clone().unwrap();
                reconcile(&coordinator, Some(&fence)).await.unwrap();
                assert!(!coordinator.frozen.load(Ordering::Acquire));
                let saved = state::read(dir.path()).unwrap().unwrap();
                assert!(saved.commit_intent.is_none());
                assert!(saved.operation.unwrap().finished);
            }
        }
    }

    #[tokio::test]
    async fn restart_can_resolve_old_intent_only_with_matching_root_fence_and_current_elf() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, op, mut root) = fixture(dir.path());
        // The original invocation has disappeared; that is not a permanent lock.
        assert_ne!(
            coordinator.invocation.as_deref(),
            Some(op.invocation_id.as_str())
        );
        reconcile(&coordinator, None).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.last_operation.as_mut().unwrap().phase = ipc::Phase::ManualRequired;
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.last_operation.as_mut().unwrap().phase = ipc::Phase::Aborted;
        root.installed.sha256 = "0".repeat(64);
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.installed = coordinator.identity.clone().unwrap();
        root.last_operation.as_mut().unwrap().phase_nonce = "0".repeat(32);
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.last_operation.as_mut().unwrap().phase_nonce = op.phase_nonce;
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(!coordinator.frozen.load(Ordering::Acquire));
        let saved = state::read(dir.path()).unwrap().unwrap();
        assert!(saved.commit_intent.is_none());
        assert!(saved.operation.unwrap().finished);
        let before = std::fs::read(dir.path().join("update-state.json")).unwrap();
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("update-state.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn pending_launch_and_missing_descriptor_never_unlock_or_restore_old_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, op, mut root) = fixture(dir.path());
        assert!(coordinator.skip_restore());
        root.pending_launch = Some(ipc::PendingLaunch {
            operation_id: op.operation_id,
            launch_nonce: "1".repeat(32),
            identity: root.installed.clone(),
            skip_cache_restore: true,
            initialized: true,
            phase: ipc::Phase::AwaitingReadiness,
            phase_nonce: op.phase_nonce,
        });
        *coordinator.root.lock().unwrap() = Some(root.clone());
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        assert!(coordinator.skip_restore());
    }

    #[tokio::test]
    async fn installer_launch_freeze_needs_confirmed_identity_and_completed_registration() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, _, mut root) = fixture(dir.path());
        *coordinator.state.lock().unwrap() = state::AppState::default();
        root.capability.available = false;
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(coordinator.frozen.load(Ordering::Acquire));
        root.capability.available = true;
        reconcile(&coordinator, Some(&root)).await.unwrap();
        assert!(!coordinator.frozen.load(Ordering::Acquire));
        assert!(!dir.path().join("update-state.json").exists());
    }
}
