use super::fs::Dir;
use crate::update::ipc::*;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub status: OperationStatus,
    pub old: InstalledIdentity,
    pub candidate: InstalledIdentity,
    pub invocation_id: String,
    pub config_revision: u64,
    pub started_at_ms: u64,
    pub rollback_attempted: bool,
    pub rollback_started: bool,
    pub pending_launch: Option<PendingLaunch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema: u32,
    pub installed: InstalledIdentity,
    pub operation: Option<Operation>,
    pub last_operation: Option<OperationStatus>,
    pub installation_pending: Option<Installation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub nonce: String,
    pub config_revision: Option<u64>,
    pub launch: PendingLaunch,
}

impl Journal {
    pub fn load(dir: &Dir) -> Result<Self> {
        let result: Self = serde_json::from_slice(&dir.read("journal.json", 32768, true)?)?;
        ensure!(result.schema == 1, "unknown journal schema");
        ensure!(
            crate::update::contract::valid_sha256(&result.installed.sha256),
            "invalid journal anchor"
        );
        if let Some(op) = &result.operation {
            ensure!(
                valid_id(&op.status.operation_id) && valid_id(&op.status.phase_nonce),
                "invalid journal operation"
            );
        }
        if let Some(pending) = &result.installation_pending {
            ensure!(
                valid_id(&pending.nonce)
                    && pending.launch.operation_id == pending.nonce
                    && pending.launch.phase_nonce == pending.nonce
                    && valid_id(&pending.launch.launch_nonce)
                    && pending.launch.identity == result.installed
                    && pending.launch.initialized == pending.config_revision.is_some()
                    && pending.config_revision.is_none_or(|revision| revision > 0)
                    && result
                        .operation
                        .as_ref()
                        .is_none_or(|op| op.status.phase.terminal()),
                "invalid installation journal"
            );
        }
        Ok(result)
    }
    pub fn save(&self, dir: &Dir) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= 32768, "journal exceeds bound");
        dir.replace("journal.json", &bytes, 0o600)
    }
    pub fn status(&self) -> PublicStatus {
        let manual = self
            .operation
            .as_ref()
            .is_some_and(|op| op.status.phase == Phase::ManualRequired);
        PublicStatus {
            schema: 1,
            installed: self.installed.clone(),
            capability: Capability {
                available: self.installed.build.official_release
                    && !manual
                    && self.installation_pending.is_none(),
                reason: if self.installation_pending.is_some() {
                    Some("installation_pending".into())
                } else if manual {
                    Some("manual_recovery_required".into())
                } else {
                    (!self.installed.build.official_release)
                        .then(|| "unsupported_installation".into())
                },
                checked_at_ms: now_ms(),
                helper_protocol: crate::update::contract::HELPER_PROTOCOL,
                install_contract: crate::update::contract::INSTALL_CONTRACT.into(),
            },
            active_operation: self
                .operation
                .as_ref()
                .filter(|o| !o.status.phase.terminal())
                .map(|o| o.status.clone()),
            last_operation: self
                .operation
                .as_ref()
                .filter(|o| o.status.phase.terminal())
                .map(|o| o.status.clone())
                .or_else(|| self.last_operation.clone()),
            pending_launch: self
                .operation
                .as_ref()
                .and_then(|o| o.pending_launch.clone())
                .or_else(|| self.installation_pending.as_ref().map(|p| p.launch.clone())),
        }
    }
    pub fn finish(&mut self, phase: Phase, reason: Option<&str>) {
        if let Some(op) = &mut self.operation {
            op.status.phase = phase;
            op.status.reason = reason.map(str::to_owned);
            op.status.updated_at_ms = now_ms();
            op.pending_launch = None;
            self.last_operation = Some(op.status.clone());
        }
    }
    pub fn commit(&mut self) -> Result<()> {
        let op = self
            .operation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing operation"))?;
        ensure!(op.status.phase == Phase::Validating, "invalid commit phase");
        self.installed = op.candidate.clone();
        self.finish(Phase::Succeeded, None);
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Helper monotonic time, not wall time or the app timestamp, owns liveness.
#[derive(Default)]
pub struct Readiness {
    last: Option<(u64, u64)>,
    since: Option<u64>,
}
impl Readiness {
    pub fn observe(
        &mut self,
        now: u64,
        sample: Option<&Health>,
        pending: &PendingLaunch,
        pid: u32,
        invocation: &str,
        revision: u64,
    ) -> bool {
        let Some(h) = sample.filter(|h| {
            h.operation_id == pending.operation_id
                && h.launch_nonce == pending.launch_nonce
                && h.identity == pending.identity
                && h.pid == pid
                && h.invocation_id == invocation
                && h.config_revision == revision
                && (if pending.initialized {
                    h.dns_generation > 0
                        && h.dns_ready
                        && h.storage_healthy
                        && h.storage_settings_applied
                } else {
                    h.config_revision == 0 && h.dns_generation == 0 && !h.dns_ready
                })
        }) else {
            self.last = None;
            self.since = None;
            return false;
        };
        if self
            .last
            .is_some_and(|(_, at)| now.saturating_sub(at) > 3000)
        {
            self.since = None;
        }
        match self.last {
            Some((seq, _)) if h.sample_seq < seq => {
                self.last = None;
                self.since = None;
                return false;
            }
            Some((seq, at)) if h.sample_seq == seq => {
                if now.saturating_sub(at) > 3000 {
                    self.since = None;
                }
            }
            _ => {
                self.last = Some((h.sample_seq, now));
                self.since.get_or_insert(now);
            }
        }
        self.since
            .is_some_and(|since| now.saturating_sub(since) >= 10000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> InstalledIdentity {
        InstalledIdentity {
            build: crate::update::build_info::BuildInfo::current(),
            sha256: "a".repeat(64),
        }
    }
    fn sample() -> (PendingLaunch, Health) {
        let pending = PendingLaunch {
            operation_id: "1".repeat(32),
            launch_nonce: "2".repeat(32),
            identity: identity(),
            skip_cache_restore: false,
            phase: Phase::Starting,
            phase_nonce: "3".repeat(32),
            initialized: true,
        };
        let health = Health {
            operation_id: pending.operation_id.clone(),
            launch_nonce: pending.launch_nonce.clone(),
            identity: identity(),
            config_revision: 7,
            dns_generation: 1,
            dns_ready: true,
            storage_healthy: true,
            storage_settings_applied: true,
            sample_seq: 1,
            pid: 123,
            invocation_id: "4".repeat(32),
        };
        (pending, health)
    }
    #[test]
    fn readiness_requires_fresh_sequences_for_ten_seconds() {
        let (pending, mut health) = sample();
        let mut state = Readiness::default();
        for tick in 0..10 {
            health.sample_seq = tick + 1;
            assert!(!state.observe(
                tick * 1000,
                Some(&health),
                &pending,
                123,
                &"4".repeat(32),
                7
            ));
        }
        health.sample_seq = 11;
        assert!(state.observe(10000, Some(&health), &pending, 123, &"4".repeat(32), 7));
        let mut stalled = Readiness::default();
        for tick in 0..61 {
            assert!(!stalled.observe(
                tick * 1000,
                Some(&health),
                &pending,
                123,
                &"4".repeat(32),
                7
            ));
        }
    }
    #[test]
    fn wrong_pid_nonce_identity_revision_and_unhealthy_storage_reset_window() {
        let (pending, health) = sample();
        for case in 0..7 {
            let mut state = Readiness::default();
            let mut changed = health.clone();
            match case {
                0 => changed.pid += 1,
                1 => changed.launch_nonce = "5".repeat(32),
                2 => changed.identity.sha256 = "b".repeat(64),
                3 => changed.config_revision += 1,
                4 => changed.storage_healthy = false,
                5 => changed.storage_settings_applied = false,
                _ => changed.dns_ready = false,
            }
            for tick in 0..20 {
                changed.sample_seq = tick + 1;
                assert!(!state.observe(
                    tick * 1000,
                    Some(&changed),
                    &pending,
                    123,
                    &"4".repeat(32),
                    7
                ));
            }
        }
    }
    #[test]
    fn fresh_installation_readiness_does_not_claim_dns_running() {
        let (mut pending, mut health) = sample();
        pending.initialized = false;
        health.config_revision = 0;
        health.dns_generation = 0;
        health.dns_ready = false;
        health.storage_healthy = false;
        health.storage_settings_applied = false;
        let mut readiness = Readiness::default();
        for tick in 0..10 {
            health.sample_seq = tick + 1;
            assert!(!readiness.observe(
                tick * 1000,
                Some(&health),
                &pending,
                123,
                &"4".repeat(32),
                0
            ));
        }
        health.sample_seq = 11;
        assert!(readiness.observe(10000, Some(&health), &pending, 123, &"4".repeat(32), 0));
        pending.initialized = true;
        assert!(!Readiness::default().observe(0, Some(&health), &pending, 123, &"4".repeat(32), 0));
    }
    #[test]
    fn installer_pending_is_the_same_envelope_and_withholds_capability() {
        let (launch, _) = sample();
        let mut installed = identity();
        installed.build.official_release = true;
        let mut journal = Journal {
            schema: 1,
            installed,
            operation: None,
            last_operation: None,
            installation_pending: Some(Installation {
                nonce: launch.operation_id.clone(),
                config_revision: Some(7),
                launch: launch.clone(),
            }),
        };
        assert!(!journal.status().capability.available);
        assert_eq!(
            journal.status().capability.reason.as_deref(),
            Some("installation_pending")
        );
        assert_eq!(journal.status().pending_launch, Some(launch));
        journal.installation_pending = None;
        assert!(journal.status().capability.available);
        assert!(journal.status().pending_launch.is_none());
    }
    #[test]
    fn committed_anchor_and_terminal_are_one_envelope_and_abort_is_a_fence() {
        let mut journal = Journal {
            schema: 1,
            installed: identity(),
            operation: Some(Operation {
                status: OperationStatus {
                    operation_id: "1".repeat(32),
                    phase_nonce: "2".repeat(32),
                    phase: Phase::Validating,
                    version: "0.1.5".into(),
                    reason: None,
                    updated_at_ms: 0,
                    downloaded_bytes: 1,
                    total_bytes: 1,
                },
                old: identity(),
                candidate: InstalledIdentity {
                    sha256: "b".repeat(64),
                    ..identity()
                },
                invocation_id: "3".repeat(32),
                config_revision: 1,
                started_at_ms: 0,
                rollback_attempted: false,
                rollback_started: false,
                pending_launch: None,
            }),
            last_operation: None,
            installation_pending: None,
        };
        let mut aborted = journal.clone();
        aborted.finish(Phase::Aborted, None);
        assert!(aborted.commit().is_err());
        assert_eq!(aborted.installed.sha256, "a".repeat(64));
        journal.commit().unwrap();
        let value: Journal =
            serde_json::from_slice(&serde_json::to_vec(&journal).unwrap()).unwrap();
        assert_eq!(value.installed.sha256, "b".repeat(64));
        assert_eq!(value.operation.unwrap().status.phase, Phase::Succeeded);
    }
}
