//! Exercise the real producer, not a hand-written approximation of its stdout.
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use parins::update::{build_info::BuildInfo, ipc::ManagedCheckOutput};
use std::{fs, process::Command};

#[test]
fn managed_check_stdout_is_the_installation_preflight_contract() {
    let state = tempfile::tempdir().unwrap();
    let salt = SaltString::encode_b64(b"test-only-salt16").unwrap();
    let password_hash = Argon2::default()
        .hash_password(b"test-only-password", &salt)
        .unwrap()
        .to_string();
    let saved = serde_json::json!({
        "username": "admin",
        "password_hash": password_hash,
        "toml": include_str!("../parins.example.toml"),
        "previous": null,
        "revision": 7,
    });
    let path = state.path().join("state.json");
    fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before = fs::read(&path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_parins"))
        .args(["--manage", "--check", "--state-dir"])
        .arg(state.path())
        .args(["--web-listen", "127.0.0.1:3000"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: ManagedCheckOutput = serde_json::from_slice(&output.stdout).unwrap();
    report.validate_build(&BuildInfo::current()).unwrap();
    assert_eq!(report.check.config_revision, 7);
    let mut other = BuildInfo::current();
    other.source_commit = "0".repeat(40);
    assert!(report.validate_build(&other).is_err());
    let mut unknown: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    unknown["extra"] = true.into();
    assert!(serde_json::from_value::<ManagedCheckOutput>(unknown).is_err());
    assert!(
        serde_json::from_value::<ManagedCheckOutput>(serde_json::to_value(&report.check).unwrap())
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 1);
}
