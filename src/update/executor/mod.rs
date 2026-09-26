//! Fixed-purpose root update executor. No command, path, URL or service selector
//! is accepted from the application. It never opens business state or executes a
//! candidate as root.
mod fs;
mod journal;

use crate::update::{
    build_info::BuildInfo,
    contract::{HELPER_PROTOCOL, MAX_BINARY_BYTES},
    ipc::*,
    reader::GithubReader,
};
use anyhow::{Context, Result, bail, ensure};
use fs::Dir;
use journal::{Installation, Journal, Operation, Readiness, now_ms};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

const UNIT: &str = "parins-managed.service";
const ROOT_STATE: &str = "/var/lib/parins-updater";
const LIVE_DIR: &str = "/opt/parins-managed";
const STATE: &str = "/var/lib/parins-managed";
const PRIVATE_STATE: &str = "/var/lib/private/parins-managed";

fn failure_code(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::StorageFull)
        || error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOSPC)
    {
        return "insufficient_space";
    }
    if let Some(error) = error.downcast_ref::<crate::update::reader::ReaderError>() {
        return error.code;
    }
    if let Some(error) = error.downcast_ref::<crate::update::contract::ContractError>() {
        return match error {
            crate::update::contract::ContractError::ManualRequired => "manual_upgrade_required",
            crate::update::contract::ContractError::ReleaseIncomplete => "release_incomplete",
            crate::update::contract::ContractError::ReleaseChanged
            | crate::update::contract::ContractError::NotNewer => "release_changed",
            _ => "verification_failed",
        };
    }
    match error.to_string().as_str() {
        "insufficient_space" => "insufficient_space",
        "rate_limited" => "rate_limited",
        "release_changed" => "release_changed",
        _ => "verification_failed",
    }
}

pub async fn run_cli() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        (args.len() == 1
            && matches!(
                args[0].as_str(),
                "run"
                    | "recover"
                    | "boot-recover"
                    | "register"
                    | "install-check"
                    | "install-restore"
                    | "rebuild-status"
            ))
            || (args.len() == 2
                && matches!(
                    args[0].as_str(),
                    "install-verify" | "install-rollback-check"
                )
                && valid_id(&args[1])),
        "invalid fixed updater operation"
    );
    ensure!(cfg!(target_os = "linux"), "updater requires Linux systemd");
    let handed_off = matches!(
        args[0].as_str(),
        "register"
            | "install-check"
            | "install-restore"
            | "install-rollback-check"
            | "rebuild-status"
    );
    let mut executor = Executor::open(handed_off)?;
    match args[0].as_str() {
        "register" => executor.register().await,
        "run" => executor.run().await,
        "recover" => executor.recover(false).await,
        "boot-recover" => executor.recover(true).await,
        "install-check" => executor.install_check(),
        "install-restore" => executor.install_restore(),
        "install-verify" => executor.install_verify(&args[1]).await,
        "install-rollback-check" => executor.install_match(&args[1]).map(|_| ()),
        "rebuild-status" => {
            let journal = Journal::load(&executor.private)?;
            ensure!(
                executor.actual_hash()? == journal.installed.sha256,
                "installed identity differs"
            );
            executor.publish(&journal)
        }
        _ => bail!("unknown updater operation"),
    }
}

struct Executor {
    root: Dir,
    private: Dir,
    live: Dir,
    staging: Dir,
    _lock: std::fs::File,
}

impl Executor {
    fn open(register: bool) -> Result<Self> {
        let root = Dir::open(Path::new(ROOT_STATE), true)?;
        let private = root.child("private")?;
        ensure!(
            rustix::fs::fstat(&private.fd)?.st_mode & 0o077 == 0,
            "updater private directory must be 0700"
        );
        let lock = rustix::fs::openat(
            &private.fd,
            "lock",
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        let stat = rustix::fs::fstat(&lock)?;
        ensure!(
            stat.st_uid == 0
                && stat.st_nlink == 1
                && stat.st_mode & 0o077 == 0
                && rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile,
            "invalid installer lock"
        );
        let lock = if register {
            // Trusted installer passes its held lock on stdin. dup shares the
            // open-file-description, avoiding a second competing flock while
            // retaining CLOEXEC before any systemctl child.
            let inherited = rustix::io::dup(std::io::stdin())?;
            rustix::io::fcntl_setfd(std::io::stdin(), rustix::io::FdFlags::CLOEXEC)?;
            let actual = rustix::fs::fstat(&inherited)?;
            ensure!(
                actual.st_ino == stat.st_ino && actual.st_dev == stat.st_dev,
                "installer lock was not handed off"
            );
            rustix::io::fcntl_setfd(&inherited, rustix::io::FdFlags::CLOEXEC)?;
            inherited
        } else {
            lock
        };
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("installer or updater is already active")?;
        let live = Dir::open(Path::new(LIVE_DIR), true)?;
        let staging = live.create_dir(".update", 0o755)?;
        Ok(Self {
            root,
            private,
            live,
            staging,
            _lock: lock.into(),
        })
    }
    fn publish(&self, journal: &Journal) -> Result<()> {
        let bytes = serde_json::to_vec(&journal.status())?;
        ensure!(bytes.len() <= STATUS_LIMIT, "oversized public status");
        self.root.replace("status.json", &bytes, 0o644)
    }
    fn persist(&self, journal: &Journal) -> Result<()> {
        journal.save(&self.private)?;
        self.publish(journal)
    }
    fn phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        let op = journal.operation.as_mut().context("missing operation")?;
        op.status.phase = phase;
        op.status.updated_at_ms = now_ms();
        self.persist(journal)
    }
    fn finish(&self, journal: &mut Journal, phase: Phase, reason: Option<&str>) -> Result<()> {
        journal.finish(phase, reason);
        self.persist(journal)
    }
    async fn register(&mut self) -> Result<()> {
        validate_units().await?;
        let build: BuildInfo =
            serde_json::from_slice(&self.private.read("install-build-info.json", 8192, true)?)?;
        let own = BuildInfo::current();
        ensure!(
            build == own,
            "helper and installation build identities differ"
        );
        fs::validate_elf(
            self.live.file("parins", MAX_BINARY_BYTES, true)?,
            &build.target,
        )?;
        let identity = InstalledIdentity {
            build,
            sha256: fs::sha256(self.live.file("parins", MAX_BINARY_BYTES, true)?)?,
        };
        if self.private.exists("journal.json")? {
            let previous = Journal::load(&self.private)?;
            ensure!(
                previous.installation_pending.is_none()
                    && previous
                        .operation
                        .as_ref()
                        .is_none_or(|o| o.status.phase.terminal()),
                "unfinished update; manual recovery required"
            );
        }
        let preflight: Option<ManagedCheckOutput> =
            serde_json::from_slice(&self.private.read("install-preflight.json", 8192, true)?)?;
        if let Some(report) = &preflight {
            report.validate_build(&identity.build)?;
        }
        let nonce = format!("{:032x}", rand::random::<u128>());
        let launch = PendingLaunch {
            operation_id: nonce.clone(),
            phase_nonce: nonce.clone(),
            launch_nonce: format!("{:032x}", rand::random::<u128>()),
            identity: identity.clone(),
            skip_cache_restore: false,
            phase: Phase::AwaitingReadiness,
            initialized: preflight.is_some(),
        };
        let journal = Journal {
            schema: 1,
            installed: identity,
            operation: None,
            last_operation: None,
            installation_pending: Some(Installation {
                nonce: nonce.clone(),
                config_revision: preflight.map(|p| p.check.config_revision),
                launch,
            }),
        };
        self.persist(&journal)?;
        println!("{nonce}");
        Ok(())
    }
    fn install_check(&self) -> Result<()> {
        if self.private.exists("journal.json")? {
            let journal = Journal::load(&self.private)?;
            ensure!(
                journal.installation_pending.is_none()
                    && journal
                        .operation
                        .as_ref()
                        .is_none_or(|o| o.status.phase.terminal()),
                "another installation or update is pending"
            );
            ensure!(
                self.actual_hash()? == journal.installed.sha256,
                "installed identity differs"
            );
        }
        Ok(())
    }
    fn install_restore(&self) -> Result<()> {
        let mut journal = Journal::load(&self.private)?;
        let current = BuildInfo::current();
        ensure!(
            journal.installation_pending.is_none()
                && journal.installed.build.durable_contract_epoch == current.durable_contract_epoch
                && journal.installed.build.runtime_database_format
                    == current.runtime_database_format
                && journal.installed.build.install_contract == current.install_contract,
            "manual recovery required: durable contract differs"
        );
        ensure!(
            self.actual_hash()? == journal.installed.sha256,
            "restored identity differs"
        );
        let report: Option<ManagedCheckOutput> =
            serde_json::from_slice(&self.private.read("install-preflight.json", 8192, true)?)?;
        if let Some(report) = &report {
            // This is the candidate's saved read-only report, not a claim that
            // the restored binary generated it. Its build must match this helper.
            report.validate_build(&current)?;
        }
        let revision = report.map(|report| report.check.config_revision);
        let nonce = format!("{:032x}", rand::random::<u128>());
        journal.installation_pending = Some(Installation {
            nonce: nonce.clone(),
            config_revision: revision,
            launch: PendingLaunch {
                operation_id: nonce.clone(),
                phase_nonce: nonce.clone(),
                launch_nonce: format!("{:032x}", rand::random::<u128>()),
                identity: journal.installed.clone(),
                skip_cache_restore: true,
                phase: Phase::AwaitingReadiness,
                initialized: revision.is_some(),
            },
        });
        self.persist(&journal)?;
        println!("{nonce}");
        Ok(())
    }
    fn install_match(&self, nonce: &str) -> Result<Journal> {
        let journal = Journal::load(&self.private)?;
        ensure!(
            journal
                .installation_pending
                .as_ref()
                .is_some_and(|p| p.nonce == nonce),
            "installation nonce differs"
        );
        ensure!(
            self.actual_hash()? == journal.installed.sha256,
            "installation identity differs"
        );
        Ok(journal)
    }
    async fn install_verify(&self, nonce: &str) -> Result<()> {
        validate_units().await?;
        let mut journal = self.install_match(nonce)?;
        let pending = journal
            .installation_pending
            .as_ref()
            .context("missing installation")?;
        self.wait_launch(&pending.launch, pending.config_revision.unwrap_or(0))
            .await?;
        journal.installation_pending = None;
        self.persist(&journal)
    }
    fn actual_hash(&self) -> Result<String> {
        fs::sha256(self.live.file("parins", MAX_BINARY_BYTES, true)?)
    }
    async fn run(&mut self) -> Result<()> {
        validate_units().await?;
        let mut journal = Journal::load(&self.private)?;
        let state = state_directory()?;
        // Consume the one name through a held directory FD. Malformed requests
        // cannot keep PathExists active and spin the root unit.
        let bytes = state.read(INBOX_NAME, INBOX_LIMIT, false);
        state.remove(INBOX_NAME)?;
        let request = Inbox::parse(&bytes?)?;
        ensure!(
            journal.installation_pending.is_none(),
            "installation_pending"
        );
        match request.request.clone() {
            Request::Stage { .. } => self.stage(&mut journal, &request).await,
            Request::Commit {
                invocation_id,
                config_revision,
            } => {
                let op = matching(&journal, &request)?;
                if op.status.phase.terminal() {
                    return self.publish(&journal);
                }
                ensure!(
                    op.status.phase == Phase::Staged
                        && op.invocation_id == invocation_id
                        && op.config_revision == config_revision,
                    "invalid commit phase"
                );
                let current_time = now_ms();
                if current_time < op.started_at_ms
                    || current_time.saturating_sub(op.started_at_ms) > 30 * 60 * 1000
                {
                    return self.finish(&mut journal, Phase::Failed, Some("plan_expired"));
                }
                let current = service().await?;
                if current.get("InvocationID") != Some(&invocation_id)
                    || !current.get("ActiveState").is_some_and(|v| v == "active")
                {
                    return self.finish(&mut journal, Phase::Failed, Some("config_changed"));
                }
                self.commit(&mut journal).await
            }
            Request::Abort {} => self.abort(&mut journal, &request),
            Request::VerifyRecovery {} => {
                let op = matching(&journal, &request)?;
                ensure!(
                    op.status.phase == Phase::AwaitingReadiness,
                    "not awaiting recovery verification"
                );
                if self.wait_ready(&journal).await.is_ok() {
                    self.finish(&mut journal, Phase::RolledBack, Some("interrupted"))
                } else {
                    self.finish(
                        &mut journal,
                        Phase::ManualRequired,
                        Some("readiness_failed"),
                    )
                }
            }
        }
    }
    fn abort(&self, journal: &mut Journal, inbox: &Inbox) -> Result<()> {
        let op = matching(journal, inbox)?;
        if op.status.phase.precommit() {
            self.finish(journal, Phase::Aborted, Some("preflight_failed"))?;
        }
        // Persisted terminal is the irreversible fence against late commit.
        self.publish(journal)
    }

    async fn stage(&mut self, journal: &mut Journal, inbox: &Inbox) -> Result<()> {
        let Request::Stage {
            download,
            current_sha256,
            invocation_id,
            config_revision,
        } = &inbox.request
        else {
            bail!("not a stage request")
        };
        if let Some(op) = &journal.operation {
            if op.status.operation_id == inbox.operation_id {
                ensure!(
                    op.status.phase_nonce == inbox.phase_nonce
                        && op.invocation_id == *invocation_id
                        && op.config_revision == *config_revision
                        && op.old.sha256 == *current_sha256
                        && op.candidate.sha256 == download.sha256,
                    "phase parameters mismatch"
                );
                return self.publish(journal);
            }
            ensure!(op.status.phase.terminal(), "update_in_progress");
            ensure!(
                now_ms().saturating_sub(op.started_at_ms) >= 60_000,
                "rate_limited"
            );
            if let Ok(dir) = self.staging.child(&op.status.operation_id) {
                for name in ["candidate.part", "candidate", "previous", "rollback"] {
                    dir.remove(name)?;
                }
                self.staging.remove_dir(&op.status.operation_id)?;
            }
        }
        ensure!(
            &journal.installed.sha256 == current_sha256 && self.actual_hash()? == *current_sha256,
            "installed identity changed"
        );
        ensure!(
            service().await?.get("InvocationID") == Some(invocation_id),
            "original invocation changed"
        );
        let version = crate::update::contract::Version::parse_tag(&download.tag)?.to_string();
        ensure!(
            crate::update::contract::valid_sha256(&download.sha256),
            "invalid candidate digest"
        );
        // Record accepted before the first network request. ExecStopPost can
        // then close metadata/download interruption as a real terminal.
        let mut candidate = journal.installed.clone();
        candidate.build.version = version;
        candidate.sha256 = download.sha256.clone();
        journal.operation = Some(Operation {
            status: OperationStatus {
                operation_id: inbox.operation_id.clone(),
                phase_nonce: inbox.phase_nonce.clone(),
                phase: Phase::Accepted,
                version: candidate.build.version.clone(),
                reason: None,
                updated_at_ms: now_ms(),
                downloaded_bytes: 0,
                total_bytes: download.size,
            },
            old: journal.installed.clone(),
            candidate,
            invocation_id: invocation_id.clone(),
            config_revision: *config_revision,
            started_at_ms: now_ms(),
            rollback_attempted: false,
            rollback_started: false,
            pending_launch: None,
        });
        self.persist(journal)?;
        let outcome: Result<()> = async {
            let reader = GithubReader::new();
            let manifest = reader.manifest(&download.tag).await?;
            ensure!(
                manifest.sha256 == download.manifest_sha256,
                "release_changed"
            );
            manifest
                .manifest
                .check_compatible(&journal.installed.build, HELPER_PROTOCOL)?;
            let artifact = manifest
                .manifest
                .artifact_for(&journal.installed.build.target)?;
            ensure!(
                artifact.name == download.asset_name
                    && artifact.size == download.size
                    && artifact.sha256 == download.sha256,
                "release_changed"
            );
            let m = &manifest.manifest;
            let candidate = InstalledIdentity {
                sha256: artifact.sha256.clone(),
                build: BuildInfo {
                    version: m.version.clone(),
                    target: artifact.target.clone(),
                    source_commit: m.source_commit.clone(),
                    official_release: true,
                    update_protocol: m.update_protocol,
                    helper_protocol: journal.installed.build.helper_protocol,
                    install_contract: m.install_contract.clone(),
                    durable_contract_epoch: m.durable_contract_epoch,
                    runtime_database_format: m.runtime_database_format,
                    cache_snapshot_format: m.cache_snapshot_format,
                    cache_semantics: m.cache_semantics,
                },
            };
            journal
                .operation
                .as_mut()
                .expect("accepted operation")
                .candidate = candidate;
            self.staging.space(
                download
                    .size
                    .saturating_add(
                        self.live
                            .file("parins", MAX_BINARY_BYTES, true)?
                            .metadata()?
                            .len(),
                    )
                    .saturating_add(32 * 1024 * 1024),
            )?;
            self.private.space(1024 * 1024)?;
            let dir = self.staging.create_dir(&inbox.operation_id, 0o755)?;
            self.phase(journal, Phase::Downloading)?;
            let file = dir.create("candidate.part", 0o644)?;
            let mut file = tokio::fs::File::from_std(file);
            let mut last = Instant::now();
            reader
                .download(download, &mut file, |size| {
                    if let Some(op) = &mut journal.operation {
                        op.status.downloaded_bytes = size;
                    }
                    if last.elapsed() >= Duration::from_secs(1) {
                        let _ = self.publish(journal);
                        last = Instant::now();
                    }
                })
                .await?;
            let file = file.into_std().await;
            fs::sync_file(&file)?;
            drop(file);
            fs::validate_elf(
                dir.file("candidate.part", MAX_BINARY_BYTES, true)?,
                &journal.installed.build.target,
            )?;
            let candidate_fd = dir.file("candidate.part", MAX_BINARY_BYTES, true)?;
            rustix::fs::fchmod(&candidate_fd, rustix::fs::Mode::from_raw_mode(0o755))?;
            fs::sync_file(&candidate_fd)?;
            dir.rename_to("candidate.part", &dir, "candidate")?;
            self.phase(journal, Phase::Staged)
        }
        .await;
        if let Err(error) = &outcome {
            self.finish(journal, Phase::Failed, Some(failure_code(error)))?;
        }
        outcome
    }
    fn candidate_dir(&self, journal: &Journal) -> Result<Dir> {
        self.staging.child(
            &journal
                .operation
                .as_ref()
                .context("missing operation")?
                .status
                .operation_id,
        )
    }
    fn copy_live_to_backup(&self, journal: &Journal) -> Result<()> {
        let dir = self.candidate_dir(journal)?;
        let op = journal.operation.as_ref().context("missing operation")?;
        ensure!(
            self.actual_hash()? == op.old.sha256,
            "unexpected installed binary"
        );
        let mut source = self.live.file("parins", MAX_BINARY_BYTES, true)?;
        let mut target = dir.create("previous", 0o755)?;
        std::io::copy(&mut source, &mut target)?;
        fs::sync_file(&target)?;
        dir.sync()?;
        ensure!(
            fs::sha256(dir.file("previous", MAX_BINARY_BYTES, true)?)? == op.old.sha256,
            "backup hash mismatch"
        );
        Ok(())
    }
    fn launch(&self, journal: &mut Journal, rollback: bool) -> Result<()> {
        let op = journal.operation.as_mut().context("missing operation")?;
        let phase = if rollback {
            Phase::RollingBack
        } else {
            Phase::Starting
        };
        op.pending_launch = Some(PendingLaunch {
            operation_id: op.status.operation_id.clone(),
            launch_nonce: format!("{:032x}", rand::random::<u128>()),
            identity: if rollback {
                op.old.clone()
            } else {
                op.candidate.clone()
            },
            skip_cache_restore: rollback,
            phase,
            phase_nonce: op.status.phase_nonce.clone(),
            initialized: true,
        });
        op.status.phase = phase;
        // This must complete before systemctl start. Status is derived but is
        // the bounded launch descriptor available inside DynamicUser sandbox.
        self.persist(journal)
    }
    async fn commit(&mut self, journal: &mut Journal) -> Result<()> {
        let op = journal.operation.as_ref().context("missing operation")?;
        ensure!(
            self.actual_hash()? == op.old.sha256,
            "installed identity changed"
        );
        let dir = self.candidate_dir(journal)?;
        ensure!(
            fs::sha256(dir.file("candidate", MAX_BINARY_BYTES, true)?)? == op.candidate.sha256,
            "candidate changed"
        );
        if self.copy_live_to_backup(journal).is_err() {
            return self.finish(journal, Phase::Failed, Some("verification_failed"));
        }
        self.phase(journal, Phase::Committing)?;
        self.phase(journal, Phase::Stopping)?;
        if stop_clean().await.is_err() {
            // A forcibly stopped old version never authorizes candidate install.
            self.rollback(journal, "unclean_shutdown", false).await?;
            return Ok(());
        }
        let result: Result<()> = async {
            dir.rename_to("candidate", &self.live, "parins")?;
            self.launch(journal, false)?;
            control("start").await?;
            self.phase(journal, Phase::Validating)?;
            self.wait_ready(journal).await?;
            journal.commit()?;
            // Unique commit point: installed identity AND succeeded in ONE
            // durable journal replace, preceding the disposable public view.
            self.persist(journal)
        }
        .await;
        if result.is_err()
            && journal
                .operation
                .as_ref()
                .is_some_and(|o| !o.status.phase.terminal())
        {
            self.rollback(journal, "readiness_failed", false).await?;
        }
        result
    }
    async fn wait_ready(&self, journal: &Journal) -> Result<()> {
        let op = journal.operation.as_ref().context("missing operation")?;
        let pending = op
            .pending_launch
            .as_ref()
            .context("missing pending launch")?;
        self.wait_launch(pending, op.config_revision).await
    }
    async fn wait_launch(&self, pending: &PendingLaunch, config_revision: u64) -> Result<()> {
        let start = Instant::now();
        let mut readiness = Readiness::default();
        let mut invocation = String::new();
        let mut executable_identity: Option<(u32, String, Option<String>)> = None;
        while start.elapsed() < Duration::from_secs(60) {
            let info = service().await?;
            let pid = info
                .get("MainPID")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let current = info.get("InvocationID").cloned().unwrap_or_default();
            if invocation != current {
                readiness = Readiness::default();
                invocation = current.clone();
            }
            let health = Dir::open(Path::new("/run/parins-managed"), false)
                .and_then(|dir| dir.read("update-health.json", 8192, false))
                .and_then(|bytes| Ok(serde_json::from_slice::<Health>(&bytes)?))
                .ok();
            // The root-owned running inode is immutable to the app. Hash once
            // per PID/InvocationID, not tens of megabytes every health tick.
            if executable_identity
                .as_ref()
                .is_none_or(|(old_pid, old_invocation, _)| {
                    *old_pid != pid || old_invocation != &current
                })
            {
                let hash = if pid > 0 {
                    std::fs::File::open(format!("/proc/{pid}/exe"))
                        .and_then(|file| fs::sha256(file).map_err(std::io::Error::other))
                        .ok()
                } else {
                    None
                };
                executable_identity = Some((pid, current.clone(), hash));
            }
            let hash = executable_identity
                .as_ref()
                .and_then(|(_, _, hash)| hash.as_deref());
            let matching = health
                .as_ref()
                .filter(|_| hash == Some(pending.identity.sha256.as_str()) && valid_id(&current));
            if readiness.observe(
                start.elapsed().as_millis() as u64,
                matching,
                pending,
                pid,
                &current,
                config_revision,
            ) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        bail!("readiness_failed")
    }
    async fn rollback(&self, journal: &mut Journal, reason: &str, boot: bool) -> Result<()> {
        let op = journal.operation.as_ref().context("missing operation")?;
        ensure!(
            !op.rollback_started || boot,
            "rollback start already attempted; manual recovery required"
        );
        let old = op.old.clone();
        let actual = self.actual_hash()?;
        ensure!(
            actual == old.sha256 || actual == op.candidate.sha256,
            "unknown live binary; manual recovery required"
        );
        journal
            .operation
            .as_mut()
            .expect("checked operation")
            .rollback_attempted = true;
        self.phase(journal, Phase::RollingBack)?;
        if !boot {
            let _ = control("stop").await;
            ensure!(
                service().await?.get("MainPID").is_some_and(|s| s == "0"),
                "old process has not exited"
            );
        }
        if actual != old.sha256 {
            let dir = self.candidate_dir(journal)?;
            ensure!(
                fs::sha256(dir.file("previous", MAX_BINARY_BYTES, true)?)? == old.sha256,
                "backup identity changed"
            );
            // Keep the only verified old copy available until durable terminal.
            let mut input = dir.file("previous", MAX_BINARY_BYTES, true)?;
            // Resume an interrupted copy using the same bounded root-owned
            // staging name; the verified previous ELF remains untouched.
            dir.remove("rollback")?;
            let mut output = dir.create("rollback", 0o755)?;
            std::io::copy(&mut input, &mut output)?;
            fs::sync_file(&output)?;
            dir.rename_to("rollback", &self.live, "parins")?;
        }
        self.launch(journal, true)?;
        if boot {
            return self.phase(journal, Phase::AwaitingReadiness);
        }
        journal
            .operation
            .as_mut()
            .expect("checked operation")
            .rollback_started = true;
        self.persist(journal)?;
        if control("start").await.is_ok() && self.wait_ready(journal).await.is_ok() {
            self.finish(journal, Phase::RolledBack, Some(reason))
        } else {
            self.finish(journal, Phase::ManualRequired, Some("readiness_failed"))
        }
    }
    async fn recover(&mut self, boot: bool) -> Result<()> {
        validate_units().await?;
        let mut journal = Journal::load(&self.private)?;
        if journal.installation_pending.is_some() {
            ensure!(
                self.actual_hash()? == journal.installed.sha256,
                "pending installation identity differs"
            );
            // Installer owns this nonce until verify/rollback. Boot only
            // republishes its durable descriptor; it never starts the app.
            return self.publish(&journal);
        }
        let Some(op) = &journal.operation else {
            ensure!(
                self.actual_hash()? == journal.installed.sha256,
                "unknown installed binary"
            );
            return self.publish(&journal);
        };
        if op.status.phase.terminal() {
            ensure!(
                self.actual_hash()? == journal.installed.sha256,
                "unknown installed binary"
            );
            return self.publish(&journal);
        }
        if op.status.phase == Phase::Staged && !boot {
            return self.publish(&journal);
        }
        if op.status.phase.precommit() {
            return self.finish(&mut journal, Phase::Failed, Some("interrupted"));
        }
        if !boot && !manager_running().await {
            // During shutdown never queue a start job or wait for the app.
            return self.phase(&mut journal, Phase::AwaitingBootRecovery);
        }
        if op.rollback_started && !boot {
            if self.actual_hash()? == op.old.sha256
                && op.pending_launch.is_some()
                && self.wait_ready(&journal).await.is_ok()
            {
                return self.finish(&mut journal, Phase::RolledBack, Some("interrupted"));
            }
            return self.finish(
                &mut journal,
                Phase::ManualRequired,
                Some("manual_recovery_required"),
            );
        }
        if self
            .rollback(&mut journal, "interrupted", boot)
            .await
            .is_err()
        {
            self.finish(
                &mut journal,
                Phase::ManualRequired,
                Some("manual_recovery_required"),
            )?;
            bail!("manual_recovery_required");
        }
        Ok(())
    }
}

fn matching<'a>(journal: &'a Journal, inbox: &Inbox) -> Result<&'a Operation> {
    let op = journal.operation.as_ref().context("unknown operation")?;
    ensure!(
        op.status.operation_id == inbox.operation_id && op.status.phase_nonce == inbox.phase_nonce,
        "unknown operation phase"
    );
    Ok(op)
}

fn state_directory() -> Result<Dir> {
    let meta = std::fs::symlink_metadata(STATE)?;
    if meta.file_type().is_symlink() {
        let path = std::fs::read_link(STATE)?;
        ensure!(
            path == Path::new(PRIVATE_STATE) || path == Path::new("private/parins-managed"),
            "unexpected StateDirectory mapping"
        );
        // systemd's id-mapped backing can be nobody-owned. Parent ownership,
        // fixed mapping and held FD establish scope, NOT dynamic UID equality.
        let parent = Dir::open(Path::new("/var/lib/private"), true)?;
        let fd = rustix::fs::openat(
            &parent.fd,
            "parins-managed",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        Ok(Dir { fd })
    } else {
        Dir::open(Path::new(STATE), false)
    }
}

async fn systemctl(arguments: &[&str], seconds: u64) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new("/usr/bin/systemctl");
    command
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("missing systemctl stdout")?;
    tokio::time::timeout(Duration::from_secs(seconds), async move {
        let mut bytes = Vec::new();
        stdout.take(65537).read_to_end(&mut bytes).await?;
        ensure!(bytes.len() <= 65536, "systemctl output bound exceeded");
        ensure!(child.wait().await?.success(), "systemctl operation failed");
        Ok(String::from_utf8(bytes)?)
    })
    .await
    .context("systemctl deadline exceeded")?
}
async fn control(action: &str) -> Result<()> {
    systemctl(&[action, UNIT], if action == "stop" { 160 } else { 60 }).await?;
    Ok(())
}
async fn service() -> Result<BTreeMap<String, String>> {
    let output = systemctl(
        &[
            "show",
            UNIT,
            "--property=MainPID,InvocationID,ActiveState,Result,ExecMainCode,ExecMainStatus",
        ],
        10,
    )
    .await?;
    Ok(output
        .lines()
        .filter_map(|line| line.split_once('=').map(|(k, v)| (k.into(), v.into())))
        .collect())
}
async fn stop_clean() -> Result<()> {
    control("stop").await?;
    let p = service().await?;
    ensure!(
        p.get("MainPID").is_some_and(|s| s == "0")
            && p.get("Result").is_some_and(|s| s == "success")
            && p.get("ExecMainCode").is_some_and(|s| s == "1")
            && p.get("ExecMainStatus").is_some_and(|s| s == "0"),
        "unclean_shutdown"
    );
    Ok(())
}
async fn manager_running() -> bool {
    systemctl(&["show", "--property=SystemState", "--value"], 10)
        .await
        .is_ok_and(|s| matches!(s.trim(), "running" | "degraded" | "starting"))
}

async fn validate_units() -> Result<()> {
    let directory = Dir::open(Path::new("/etc/systemd/system"), true)?;
    for (name, expected) in [
        (UNIT, include_str!("../../../deploy/parins-managed.service")),
        (
            "parins-updater.service",
            include_str!("../../../deploy/parins-updater.service"),
        ),
        (
            "parins-updater.path",
            include_str!("../../../deploy/parins-updater.path"),
        ),
        (
            "parins-update-recovery.service",
            include_str!("../../../deploy/parins-update-recovery.service"),
        ),
    ] {
        ensure!(
            directory.read(name, 32768, true)? == expected.as_bytes(),
            "unsupported unit contract"
        );
        ensure!(
            systemctl(&["show", name, "--property=FragmentPath", "--value"], 10)
                .await?
                .trim()
                == format!("/etc/systemd/system/{name}"),
            "shadowed updater unit"
        );
        let dropins = systemctl(&["show", name, "--property=DropInPaths", "--value"], 10).await?;
        for path in dropins.split_whitespace() {
            ensure!(name == UNIT, "updater unit drop-ins unsupported");
            let parent = Path::new(path).parent().context("drop-in parent")?;
            let file = Path::new(path)
                .file_name()
                .and_then(|v| v.to_str())
                .context("drop-in name")?;
            let bytes = Dir::open(parent, true)?.read(file, 8192, true)?;
            let text = String::from_utf8(bytes)?;
            let mut section = false;
            for line in text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with(['#', ';']))
            {
                if line == "[Service]" {
                    section = true;
                    continue;
                }
                let groups = line
                    .strip_prefix("SupplementaryGroups=")
                    .context("unsupported drop-in")?;
                ensure!(
                    section
                        && !groups.is_empty()
                        && groups
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_. -".contains(&b)),
                    "unsupported certificate group"
                );
            }
        }
    }
    for (key, value) in [
        ("FragmentPath", "/etc/systemd/system/parins-managed.service"),
        ("DynamicUser", "yes"),
        ("User", "parins-managed"),
        ("Group", "parins-managed"),
        ("StateDirectory", "parins-managed"),
        ("WorkingDirectory", STATE),
        ("RuntimeDirectory", "parins-managed"),
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
    ] {
        ensure!(
            systemctl(&["show", UNIT, &format!("--property={key}"), "--value"], 10)
                .await?
                .trim()
                == value,
            "effective unit contract differs"
        );
    }
    let helper = Dir::open(Path::new("/usr/libexec"), true)?;
    helper.file("parins-updater", MAX_BINARY_BYTES, true)?;
    let command = systemctl(&["show", UNIT, "--property=ExecStart", "--value"], 10).await?;
    ensure!(command.matches('{').count()==1 && command.starts_with("{ path=/opt/parins-managed/parins ; argv[]=/opt/parins-managed/parins --manage --state-dir /var/lib/parins-managed --web-listen 0.0.0.0:3000 ; ignore_errors=no ; "),"effective executable contract changed");
    for property in [
        "RootDirectory",
        "RootImage",
        "Environment",
        "EnvironmentFiles",
        "ExecStartPre",
        "ExecStartPost",
        "ExecStop",
        "ExecStopPost",
    ] {
        ensure!(
            systemctl(
                &["show", UNIT, &format!("--property={property}"), "--value"],
                10
            )
            .await?
            .trim()
            .is_empty(),
            "effective managed unit override"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn consume_abort_fixture(
    path: &Path,
    status: &PublicStatus,
    inbox: &[u8],
) -> Result<PublicStatus> {
    // Only the filesystem/installation fixture is substituted: parsing, matching,
    // Abort arbitration, journal durability and public-status publication are real.
    let executor = Executor {
        root: Dir::open(path, false)?,
        private: Dir::open(path, false)?,
        live: Dir::open(path, false)?,
        staging: Dir::open(path, false)?,
        _lock: std::fs::File::create(path.join("lock"))?,
    };
    let mut journal = Journal {
        schema: 1,
        installed: status.installed.clone(),
        operation: Some(Operation {
            status: status
                .active_operation
                .clone()
                .context("fixture active operation")?,
            old: status.installed.clone(),
            candidate: status.installed.clone(),
            invocation_id: "f".repeat(32),
            config_revision: 2,
            started_at_ms: now_ms(),
            rollback_attempted: false,
            rollback_started: false,
            pending_launch: status.pending_launch.clone(),
        }),
        last_operation: status.last_operation.clone(),
        installation_pending: None,
    };
    executor.persist(&journal)?;
    let request = Inbox::parse(inbox)?;
    ensure!(
        matches!(request.request, Request::Abort {}),
        "expected abort"
    );
    executor.abort(&mut journal, &request)?;
    let saved: Journal = serde_json::from_slice(&std::fs::read(path.join("journal.json"))?)?;
    let published: PublicStatus =
        serde_json::from_slice(&std::fs::read(path.join("status.json"))?)?;
    ensure!(
        serde_json::to_value(saved.status().last_operation)?
            == serde_json::to_value(&published.last_operation)?,
        "fence not durable"
    );
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stage_failures_keep_stable_actionable_reasons() {
        for code in ["insufficient_space", "rate_limited", "release_changed"] {
            assert_eq!(failure_code(&anyhow::anyhow!(code)), code);
        }
        assert_eq!(
            failure_code(&crate::update::contract::ContractError::ReleaseChanged.into()),
            "release_changed"
        );
        assert_eq!(
            failure_code(&crate::update::contract::ContractError::ManualRequired.into()),
            "manual_upgrade_required"
        );
        assert_eq!(
            failure_code(&anyhow::anyhow!("untrusted remote detail")),
            "verification_failed"
        );
        assert_eq!(
            failure_code(&std::io::Error::from(std::io::ErrorKind::StorageFull).into()),
            "insufficient_space"
        );
        assert_eq!(
            failure_code(&rustix::io::Errno::NOSPC.into()),
            "insufficient_space"
        );
    }
}
