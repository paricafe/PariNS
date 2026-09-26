use super::*;
use std::os::unix::fs::symlink;

#[test]
fn lock_duplicate_is_not_inherited_by_spawned_child() {
    let fixture = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(fixture.path().join("lock")).unwrap();
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let handoff = rustix::io::dup(&file).unwrap();
    rustix::io::fcntl_setfd(&handoff, rustix::io::FdFlags::CLOEXEC).unwrap();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("10")
        .spawn()
        .unwrap();
    drop(file);
    drop(handoff);
    let next = std::fs::File::open(fixture.path().join("lock")).unwrap();
    let result = rustix::fs::flock(&next, rustix::fs::FlockOperation::NonBlockingLockExclusive);
    child.kill().unwrap();
    child.wait().unwrap();
    result.unwrap();
}
#[test]
fn nofollow_single_link_and_bounded_files() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = Dir::open(fixture.path(), false).unwrap();
    std::fs::write(fixture.path().join("normal"), b"hello").unwrap();
    assert_eq!(dir.read("normal", 5, false).unwrap(), b"hello");
    assert!(dir.read("normal", 4, false).is_err());
    symlink("normal", fixture.path().join("link")).unwrap();
    assert!(dir.read("link", 5, false).is_err());
    std::fs::hard_link(fixture.path().join("normal"), fixture.path().join("hard")).unwrap();
    assert!(dir.read("normal", 5, false).is_err());
    assert!(dir.read(".", 8192, false).is_err());
}

#[test]
fn malformed_or_oversized_existing_journal_is_not_missing() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = Dir::open(fixture.path(), false).unwrap();
    assert!(!dir.exists("journal.json").unwrap());
    std::fs::write(fixture.path().join("journal.json"), vec![b'x'; 32769]).unwrap();
    assert!(dir.exists("journal.json").unwrap());
    assert!(dir.read("journal.json", 32768, false).is_err());
    assert_eq!(
        std::fs::metadata(fixture.path().join("journal.json"))
            .unwrap()
            .len(),
        32769
    );
}
#[test]
fn every_replace_boundary_leaves_complete_old_or_new_envelope() {
    for boundary in 0..4 {
        let fixture = tempfile::tempdir().unwrap();
        let dir = Dir::open(fixture.path(), false).unwrap();
        dir.replace(
            "journal.json",
            br#"{"installed":"old","phase":"staged"}"#,
            0o600,
        )
        .unwrap();
        FAIL_AFTER.with(|s| s.set(Some(boundary)));
        assert!(
            dir.replace(
                "journal.json",
                br#"{"installed":"new","phase":"succeeded"}"#,
                0o600
            )
            .is_err()
        );
        FAIL_AFTER.with(|s| s.set(None));
        let value: serde_json::Value =
            serde_json::from_slice(&dir.read("journal.json", 8192, false).unwrap()).unwrap();
        assert!(
            value == serde_json::json!({"installed":"old","phase":"staged"})
                || value == serde_json::json!({"installed":"new","phase":"succeeded"})
        );
    }
}
#[test]
fn every_binary_rename_boundary_retains_live_file() {
    for boundary in 0..3 {
        let fixture = tempfile::tempdir().unwrap();
        let dir = Dir::open(fixture.path(), false).unwrap();
        std::fs::write(fixture.path().join("parins"), b"old").unwrap();
        std::fs::write(fixture.path().join("candidate"), b"new").unwrap();
        FAIL_AFTER.with(|s| s.set(Some(boundary)));
        assert!(dir.rename_to("candidate", &dir, "parins").is_err());
        FAIL_AFTER.with(|s| s.set(None));
        assert_eq!(dir.read("parins", 3, false).unwrap(), b"new");
    }
}
#[test]
fn explicit_modes_survive_private_umask_without_process_global_changes() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let dir = Dir::open(fixture.path(), false).unwrap();
    dir.replace("status.json", b"{}", 0o644).unwrap();
    assert_eq!(
        std::fs::metadata(fixture.path().join("status.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}
