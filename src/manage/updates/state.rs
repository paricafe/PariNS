use super::*;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
};

#[cfg(test)]
pub(super) static FAIL_DIRECTORY_SYNC: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckState {
    pub validated: bool,
    pub etag: Option<String>,
    pub last_check_at_ms: Option<u64>,
    pub last_success_at_ms: Option<u64>,
    pub next_check_at_ms: u64,
    pub retry_at_ms: u64,
    pub failures: u32,
    pub error: Option<String>,
    pub manual: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidate {
    pub version: String,
    pub tag: String,
    pub published_at: Option<String>,
    pub notes: String,
    pub manual_reason: Option<String>,
    pub manifest: Option<Manifest>,
    pub download: Option<DownloadTuple>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub plan_id: String,
    pub expires_at_ms: u64,
    pub config_revision: u64,
    pub current_sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Operation {
    pub plan_id: String,
    pub expected_version: String,
    pub config_revision: u64,
    pub operation_id: String,
    pub phase_nonce: String,
    pub invocation_id: String,
    pub download: DownloadTuple,
    pub manifest: Manifest,
    pub phase: String,
    pub reason: Option<String>,
    pub finished: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppState {
    pub schema: u32,
    pub check: CheckState,
    pub candidate: Option<Candidate>,
    pub plan: Option<Plan>,
    pub operation: Option<Operation>,
    pub commit_intent: Option<String>,
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            schema: 1,
            check: CheckState::default(),
            candidate: None,
            plan: None,
            operation: None,
            commit_intent: None,
        }
    }
}
pub(super) fn read(dir: &Path) -> Result<Option<AppState>> {
    let Some(bytes) = store::read_bounded(&dir.join("update-state.json"), 256 * 1024)? else {
        return Ok(None);
    };
    let state: AppState = serde_json::from_slice(&bytes)?;
    validate(&state)?;
    Ok(Some(state))
}
pub(super) fn write(dir: &Path, state: &AppState) -> Result<()> {
    validate(state)?;
    let bytes = serde_json::to_vec(state)?;
    ensure!(bytes.len() <= 256 * 1024, "update state too large");
    atomic(dir, "update-state.json", &bytes)
}
fn validate(state: &AppState) -> Result<()> {
    ensure!(state.schema == 1, "unknown update state");
    if let Some(plan) = &state.plan {
        ensure!(
            ipc::valid_id(&plan.plan_id)
                && crate::update::contract::valid_sha256(&plan.current_sha256)
                && state.candidate.is_some(),
            "invalid update plan"
        );
    }
    if let Some(op) = &state.operation {
        ensure!(
            ipc::valid_id(&op.plan_id)
                && ipc::valid_id(&op.operation_id)
                && ipc::valid_id(&op.phase_nonce)
                && ipc::valid_id(&op.invocation_id),
            "invalid update operation identifiers"
        );
        op.manifest.validate()?;
        ensure!(
            op.expected_version == op.manifest.version && op.download.tag == op.manifest.tag,
            "invalid update operation binding"
        );
    }
    if let Some(intent) = &state.commit_intent {
        ensure!(
            state
                .operation
                .as_ref()
                .is_some_and(|op| &op.operation_id == intent && !op.finished),
            "orphaned update commit intent"
        );
    }
    Ok(())
}
pub(super) fn atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let target = dir.join(name);
    store::checked_open(&target, false)?;
    // Each caller has one process-owned writer for its fixed destination.
    // Reuse that reserved temporary name so crashes cannot accumulate files.
    let temp = dir.join(format!(".{name}.tmp"));
    if store::checked_open(&temp, false)?.is_some() {
        std::fs::remove_file(&temp)?;
    }
    let result = (|| {
        let mut file = store::create_private(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)?;
        #[cfg(test)]
        if name == "update-state.json"
            && FAIL_DIRECTORY_SYNC.lock().unwrap().as_deref() == Some(dir)
        {
            anyhow::bail!("injected directory sync failure");
        }
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temp);
    result
}
pub(super) fn own_identity() -> Result<ipc::InstalledIdentity> {
    #[cfg(target_os = "linux")]
    let path = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let path = std::env::current_exe()?;
    let mut file = File::open(path)?;
    ensure!(file.metadata()?.is_file(), "program must be a regular file");
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    let mut size = 0;
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        };
        size += n;
        ensure!(size <= 128 * 1024 * 1024, "program too large");
        hash.update(&bytes[..n]);
    }
    Ok(ipc::InstalledIdentity {
        build: BuildInfo::current(),
        sha256: format!("{:x}", hash.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_intent_is_rejected_without_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            commit_intent: Some("a".repeat(32)),
            ..AppState::default()
        };
        let bytes = serde_json::to_vec(&state).unwrap();
        atomic(dir.path(), "update-state.json", &bytes).unwrap();
        assert!(read(dir.path()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("update-state.json")).unwrap(),
            bytes
        );
        assert!(write(dir.path(), &state).is_err());
    }

    #[test]
    fn reserved_temporary_is_reclaimed_without_touching_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut partial =
            store::create_private(&dir.path().join(".update-state.json.tmp")).unwrap();
        partial.write_all(b"partial").unwrap();
        let unknown = dir.path().join(".unrelated.tmp");
        std::fs::write(&unknown, b"keep").unwrap();
        write(dir.path(), &AppState::default()).unwrap();
        assert!(read(dir.path()).unwrap().is_some());
        assert!(!dir.path().join(".update-state.json.tmp").exists());
        assert_eq!(std::fs::read(unknown).unwrap(), b"keep");
    }
}
