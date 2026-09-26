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
fn download_admission_stops_on_pressure_catalog_directory_sync_uncertainty() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "unused", b"unused.test\n", 100);
    store.set_quota(MAX_OBJECT + MAX_CATALOG).unwrap();
    store.fault = Some("directory_sync");
    assert!(store.collect_for_download(101, true).is_err());
    assert!(store.catalog.records.is_empty());
    assert!(store.object_path(&hash).exists());
    assert!(store.begin_staging(MAX_OBJECT, 101).is_err());
    assert!(!*store.staging.lock().unwrap());
    store.fault = None;
    store.reconcile_sync().unwrap();
    store.collect_for_download(101, true).unwrap();
    assert!(!store.object_path(&hash).exists());
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
    // Disabled/new configured sources have GC references before their first LKG.
    assert!(validate_catalog(&catalog).is_ok());
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
    store.collect(100 + PREPARED_TTL, false).unwrap();
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

#[test]
fn installed_candidates_are_unselected_and_pins_protect_batched_compile() {
    let (_dir, mut store) = store();
    let old = prepare(&mut store, "one", b"old.test\n", 100);
    store
        .set_references(vec![digest(b"one")], vec![], 100)
        .unwrap();
    let revision = store.revision();
    let (stage, metadata) = candidate(&mut store, "one", b"new.test\n", 101);
    let new = store.install_object(stage, metadata, 101).unwrap();
    let pin = store.pin(&new.sha256).unwrap();
    let (stage, metadata) = candidate(&mut store, "two", b"two.test\n", 101);
    let second = store.install_object(stage, metadata, 101).unwrap();
    let second_pin = store.pin(&second.sha256).unwrap();
    assert_eq!(store.record(&digest(b"one")).unwrap().sha256, old);
    assert_eq!(store.revision(), revision);
    store.collect(102, true).unwrap();
    assert!(store.object_path(&new.sha256).exists());
    assert!(store.object_path(&second.sha256).exists());
    assert!(
        store
            .commit_records(vec![new.clone(), second.clone()], revision + 1, 102)
            .is_err()
    );
    assert_eq!(store.record(&digest(b"one")).unwrap().sha256, old);
    store
        .commit_records(vec![new.clone(), second], revision, 102)
        .unwrap();
    assert_eq!(store.revision(), revision + 1);
    assert_eq!(store.verified_content(&digest(b"one")).unwrap().0, new);
    drop((pin, second_pin));
}

#[test]
fn batch_commit_failures_have_exact_rename_semantics() {
    for fault in ["write", "rename", "directory_sync"] {
        let (_dir, mut store) = store();
        let old = prepare(&mut store, "one", b"old.test\n", 100);
        store
            .set_references(vec![digest(b"one")], vec![], 100)
            .unwrap();
        let revision = store.revision();
        let (stage, metadata) = candidate(&mut store, "one", b"new.test\n", 101);
        let new = store.install_object(stage, metadata, 101).unwrap();
        let pin = store.pin(&new.sha256).unwrap();
        store.fault = Some(fault);
        let result = store.commit_records(vec![new.clone()], revision, 101);
        if fault == "directory_sync" {
            assert_eq!(result.unwrap(), CommitOutcome::CommittedUncertain);
            assert_eq!(store.record(&digest(b"one")).unwrap().sha256, new.sha256);
            assert_eq!(store.revision(), revision + 1);
            assert!(store.set_references(vec![], vec![], 102).is_err());
        } else {
            assert!(result.is_err());
            assert_eq!(store.record(&digest(b"one")).unwrap().sha256, old);
            assert_eq!(store.revision(), revision);
        }
        assert!(store.object_path(&old).exists());
        assert!(store.object_path(&new.sha256).exists());
        drop(pin);
    }
}

#[test]
fn metadata_and_reference_updates_do_not_advance_content_revision() {
    let (_dir, mut store) = store();
    prepare(&mut store, "one", b"one.test\n", 100);
    let revision = store.revision();
    store
        .set_references(vec![digest(b"one"), digest(b"disabled")], vec![], 101)
        .unwrap();
    assert_eq!(store.revision(), revision);
    let mut record = store.record(&digest(b"one")).unwrap().clone();
    record.prepared_at = 102;
    record.validators.etag = Some("\"new-metadata\"".into());
    store
        .commit_records(vec![record.clone()], revision, 102)
        .unwrap();
    assert_eq!(store.revision(), revision);
    assert_eq!(store.record(&digest(b"one")).unwrap(), &record);
    let path = store.dir.clone();
    drop(store);
    let reopened = Store::open(&path, 8 * 1024 * 1024, 102 + PREPARED_TTL).unwrap();
    assert_eq!(reopened.revision(), revision);
    assert_eq!(
        reopened.verified_content(&digest(b"one")).unwrap().0,
        record
    );
}

#[test]
fn reopen_waits_for_authoritative_config_roots_before_expiring_preparations() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "new-config-source", b"new.test\n", 100);
    let path = store.dir.clone();
    // Model Config having been saved immediately before the process stopped,
    // without the follow-up catalog GC-root synchronization.
    drop(store);
    let now = 100 + PREPARED_TTL;
    let mut reopened = Store::open(&path, 8 * 1024 * 1024, now).unwrap();
    reopened
        .set_references(vec![digest(b"new-config-source")], vec![], now)
        .unwrap();
    reopened.collect(now, true).unwrap();
    assert!(reopened.object_path(&hash).exists());
}

#[test]
fn read_only_does_not_create_lock_or_change_authority_even_with_writer_present() {
    let (dir, mut store) = store();
    assert!(Store::read_only(&dir.path().join("missing")).is_err());
    assert!(!dir.path().join("missing").exists());
    prepare(&mut store, "one", b"one.test\n", 100);
    let path = store.dir.clone();
    let before = fs::read(path.join(CATALOG)).unwrap();
    let view = Store::read_only(&path).unwrap();
    assert_eq!(view.revision(), store.revision());
    assert_eq!(view.records(), store.records());
    assert_eq!(
        view.verified_content(&digest(b"one")).unwrap().0.sha256,
        digest(b"one.test\n")
    );
    drop(view);
    drop(store);
    fs::remove_file(path.join("lock")).unwrap();
    let view = Store::read_only(&path).unwrap();
    assert!(view.verified_content(&digest(b"absent")).is_err());
    assert!(!path.join("lock").exists());
    assert_eq!(fs::read(path.join(CATALOG)).unwrap(), before);
    fs::write(path.join(CATALOG), b"broken").unwrap();
    assert!(Store::read_only(&path).is_err());
    assert_eq!(fs::read(path.join(CATALOG)).unwrap(), b"broken");
}

#[test]
fn active_corrupt_and_missing_objects_repair_without_changing_selected_revision() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    store
        .set_references(vec![digest(b"one")], vec![], 100)
        .unwrap();
    let revision = store.revision();
    for missing in [false, true] {
        if missing {
            fs::remove_file(store.object_path(&hash)).unwrap();
        } else {
            fs::write(store.object_path(&hash), b"broken").unwrap();
        }
        assert!(store.verified_content(&digest(b"one")).is_err());
        let (stage, metadata) = candidate(&mut store, "one", b"one.test\n", 101);
        let repaired = store.install_object(stage, metadata, 101).unwrap();
        assert_eq!(store.revision(), revision);
        assert_eq!(
            store.verified_content(&digest(b"one")).unwrap().0.sha256,
            hash
        );
        store.commit_records(vec![repaired], revision, 101).unwrap();
        assert_eq!(store.revision(), revision);
    }
}

#[test]
fn status_errors_are_bounded_and_persist_without_valid_content_or_revision_change() {
    let (_dir, mut store) = store();
    let hash = prepare(&mut store, "one", b"one.test\n", 100);
    let revision = store.revision();
    fs::remove_file(store.object_path(&hash)).unwrap();
    let status = SourceStatus {
        last_attempt: Some(101),
        next_update: Some(401),
        failures: 1,
        last_error: Some("request_timeout".into()),
        error_line: None,
    };
    store
        .update_status(&digest(b"one"), status.clone())
        .unwrap();
    assert_eq!(store.revision(), revision);
    assert_eq!(store.record(&digest(b"one")).unwrap().status, status);
    assert_eq!(store.record(&digest(b"one")).unwrap().prepared_at, 100);
    let mut oversized = status.clone();
    oversized.last_error = Some("x".repeat(513));
    assert!(store.update_status(&digest(b"one"), oversized).is_err());
    assert_eq!(store.record(&digest(b"one")).unwrap().status, status);
}

fn index_fixture(digest: &str, payload: &[u8]) -> Vec<u8> {
    let mut bytes = crate::policy::radix::DERIVED_PREFIX.to_vec();
    for i in 0..32 {
        bytes.push(u8::from_str_radix(&digest[2 * i..2 * i + 2], 16).unwrap());
    }
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn derived_index_stream_is_bounded_atomic_and_repairable_without_catalog_mutation() {
    let (_dir, mut store) = store();
    let input = digest(b"input");
    let first = index_fixture(&input, b"codec-owned body");
    let before = fs::read(store.dir.join(CATALOG)).unwrap();
    store
        .write_index(&input, first.len() as u64, 100, |out| {
            out.write_all(&first)?;
            Ok(())
        })
        .unwrap();
    let path = store.dir.join("indexes").join(format!("{input}.bin"));
    assert_eq!(fs::read(&path).unwrap(), first);
    let index_pin = store.pin_index(&input).unwrap();
    assert!(
        store
            .write_index(&input, first.len() as u64 - 1, 101, |out| {
                out.write_all(&first)?;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), first);
    assert_eq!(fs::read_dir(store.dir.join("staging")).unwrap().count(), 0);
    fs::write(&path, b"damaged derived index").unwrap();
    assert!(store.read_index(&input, 1024).is_err());
    store
        .write_index(&input, first.len() as u64, 102, |out| {
            out.write_all(&first)?;
            Ok(())
        })
        .unwrap();
    let mut file = Store::read_only(&store.dir)
        .unwrap()
        .read_index(&input, 1024)
        .unwrap()
        .unwrap();
    let mut read = Vec::new();
    file.read_to_end(&mut read).unwrap();
    assert_eq!(read, first);
    assert_eq!(fs::read(store.dir.join(CATALOG)).unwrap(), before);
    drop(index_pin);
}

#[test]
fn derived_index_gc_and_disk_quota_respect_work_pins_and_unknown_files() {
    let (_dir, mut store) = store();
    let input = digest(b"input");
    let bytes = index_fixture(&input, &[0; 100]);
    store
        .write_index(&input, bytes.len() as u64, 100, |out| {
            out.write_all(&bytes)?;
            Ok(())
        })
        .unwrap();
    let pin = store.pin_index(&input).unwrap();
    let unknown_path = store
        .dir
        .join("indexes")
        .join(format!("{}.bin", digest(b"unknown")));
    create_private(&unknown_path)
        .unwrap()
        .write_all(b"unowned")
        .unwrap();
    let used = store.disk_bytes().unwrap();
    store.quota = used + MAX_CATALOG + 9;
    assert!(store.begin_staging(10, 101).is_err());
    assert!(store.read_index(&input, 1024).unwrap().is_some());
    assert_eq!(fs::read(&unknown_path).unwrap(), b"unowned");
    drop(pin);
    let stage = store.begin_staging(10, 102).unwrap();
    store.discard(stage).unwrap();
    assert!(store.read_index(&input, 1024).unwrap().is_none());
    assert_eq!(fs::read(unknown_path).unwrap(), b"unowned");
}

#[test]
fn tiny_derived_generations_are_collected_without_byte_pressure() {
    let (_dir, mut store) = store();
    for i in 0..40 {
        let input = digest(format!("input-{i}").as_bytes());
        let bytes = index_fixture(&input, b"small body");
        store
            .write_index(&input, bytes.len() as u64, 100, |out| {
                out.write_all(&bytes)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(fs::read_dir(store.dir.join("indexes")).unwrap().count(), 1);
    }
}

#[test]
fn metadata_capacity_is_bounded_not_just_serialized_length() {
    let (_dir, mut store) = store();
    prepare(&mut store, "one", b"one.test\n", 100);
    let mut record = store.records()[0].clone();
    record.validators.final_url.reserve(32 * 1024);
    assert!(validate_record(&record).is_err());
    let mut catalog = store.catalog.clone();
    catalog.records.reserve(4096);
    assert!(validate_catalog(&catalog).is_err());
}

#[test]
fn expired_pre_rename_check_preserves_catalog_selection_and_revision() {
    let (_dir, mut store) = store();
    let old = prepare(&mut store, "one", b"old.test\n", 100);
    let revision = store.revision();
    let before = fs::read(store.dir.join(CATALOG)).unwrap();
    let (stage, metadata) = candidate(&mut store, "one", b"new.test\n", 101);
    let replacement = store.install_object(stage, metadata, 101).unwrap();
    let called = std::cell::Cell::new(false);
    let result = store.commit_batch_with_check(vec![replacement], &[], revision, || {
        called.set(true);
        anyhow::bail!("expired");
    });
    assert!(called.get());
    assert!(result.is_err());
    assert_eq!(store.revision(), revision);
    assert_eq!(store.record(&digest(b"one")).unwrap().sha256, old);
    assert_eq!(fs::read(store.dir.join(CATALOG)).unwrap(), before);
}
