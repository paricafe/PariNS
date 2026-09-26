//! Bounded local protocol. Root's journal, never these requests, owns installation.
use serde::{Deserialize, Serialize};

use super::{build_info::BuildInfo, contract::valid_sha256, reader::DownloadTuple};

pub const STATUS_PATH: &str = "/var/lib/parins-updater/status.json";
pub const HEALTH_PATH: &str = "/run/parins-managed/update-health.json";
pub const INBOX_NAME: &str = "update-request.json";
pub const LIVE_PATH: &str = "/opt/parins-managed/parins";
pub const INBOX_LIMIT: usize = 8192;
pub const STATUS_LIMIT: usize = 32768;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    pub config_revision: u64,
    pub material_digest: String,
}

/// Exact stdout contract of `parins --manage --check`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedCheckOutput {
    pub check: CheckReport,
    pub build_info: BuildInfo,
}

impl ManagedCheckOutput {
    pub fn validate_build(&self, expected: &BuildInfo) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.build_info == *expected,
            "preflight build identity differs"
        );
        anyhow::ensure!(
            self.check.config_revision > 0 && valid_sha256(&self.check.material_digest),
            "invalid installation preflight"
        );
        Ok(())
    }
}

pub fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn candidate_path(operation_id: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(valid_id(operation_id), "invalid operation identifier");
    Ok(std::path::Path::new("/opt/parins-managed/.update")
        .join(operation_id)
        .join("candidate"))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledIdentity {
    pub build: BuildInfo,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inbox {
    pub schema: u32,
    pub operation_id: String,
    pub phase_nonce: String,
    pub request: Request,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Stage {
        download: DownloadTuple,
        current_sha256: String,
        invocation_id: String,
        config_revision: u64,
    },
    Commit {
        invocation_id: String,
        config_revision: u64,
    },
    Abort {},
    VerifyRecovery {},
}

impl Inbox {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(bytes.len() <= INBOX_LIMIT, "oversized inbox");
        let result: Self = serde_json::from_slice(bytes)?;
        anyhow::ensure!(
            result.schema == 1 && valid_id(&result.operation_id) && valid_id(&result.phase_nonce),
            "invalid inbox identifiers"
        );
        match &result.request {
            Request::Stage {
                current_sha256,
                invocation_id,
                ..
            } => anyhow::ensure!(
                valid_sha256(current_sha256) && valid_id(invocation_id),
                "invalid stage identity"
            ),
            Request::Commit { invocation_id, .. } => {
                anyhow::ensure!(valid_id(invocation_id), "invalid commit identity")
            }
            _ => {}
        }
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Downloading,
    Staged,
    Committing,
    Stopping,
    Starting,
    Validating,
    RollingBack,
    AwaitingBootRecovery,
    AwaitingReadiness,
    Succeeded,
    RolledBack,
    Failed,
    Aborted,
    ManualRequired,
}

impl Phase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::RolledBack
                | Self::Failed
                | Self::Aborted
                | Self::ManualRequired
        )
    }
    pub fn precommit(self) -> bool {
        matches!(self, Self::Accepted | Self::Downloading | Self::Staged)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingLaunch {
    pub operation_id: String,
    pub launch_nonce: String,
    pub identity: InstalledIdentity,
    pub skip_cache_restore: bool,
    pub phase: Phase,
    pub phase_nonce: String,
    pub initialized: bool,
}

pub fn read_public_status() -> anyhow::Result<PublicStatus> {
    use std::{fs::File, io::Read};
    let meta = std::fs::symlink_metadata(STATUS_PATH)?;
    anyhow::ensure!(
        meta.is_file() && meta.len() <= STATUS_LIMIT as u64,
        "invalid helper status"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            meta.uid() == 0 && meta.mode() & 0o022 == 0 && meta.nlink() == 1,
            "untrusted helper status"
        );
    }
    let mut bytes = Vec::new();
    File::open(STATUS_PATH)?
        .take(STATUS_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= STATUS_LIMIT, "oversized helper status");
    let value: PublicStatus = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(value.schema == 1, "unknown helper status");
    Ok(value)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub operation_id: String,
    pub launch_nonce: String,
    pub identity: InstalledIdentity,
    pub config_revision: u64,
    pub dns_generation: u64,
    pub dns_ready: bool,
    pub storage_healthy: bool,
    pub storage_settings_applied: bool,
    pub sample_seq: u64,
    pub pid: u32,
    pub invocation_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationStatus {
    pub operation_id: String,
    pub phase_nonce: String,
    pub phase: Phase,
    pub version: String,
    pub reason: Option<String>,
    pub updated_at_ms: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub available: bool,
    pub reason: Option<String>,
    pub checked_at_ms: u64,
    pub helper_protocol: u32,
    pub install_contract: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicStatus {
    pub schema: u32,
    pub installed: InstalledIdentity,
    pub capability: Capability,
    pub active_operation: Option<OperationStatus>,
    pub last_operation: Option<OperationStatus>,
    pub pending_launch: Option<PendingLaunch>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inbox_rejects_unknown_fields_paths_and_unbounded_input() {
        let json = format!(
            r#"{{"schema":1,"operation_id":"{}","phase_nonce":"{}","request":{{"kind":"abort"}}}}"#,
            "a".repeat(32),
            "b".repeat(32)
        );
        assert!(Inbox::parse(json.as_bytes()).is_ok());
        assert!(
            Inbox::parse(
                json.replace("\"abort\"", "\"abort\",\"path\":\"/etc/passwd\"")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Inbox::parse(json.replace(&"a".repeat(32), "../candidate").as_bytes()).is_err());
        assert!(Inbox::parse(&vec![b' '; INBOX_LIMIT + 1]).is_err());
        assert!(candidate_path("../parins").is_err());
    }
}
