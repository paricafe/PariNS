use super::super::download::REPRESENTATION;
use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("subscriptions"), 8 * 1024 * 1024, 100).unwrap();
    (dir, store)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn candidate(store: &mut Store, id: &str, text: &[u8], now: u64) -> (Staging, PreparedMetadata) {
    let mut stage = store.begin_staging(text.len() as u64, now).unwrap();
    stage.file.write_all(text).unwrap();
    let sha256 = digest(text);
    let metadata = PreparedMetadata {
        fingerprint: digest(id.as_bytes()),
        sha256: sha256.clone(),
        bytes: text.len() as u64,
        rules: 1,
        validators: Validators {
            final_url: "https://example.com/list".into(),
            representation: REPRESENTATION.into(),
            content_sha256: sha256,
            etag: Some("\"fixture\"".into()),
            last_modified: None,
        },
    };
    (stage, metadata)
}
fn prepare(store: &mut Store, id: &str, text: &[u8], now: u64) -> String {
    let (stage, metadata) = candidate(store, id, text, now);
    let hash = metadata.sha256.clone();
    assert_eq!(
        store.prepare(stage, metadata, now).unwrap(),
        CommitOutcome::Durable
    );
    hash
}

#[test]
fn empty_catalog_precedes_objects_and_reopen_uses_only_authority() {
    let (dir, mut store) = store();
    let path = store.dir.clone();
    assert!(path.join(CATALOG).is_file());
    assert!(store.catalog.records.is_empty());
    assert!(Store::open(&path, store.quota, 100).is_err());
    prepare(&mut store, "one", b".example.com\n", 100);
    drop(store);
    let reopened = Store::open(&path, 8 * 1024 * 1024, 101).unwrap();
    let (_, mut file) = reopened
        .read_prepared(&digest(b"one"), 101)
        .unwrap()
        .unwrap();
    let mut bytes = String::new();
    file.read_to_string(&mut bytes).unwrap();
    assert_eq!(bytes, ".example.com\n");
    drop(reopened);
    fs::remove_file(path.join(CATALOG)).unwrap();
    assert!(Store::open(&path, 8 * 1024 * 1024, 102).is_err());
    assert_eq!(fs::read_dir(path.join("objects")).unwrap().count(), 1);
    drop(dir);
}

#[test]
fn bad_authority_and_oversize_never_reset_or_scan_objects() {
    let (_dir, mut store) = store();
    prepare(&mut store, "one", b"a.test\n", 100);
    let path = store.dir.clone();
    drop(store);
    fs::write(path.join(CATALOG), b"{broken").unwrap();
    assert!(Store::open(&path, 8 * 1024 * 1024, 100).is_err());
    assert_eq!(fs::read(path.join(CATALOG)).unwrap(), b"{broken");
    assert_eq!(fs::read_dir(path.join("objects")).unwrap().count(), 1);
    fs::write(path.join(CATALOG), vec![b' '; MAX_CATALOG as usize + 1]).unwrap();
    assert!(Store::open(&path, 8 * 1024 * 1024, 100).is_err());
}

#[test]
fn failed_validation_or_precommit_write_keeps_old_content_and_validators() {
    let (_dir, mut store) = store();
    let old = prepare(&mut store, "one", b"old.test\n", 100);
    for fault in ["write", "rename", "digest", "binding"] {
        let (stage, mut metadata) = candidate(&mut store, "one", b"new.test\n", 101);
        if fault == "digest" {
            metadata.sha256 = digest(b"wrong");
            metadata.validators.content_sha256 = metadata.sha256.clone();
        } else if fault == "binding" {
            metadata.validators.content_sha256 = digest(b"wrong");
        } else {
            store.fault = Some(fault);
        }
        assert!(store.prepare(stage, metadata, 101).is_err());
        store.fault = None;
        let (record, _) = store.read_prepared(&digest(b"one"), 101).unwrap().unwrap();
        assert_eq!(record.sha256, old);
        assert_eq!(record.validators.content_sha256, old);
        assert_eq!(record.validators.etag.as_deref(), Some("\"fixture\""));
    }
}

#[test]
fn postrename_sync_uncertainty_publishes_new_authority_and_holds_both_objects() {
    let (_dir, mut store) = store();
    let old = prepare(&mut store, "one", b"old.test\n", 100);
    let (stage, metadata) = candidate(&mut store, "one", b"new.test\n", 101);
    let new = metadata.sha256.clone();
    store.fault = Some("directory_sync");
    assert_eq!(
        store.prepare(stage, metadata, 101).unwrap(),
        CommitOutcome::CommittedUncertain
    );
    assert_eq!(
        store
            .read_prepared(&digest(b"one"), 101)
            .unwrap()
            .unwrap()
            .0
            .sha256,
        new
    );
    assert!(store.object_path(&old).exists() && store.object_path(&new).exists());
    assert!(store.begin_staging(10, 102).is_err());
    assert!(store.collect(102, true).is_err());
    store.fault = None;
    store.reconcile_sync().unwrap();
    store.collect(102, false).unwrap();
    assert!(!store.object_path(&old).exists());
    assert!(store.object_path(&new).exists());
}

#[test]
fn current_previous_and_work_pins_survive_expiry_and_pressure() {
    let (_dir, mut store) = store();
    let current = prepare(&mut store, "current", b"current.test\n", 100);
    let previous = prepare(&mut store, "previous", b"previous.test\n", 100);
    let pinned = prepare(&mut store, "pinned", b"pinned.test\n", 100);
    let discard = prepare(&mut store, "discard", b"discard.test\n", 100);
    let pin = store.pin(&pinned).unwrap();
    // Test-only transaction primitive: real configuration publication is not exposed.
    let mut catalog = store.catalog.clone();
    catalog.current = vec![digest(b"current")];
    catalog.previous = vec![digest(b"previous")];
    store.commit_catalog(catalog).unwrap();
    let stage = store.begin_staging(10, 101).unwrap();
    let metadata = PreparedMetadata {
        fingerprint: digest(b"current"),
        sha256: current.clone(),
        bytes: 1,
        rules: 1,
        validators: store.catalog.records[0].validators.clone(),
    };
    assert!(store.prepare(stage, metadata, 101).is_err());
    store.collect(100 + PREPARED_TTL, true).unwrap();
    for hash in [current, previous, pinned.clone()] {
        assert!(store.object_path(&hash).exists());
    }
    assert!(!store.object_path(&discard).exists());
    drop(pin);
    store.collect(100 + PREPARED_TTL, false).unwrap();
    assert!(!store.object_path(&pinned).exists());
}

#[test]
fn prepared_slots_bounded_and_unknown_files_never_deleted() {
    let (_dir, mut store) = store();
    let unknown = store.dir.join("objects/note.txt");
    fs::write(&unknown, b"not owned").unwrap();
    for i in 0..20 {
        prepare(
            &mut store,
            &format!("source-{i}"),
            format!("{i}.test\n").as_bytes(),
            100 + i,
        );
    }
    store.collect(120, false).unwrap();
    assert_eq!(store.catalog.records.len(), 16);
    assert_eq!(fs::read_dir(store.dir.join("objects")).unwrap().count(), 17);
    assert!(
        store
            .read_prepared(&digest(b"source-0"), 120)
            .unwrap()
            .is_none()
    );
    store.collect(120 + PREPARED_TTL, false).unwrap();
    assert!(store.catalog.records.is_empty());
    assert_eq!(fs::read(unknown).unwrap(), b"not owned");
}

#[test]
fn disk_budget_counts_unknown_and_catalog_overlap_and_cancelled_staging() {
    let (_dir, mut store) = store();
    let used = store.disk_bytes().unwrap();
    store.quota = used + MAX_CATALOG + 9;
    assert!(store.begin_staging(10, 100).is_err());
    store.quota += 1;
    let mut stage = store.begin_staging(10, 100).unwrap();
    stage.file.write_all(b"partial").unwrap();
    let path = stage.path.clone();
    assert_eq!(store.disk_bytes().unwrap(), used + 7);
    drop(stage);
    assert!(store.begin_staging(1, 100).is_err());
    assert_eq!(fs::read(path).unwrap(), b"partial");
    let dir = store.dir.clone();
    drop(store);
    let mut reopened = Store::open(&dir, 8 * 1024 * 1024, 100).unwrap();
    assert_eq!(fs::read_dir(dir.join("staging")).unwrap().count(), 0);
    fs::write(dir.join("unrelated.bin"), vec![0; 100]).unwrap();
    assert_eq!(reopened.disk_bytes().unwrap(), used + 100);
    let stage = reopened.begin_staging(1, 100).unwrap();
    reopened.discard(stage).unwrap();
    assert!(dir.join("unrelated.bin").exists());
}

#[cfg(unix)]
#[test]
fn private_files_and_directory_links_are_rejected_without_external_mutation() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    let object = store.object_path(&hash);
    assert_eq!(
        fs::metadata(&store.dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&object).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let outside = store.dir.parent().unwrap().join("outside");
    create_private(&outside)
        .unwrap()
        .write_all(b"one.test\n")
        .unwrap();
    fs::remove_file(&object).unwrap();
    symlink(&outside, &object).unwrap();
    assert!(store.read_prepared(&digest(b"one"), 100).is_err());
    assert!(store.collect(100 + PREPARED_TTL, false).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"one.test\n");
    fs::remove_file(&object).unwrap();
    fs::hard_link(&outside, &object).unwrap();
    assert!(store.read_prepared(&digest(b"one"), 100).is_err());
    let path = store.dir.clone();
    drop(store);
    let link = path.parent().unwrap().join("linked");
    symlink(&path, &link).unwrap();
    assert!(Store::open(&link, 8 * 1024 * 1024, 100).is_err());
}

#[test]
fn catalog_limits_and_duplicate_references_are_authoritative() {
    let (_dir, mut store) = store();
    prepare(&mut store, "one", b"one.test\n", 100);
    let mut catalog = store.catalog.clone();
    catalog.schema = 2;
    assert!(validate_catalog(&catalog).is_err());
    catalog.schema = 1;
    catalog.records.push(catalog.records[0].clone());
    assert!(validate_catalog(&catalog).is_err());
    catalog.records.pop();
    catalog.current = vec![digest(b"absent")];
    assert!(validate_catalog(&catalog).is_err());
    catalog.current = vec![digest(b"one"); 2];
    assert!(validate_catalog(&catalog).is_err());
}

#[test]
fn catalog_supports_exactly_16_current_16_previous_16_prepared() {
    let (_dir, mut store) = store();
    prepare(&mut store, "one", b"one.test\n", 100);
    let record = store.catalog.records[0].clone();
    let mut catalog = Catalog::default();
    for i in 0..48 {
        let mut entry = record.clone();
        entry.fingerprint = digest(format!("source-{i}").as_bytes());
        if i < 16 {
            catalog.current.push(entry.fingerprint.clone());
        } else if i < 32 {
            catalog.previous.push(entry.fingerprint.clone());
        }
        catalog.records.push(entry);
    }
    assert_eq!(unreferenced(&catalog).count(), 16);
    store.commit_catalog(catalog.clone()).unwrap();
    catalog.records.push(record);
    assert!(store.commit_catalog(catalog).is_err());
    assert_eq!(store.catalog.records.len(), 48);
}

#[test]
fn all_prepared_slots_pinned_reject_extra_without_evicting_existing() {
    let (_dir, mut store) = store();
    let mut pins = Vec::new();
    for i in 0..16 {
        let hash = prepare(
            &mut store,
            &i.to_string(),
            format!("{i}.test\n").as_bytes(),
            100,
        );
        pins.push(store.pin(&hash).unwrap());
    }
    let (stage, metadata) = candidate(&mut store, "overflow", b"overflow.test\n", 101);
    assert!(store.prepare(stage, metadata, 101).is_err());
    assert_eq!(store.catalog.records.len(), 16);
    for i in 0..16 {
        assert!(
            store
                .read_prepared(&digest(i.to_string().as_bytes()), 101)
                .unwrap()
                .is_some()
        );
    }
    drop(pins);
}

#[test]
fn selected_corrupt_object_can_be_repaired_but_unknown_hash_name_cannot() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    fs::write(store.object_path(&hash), b"corrupt").unwrap();
    assert!(store.read_prepared(&digest(b"one"), 100).is_err());
    prepare(&mut store, "one", b"one.test\n", 101);
    assert!(store.read_prepared(&digest(b"one"), 101).unwrap().is_some());
    let unknown = store.object_path(&digest(b"other.test\n"));
    create_private(&unknown)
        .unwrap()
        .write_all(b"unowned")
        .unwrap();
    let (stage, metadata) = candidate(&mut store, "other", b"other.test\n", 102);
    assert!(store.prepare(stage, metadata, 102).is_err());
    assert_eq!(fs::read(unknown).unwrap(), b"unowned");
}

#[test]
fn maintenance_uncertainty_blocks_new_staging_until_reconciled() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    store.fault = Some("directory_sync");
    assert!(store.begin_staging(1, 100 + PREPARED_TTL).is_err());
    assert!(store.uncertain);
    assert!(store.object_path(&hash).exists());
    assert_eq!(fs::read_dir(store.dir.join("staging")).unwrap().count(), 0);
    store.fault = None;
    store.reconcile_sync().unwrap();
    let stage = store.begin_staging(1, 100 + PREPARED_TTL).unwrap();
    store.discard(stage).unwrap();
    assert!(!store.object_path(&hash).exists());
}

#[test]
fn staging_and_work_pins_hold_exclusive_directory_lease() {
    let (_dir, mut store) = store();
    let path = store.dir.clone();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    let pin = store.pin(&hash).unwrap();
    let stage = store.begin_staging(1, 101).unwrap();
    let writer_lease = stage.lease.clone();
    drop(store);
    assert!(Store::open(&path, 8 * 1024 * 1024, 101).is_err());
    drop(stage);
    assert!(Store::open(&path, 8 * 1024 * 1024, 101).is_err());
    drop(pin);
    assert!(Store::open(&path, 8 * 1024 * 1024, 101).is_err());
    drop(writer_lease);
    assert!(Store::open(&path, 8 * 1024 * 1024, 101).is_ok());
}
