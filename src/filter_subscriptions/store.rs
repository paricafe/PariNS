//! Low-privilege source objects and their sole selection authority.
//! Object installation and catalog selection are separate transactions. The
//! service owns aggregate validation and policy publication around these calls.
use super::download::Validators;
use crate::private_files::{checked_open, create_private, private_permissions};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_CATALOG: u64 = 256 * 1024;
const MAX_OBJECT: u64 = 16 * 1024 * 1024;
const MAX_INDEX: u64 = 512 * 1024 * 1024;
const PREPARED_TTL: u64 = 24 * 60 * 60;
const CATALOG: &str = "catalog.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRecord {
    pub fingerprint: String,
    pub sha256: String,
    pub bytes: u64,
    pub rules: u64,
    pub validators: Validators,
    pub prepared_at: u64,
    pub status: SourceStatus,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceStatus {
    pub last_attempt: Option<u64>,
    pub next_update: Option<u64>,
    pub failures: u32,
    pub last_error: Option<String>,
    pub error_line: Option<u64>,
}
pub(crate) struct PreparedMetadata {
    pub fingerprint: String,
    pub sha256: String,
    pub bytes: u64,
    pub rules: u64,
    pub validators: Validators,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema: u32,
    content_revision: u64,
    current: Vec<String>,
    previous: Vec<String>,
    records: Vec<SourceRecord>,
}
impl Default for Catalog {
    fn default() -> Self {
        Self {
            schema: 1,
            content_revision: 0,
            current: vec![],
            previous: vec![],
            records: vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Durable,
    CommittedUncertain,
}

pub(crate) struct Staging {
    pub file: File,
    pub(super) lease: Arc<File>,
    path: PathBuf,
    reserved: u64,
    active: Arc<Mutex<bool>>,
    completed: bool,
}
impl Drop for Staging {
    fn drop(&mut self) {
        // Cancellation can leave a staging writer's blocking write alive. Only an
        // explicit completion after the caller drained that writer releases
        // the reservation. An abandoned operation keeps this Store busy.
        if self.completed {
            let _ = fs::remove_file(&self.path);
            *self.active.lock().unwrap() = false;
        }
    }
}

pub(crate) struct WorkPin {
    hash: String,
    pins: Arc<Mutex<BTreeMap<String, usize>>>,
    _lease: Arc<File>,
}
impl Drop for WorkPin {
    fn drop(&mut self) {
        let mut pins = self.pins.lock().unwrap();
        if let Some(count) = pins.get_mut(&self.hash) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&self.hash);
            }
        }
    }
}

pub(crate) struct Store {
    dir: PathBuf,
    _lock: Arc<File>,
    catalog: Catalog,
    quota: u64,
    uncertain: bool,
    staging: Arc<Mutex<bool>>,
    pins: Arc<Mutex<BTreeMap<String, usize>>>,
    #[cfg(test)]
    fault: Option<&'static str>,
}

/// A bounded authority snapshot for preflight. This never creates directories,
/// acquires the writer lock, performs GC, or writes derived files.
pub(crate) struct ReadOnlyStore {
    dir: PathBuf,
    catalog: Catalog,
}

impl ReadOnlyStore {
    pub(crate) fn revision(&self) -> u64 {
        self.catalog.content_revision
    }

    pub(crate) fn records(&self) -> &[SourceRecord] {
        &self.catalog.records
    }

    pub(crate) fn record(&self, fingerprint: &str) -> Option<&SourceRecord> {
        self.catalog
            .records
            .iter()
            .find(|r| r.fingerprint == fingerprint)
    }

    pub(crate) fn verified_content(&self, fingerprint: &str) -> Result<(SourceRecord, File)> {
        let record = self
            .record(fingerprint)
            .context("subscription_material_missing")?;
        Ok((record.clone(), verified_object(&self.dir, record)?))
    }

    pub(crate) fn read_index(&self, input_digest: &str, max_bytes: u64) -> Result<Option<File>> {
        read_index(&self.dir, input_digest, max_bytes)
    }
}

impl Store {
    pub(crate) fn verified_record(&self, record: &SourceRecord) -> Result<File> {
        validate_record(record)?;
        verified_object(&self.dir, record)
    }
    pub(crate) fn read_only(path: &Path) -> Result<ReadOnlyStore> {
        check_private_dir(path)?;
        let catalog = read_catalog(
            checked_open(&path.join(CATALOG), false)?.context("subscription_material_missing")?,
        )?;
        // Do not inspect unrelated objects: the selected records are authority.
        check_private_dir(&path.join("objects"))?;
        Ok(ReadOnlyStore {
            dir: path.to_owned(),
            catalog,
        })
    }

    pub(crate) fn revision(&self) -> u64 {
        self.catalog.content_revision
    }

    pub(crate) fn records(&self) -> &[SourceRecord] {
        &self.catalog.records
    }

    pub(crate) fn record(&self, fingerprint: &str) -> Option<&SourceRecord> {
        self.catalog
            .records
            .iter()
            .find(|r| r.fingerprint == fingerprint)
    }

    pub(crate) fn verified_content(&self, fingerprint: &str) -> Result<(SourceRecord, File)> {
        let record = self
            .record(fingerprint)
            .context("subscription_material_missing")?;
        Ok((record.clone(), verified_object(&self.dir, record)?))
    }

    pub(crate) fn open(path: &Path, max_disk: u64, _now: u64) -> Result<Self> {
        ensure!(
            (MAX_CATALOG..=2 * 1024 * 1024 * 1024).contains(&max_disk),
            "invalid subscription disk budget"
        );
        private_dir(path)?;
        let dir = path.canonicalize()?;
        ensure!(
            dir.parent().is_some(),
            "dedicated subscription directory required"
        );
        let lock_path = dir.join("lock");
        let lock = match create_private(&lock_path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                checked_open(&lock_path, true)?.context("subscription lock disappeared")?
            }
            Err(error) => return Err(error.into()),
        };
        lock.try_lock()
            .context("subscription directory already in use")?;
        let catalog = if let Some(file) = checked_open(&dir.join(CATALOG), false)? {
            read_catalog(file)?
        } else {
            // Absence is safe only before any other feature state existed.
            ensure!(
                fs::read_dir(&dir)?
                    .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "lock")),
                "subscription catalog missing; manual recovery required"
            );
            Catalog::default()
        };
        let mut store = Self {
            dir,
            _lock: Arc::new(lock),
            catalog,
            quota: max_disk,
            uncertain: false,
            staging: Arc::new(Mutex::new(false)),
            pins: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(test)]
            fault: None,
        };
        if !store.dir.join(CATALOG).exists() {
            ensure!(
                store.commit_catalog(store.catalog.clone())? == CommitOutcome::Durable,
                "initial catalog durability uncertain; reopen required"
            );
        }
        for name in ["objects", "staging", "indexes"] {
            private_dir(&store.dir.join(name))?;
        }
        ensure!(
            store.disk_bytes()? <= 2 * 1024 * 1024 * 1024,
            "subscription storage exceeds hard disk limit; manual recovery required"
        );
        store.cleanup_staging()?;
        // Config is the authority for current/previous references. A crash may
        // follow its commit but precede reference maintenance. Never collect
        // here before the service has synchronized those authoritative roots.
        ensure!(
            store.disk_bytes()? <= store.quota,
            "subscription disk budget exceeded"
        );
        Ok(store)
    }

    pub(crate) fn begin_staging(&mut self, reserve_bytes: u64, now: u64) -> Result<Staging> {
        self.begin_reserved(reserve_bytes, MAX_OBJECT, "download", now)
    }

    // Admission holds the publication/UP-freeze gate and has synchronized the
    // Config roots. Only here may staging pressure evict selected preparations.
    pub(crate) fn collect_for_download(&mut self, now: u64, needs_download: bool) -> Result<()> {
        self.collect(now, false)?;
        if needs_download && !self.has_staging_space(MAX_OBJECT)? {
            self.collect(now, true)?;
        }
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        Ok(())
    }

    fn has_staging_space(&self, reserve_bytes: u64) -> Result<bool> {
        Ok(self
            .disk_bytes()?
            .checked_add(reserve_bytes + MAX_CATALOG)
            .is_some_and(|sum| sum <= self.quota))
    }

    fn begin_reserved(
        &mut self,
        reserve_bytes: u64,
        max: u64,
        kind: &str,
        _now: u64,
    ) -> Result<Staging> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            !*self.staging.lock().unwrap(),
            "subscription preparation already running"
        );
        ensure!(
            (1..=max).contains(&reserve_bytes),
            "invalid staging reservation"
        );
        // Download/compile can run outside the publication/UP-freeze gate.
        // Reclaim only unselected objects here, never mutate catalog authority.
        self.collect_orphans()?;
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            self.has_staging_space(reserve_bytes)?,
            "subscription disk budget exceeded"
        );
        let path = self
            .dir
            .join("staging")
            .join(format!("{kind}-{}.tmp", random_hex()));
        let file = create_private(&path)?;
        *self.staging.lock().unwrap() = true;
        Ok(Staging {
            file,
            lease: self._lock.clone(),
            path,
            reserved: reserve_bytes,
            active: self.staging.clone(),
            completed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn prepare(
        &mut self,
        stage: Staging,
        metadata: PreparedMetadata,
        now: u64,
    ) -> Result<CommitOutcome> {
        // Complete/release staging even when an active source is rejected.
        if let Err(error) = self.ensure_preparable(&metadata.fingerprint) {
            self.discard(stage)?;
            return Err(error);
        }
        let record = self.install_object(stage, metadata, now)?;
        self.commit_records(vec![record], self.revision(), now)
    }

    /// Install verified bytes without changing the selected version. The caller
    /// pins the result before any subsequent operation that can collect orphans.
    pub(crate) fn install_object(
        &mut self,
        mut stage: Staging,
        metadata: PreparedMetadata,
        now: u64,
    ) -> Result<SourceRecord> {
        ensure!(
            stage.path.parent() == Some(self.dir.join("staging").as_path())
                && Arc::ptr_eq(&stage.active, &self.staging),
            "foreign staging file"
        );
        // The coordinator must first drain StagingWriter::finish().await.
        // The same requirement applies to discard().
        stage.completed = true;
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        let metadata = SourceRecord {
            fingerprint: metadata.fingerprint,
            sha256: metadata.sha256,
            bytes: metadata.bytes,
            rules: metadata.rules,
            validators: metadata.validators,
            prepared_at: now,
            status: SourceStatus {
                last_attempt: Some(now),
                ..SourceStatus::default()
            },
        };
        validate_record(&metadata)?;
        ensure!(
            metadata.bytes <= stage.reserved,
            "staging reservation exceeded"
        );
        verify_file(&mut stage.file, &metadata)?;
        // Check the path again before publication; a substituted link is never followed.
        let mut path_file = checked_open(&stage.path, false)?.context("staging object missing")?;
        verify_file(&mut path_file, &metadata)?;
        stage.file.sync_all()?;
        let object = self.object_path(&metadata.sha256);
        if let Some(mut file) = checked_open(&object, false)? {
            if verify_file(&mut file, &metadata).is_err() {
                ensure!(
                    self.catalog
                        .records
                        .iter()
                        .any(|r| r.sha256 == metadata.sha256),
                    "unowned corrupt object; manual recovery required"
                );
                // Only an authority-selected, regular private corrupt object
                // can be repaired, with fully verified identical hash bytes.
                fs::rename(&stage.path, &object)?;
                sync_dir(&self.dir.join("objects"))?;
                sync_dir(&self.dir.join("staging"))?;
            }
        } else {
            fs::rename(&stage.path, &object)?;
            sync_dir(&self.dir.join("objects"))?;
            sync_dir(&self.dir.join("staging"))?;
        }
        drop(stage);
        Ok(metadata)
    }

    /// Replace accepted source selections atomically, after aggregate compilation
    /// and the service's config/local-basis/freeze checks have passed.
    #[cfg(test)]
    pub(crate) fn commit_records(
        &mut self,
        records: Vec<SourceRecord>,
        expected_revision: u64,
        _now: u64,
    ) -> Result<CommitOutcome> {
        self.commit_batch(records, &[], expected_revision)
    }

    #[cfg(test)]
    pub(crate) fn commit_batch(
        &mut self,
        records: Vec<SourceRecord>,
        statuses: &[(String, SourceStatus)],
        expected_revision: u64,
    ) -> Result<CommitOutcome> {
        self.commit_batch_with_check(records, statuses, expected_revision, || Ok(()))
    }

    pub(crate) fn commit_batch_with_check(
        &mut self,
        records: Vec<SourceRecord>,
        statuses: &[(String, SourceStatus)],
        expected_revision: u64,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<CommitOutcome> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            self.revision() == expected_revision,
            "subscription content revision conflict"
        );
        ensure!(records.len() <= 16, "too many candidate sources");
        let mut unique = BTreeSet::new();
        for record in &records {
            validate_record(record)?;
            ensure!(
                unique.insert(record.fingerprint.clone()),
                "duplicate candidate fingerprint"
            );
            verified_object(&self.dir, record)?;
        }
        let mut candidate = self.catalog.clone();
        candidate
            .records
            .retain(|r| !unique.contains(&r.fingerprint));
        candidate.records.extend(records);
        ensure!(statuses.len() <= 16, "too many candidate statuses");
        for (fingerprint, status) in statuses {
            if let Some(record) = candidate
                .records
                .iter_mut()
                .find(|r| &r.fingerprint == fingerprint)
            {
                record.status = status.clone();
            }
        }
        self.trim_prepared(&mut candidate, &unique)?;
        self.commit_selection_with_check(candidate, check)
    }

    /// Config owns these GC roots, including disabled fingerprints that may have
    /// no content yet. Updating roots does not itself create a content revision.
    pub(crate) fn set_references(
        &mut self,
        current: Vec<String>,
        previous: Vec<String>,
        _now: u64,
    ) -> Result<CommitOutcome> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        validate_references(&current)?;
        validate_references(&previous)?;
        let mut candidate = self.catalog.clone();
        candidate.current = current;
        candidate.previous = previous;
        self.trim_prepared(&mut candidate, &BTreeSet::new())?;
        self.commit_selection(candidate)
    }

    /// Attempt/backoff metadata can be persisted even when the selected object
    /// needs repair. It cannot alter its hash, validators, or success timestamp.
    pub(crate) fn update_status(
        &mut self,
        fingerprint: &str,
        status: SourceStatus,
    ) -> Result<CommitOutcome> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        let mut candidate = self.catalog.clone();
        candidate
            .records
            .iter_mut()
            .find(|r| r.fingerprint == fingerprint)
            .context("subscription_material_missing")?
            .status = status;
        self.commit_catalog(candidate)
    }

    fn trim_prepared(&self, candidate: &mut Catalog, retain: &BTreeSet<String>) -> Result<()> {
        let pins = self.pins.lock().unwrap();
        while unreferenced(candidate).count() > 16 {
            let evict = unreferenced(candidate)
                .filter(|r| !pins.contains_key(&r.sha256) && !retain.contains(&r.fingerprint))
                .min_by_key(|r| r.prepared_at)
                .map(|r| r.fingerprint.clone())
                .context("prepared slots are pinned")?;
            candidate.records.retain(|r| r.fingerprint != evict);
        }
        Ok(())
    }

    fn commit_selection(&mut self, candidate: Catalog) -> Result<CommitOutcome> {
        self.commit_selection_with_check(candidate, || Ok(()))
    }
    fn commit_selection_with_check(
        &mut self,
        mut candidate: Catalog,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<CommitOutcome> {
        let choices = |catalog: &Catalog| -> BTreeMap<String, String> {
            catalog
                .records
                .iter()
                .map(|r| (r.fingerprint.clone(), r.sha256.clone()))
                .collect()
        };
        if choices(&candidate) != choices(&self.catalog) {
            candidate.content_revision = candidate
                .content_revision
                .checked_add(1)
                .context("catalog revision overflow")?;
        }
        // No fallible maintenance after the rename commit point.
        self.commit_catalog_with_check(candidate, check)
    }

    #[cfg(test)]
    pub(crate) fn ensure_preparable(&self, fingerprint: &str) -> Result<()> {
        ensure!(valid_hash(fingerprint), "invalid source fingerprint");
        // Reference roots include disabled configured sources. Only Service's
        // effective settings can determine whether preparation would refresh.
        Ok(())
    }

    pub(crate) fn discard(&mut self, mut stage: Staging) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&stage.active, &self.staging),
            "foreign staging file"
        );
        stage.completed = true;
        drop(stage);
        Ok(())
    }

    pub(crate) fn read_prepared(
        &self,
        fingerprint: &str,
        now: u64,
    ) -> Result<Option<(SourceRecord, File)>> {
        let Some(record) = self
            .catalog
            .records
            .iter()
            .find(|r| r.fingerprint == fingerprint)
        else {
            return Ok(None);
        };
        if !referenced(&self.catalog, fingerprint)
            && now.saturating_sub(record.prepared_at) >= PREPARED_TTL
        {
            return Ok(None);
        }
        let file = verified_object(&self.dir, record)?;
        Ok(Some((record.clone(), file)))
    }

    pub(crate) fn pin(&self, sha256: &str) -> Result<WorkPin> {
        ensure!(valid_hash(sha256), "invalid object digest");
        let mut file = checked_open(&self.object_path(sha256), false)?
            .context("subscription_material_missing")?;
        ensure!(
            hash_file(&mut file)? == sha256,
            "source object digest mismatch"
        );
        self.pin_key(sha256.to_owned())
    }

    pub(crate) fn pin_index(&self, input_digest: &str) -> Result<WorkPin> {
        self.read_index(input_digest, MAX_INDEX)?
            .context("derived index missing")?;
        self.pin_key(format!("index:{input_digest}"))
    }

    fn pin_key(&self, key: String) -> Result<WorkPin> {
        let mut pins = self.pins.lock().unwrap();
        let count = pins.entry(key.clone()).or_default();
        *count = count.checked_add(1).context("pin count overflow")?;
        Ok(WorkPin {
            hash: key,
            pins: self.pins.clone(),
            _lease: self._lock.clone(),
        })
    }

    pub(crate) fn read_index(&self, input_digest: &str, max_bytes: u64) -> Result<Option<File>> {
        read_index(&self.dir, input_digest, max_bytes)
    }

    /// The policy codec owns the single versioned format. Storage only bounds
    /// its stream and recognizes the fixed ownership prefix for GC.
    pub(crate) fn write_index(
        &mut self,
        input_digest: &str,
        max_bytes: u64,
        now: u64,
        write: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<CommitOutcome> {
        ensure!(valid_hash(input_digest), "invalid index digest");
        let mut stage = self.begin_reserved(max_bytes, MAX_INDEX, "index", now)?;
        // This callback is synchronous; no blocking task can outlive the lease.
        stage.completed = true;
        let mut bounded = LimitedWriter {
            file: &mut stage.file,
            remaining: max_bytes,
        };
        write(&mut bounded)?;
        stage.file.flush()?;
        ensure!(
            owned_index(&mut stage.file, input_digest)?,
            "invalid derived index ownership"
        );
        stage.file.sync_all()?;
        let path = self.dir.join("indexes").join(format!("{input_digest}.bin"));
        // A corrupt derived file is replaceable, but links and external files
        // are not. No policy or source selection depends on this replacement.
        checked_open(&path, false)?;
        fs::rename(&stage.path, path)?;
        let sync =
            sync_dir(&self.dir.join("indexes")).and_then(|()| sync_dir(&self.dir.join("staging")));
        Ok(if sync.is_ok() {
            CommitOutcome::Durable
        } else {
            CommitOutcome::CommittedUncertain
        })
    }

    pub(crate) fn reconcile_sync(&mut self) -> Result<()> {
        // No replay: inspect the actual authority and synchronize its directory.
        let actual = read_catalog(
            checked_open(&self.dir.join(CATALOG), false)?.context("catalog missing")?,
        )?;
        sync_dir(&self.dir)?;
        self.catalog = actual;
        self.uncertain = false;
        Ok(())
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.dir.join("objects").join(format!("{hash}.txt"))
    }

    fn commit_catalog(&mut self, candidate: Catalog) -> Result<CommitOutcome> {
        self.commit_catalog_with_check(candidate, || Ok(()))
    }
    fn commit_catalog_with_check(
        &mut self,
        candidate: Catalog,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<CommitOutcome> {
        validate_catalog(&candidate)?;
        let bytes = serde_json::to_vec(&candidate)?;
        ensure!(bytes.len() as u64 <= MAX_CATALOG, "catalog exceeds 256 KiB");
        checked_open(&self.dir.join(CATALOG), false)?;
        ensure!(
            self.disk_bytes()?
                .checked_add(bytes.len() as u64)
                .is_some_and(|n| n <= self.quota),
            "subscription disk budget exceeded"
        );
        let temporary = self.dir.join(format!("catalog-{}.tmp", random_hex()));
        let result = (|| -> Result<CommitOutcome> {
            let mut file = create_private(&temporary)?;
            #[cfg(test)]
            ensure!(self.fault != Some("write"), "injected disk full");
            file.write_all(&bytes)?;
            file.sync_all()?;
            // Deadline/close is rechecked after the potentially blocking fsync,
            // immediately before the single authority commit point.
            check()?;
            #[cfg(test)]
            ensure!(self.fault != Some("rename"), "injected rename failure");
            fs::rename(&temporary, self.dir.join(CATALOG))?;
            self.catalog = candidate;
            let sync = sync_dir(&self.dir);
            #[cfg(test)]
            let sync = if self.fault == Some("directory_sync") {
                Err(anyhow::anyhow!("injected directory sync failure"))
            } else {
                sync
            };
            self.uncertain = sync.is_err();
            Ok(if self.uncertain {
                CommitOutcome::CommittedUncertain
            } else {
                CommitOutcome::Durable
            })
        })();
        let _ = fs::remove_file(temporary);
        result
    }

    pub(crate) fn collect(&mut self, now: u64, pressure: bool) -> Result<()> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            self.disk_bytes()? <= 2 * 1024 * 1024 * 1024,
            "subscription storage exceeds hard disk limit; manual recovery required"
        );
        let mut candidate = self.catalog.clone();
        let pins = self.pins.lock().unwrap().clone();
        candidate.records.retain(|r| {
            referenced(&self.catalog, &r.fingerprint)
                || pins.contains_key(&r.sha256)
                || (!pressure && now.saturating_sub(r.prepared_at) < PREPARED_TTL)
        });
        if candidate.records.len() != self.catalog.records.len() {
            candidate.content_revision = candidate
                .content_revision
                .checked_add(1)
                .context("catalog revision overflow")?;
            if self.commit_catalog(candidate)? == CommitOutcome::CommittedUncertain {
                return Ok(());
            }
        }
        self.collect_orphans()
    }

    fn collect_orphans(&self) -> Result<()> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        let pins = self.pins.lock().unwrap().clone();
        let keep: BTreeSet<_> = self
            .catalog
            .records
            .iter()
            .map(|r| r.sha256.as_str())
            .chain(pins.keys().map(String::as_str))
            .collect();
        for entry in fs::read_dir(self.dir.join("objects"))? {
            let path = entry?.path();
            let Some(hash) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".txt"))
                .filter(|s| valid_hash(s))
            else {
                continue;
            };
            if keep.contains(hash) {
                continue;
            }
            let Some(mut file) = checked_open(&path, false)? else {
                continue;
            };
            // Valid content-addressing establishes exact ownership for an orphan.
            if file.metadata()?.len() <= MAX_OBJECT && hash_file(&mut file)? == hash {
                fs::remove_file(path)?;
            }
        }
        sync_dir(&self.dir.join("objects"))?;
        // Disposable indexes retain only active work pins. Byte pressure alone
        // cannot bound many tiny generations before the directory entry limit.
        {
            for entry in fs::read_dir(self.dir.join("indexes"))? {
                let path = entry?.path();
                let Some(hash) = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_suffix(".bin"))
                    .filter(|s| valid_hash(s))
                else {
                    continue;
                };
                if pins.contains_key(&format!("index:{hash}")) {
                    continue;
                }
                let Some(mut file) = checked_open(&path, false)? else {
                    continue;
                };
                if owned_index(&mut file, hash)? {
                    fs::remove_file(path)?;
                }
            }
            sync_dir(&self.dir.join("indexes"))?;
        }
        Ok(())
    }

    fn cleanup_staging(&self) -> Result<()> {
        for (directory, prefix) in [
            (self.dir.join("staging"), "download-"),
            (self.dir.join("staging"), "index-"),
            (self.dir.clone(), "catalog-"),
        ] {
            for entry in fs::read_dir(directory)? {
                let path = entry?.path();
                let owned = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix(prefix))
                    .and_then(|s| s.strip_suffix(".tmp"))
                    .is_some_and(valid_hash);
                if owned && checked_open(&path, false)?.is_some() {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn set_quota(&mut self, max_disk: u64) -> Result<()> {
        ensure!(
            (MAX_CATALOG..=2 * 1024 * 1024 * 1024).contains(&max_disk),
            "invalid subscription disk budget"
        );
        ensure!(
            self.disk_bytes()? <= max_disk,
            "subscription disk budget exceeded"
        );
        self.quota = max_disk;
        Ok(())
    }

    pub(crate) fn disk_bytes(&self) -> Result<u64> {
        let mut bytes = 0_u64;
        let mut count = 0;
        for dir in [
            self.dir.clone(),
            self.dir.join("objects"),
            self.dir.join("staging"),
            self.dir.join("indexes"),
        ] {
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(&dir)? {
                count += 1;
                ensure!(count <= 4096, "too many subscription storage entries");
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if dir == self.dir
                    && ["objects", "staging", "indexes"]
                        .iter()
                        .any(|name| entry.file_name() == *name)
                {
                    ensure!(
                        metadata.is_dir(),
                        "subscription storage component is not a directory"
                    );
                    private_permissions(&metadata, 0o700)?;
                    continue;
                }
                ensure!(
                    metadata.is_file(),
                    "unexpected non-file in subscription directory"
                );
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("disk accounting overflow")?;
            }
        }
        Ok(bytes)
    }
}

fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let created = match builder.create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    check_private_dir(path)?;
    if created {
        sync_dir(
            path.parent()
                .context("subscription directory needs a parent")?,
        )?;
    }
    Ok(())
}
struct LimitedWriter<'a> {
    file: &'a mut File,
    remaining: u64,
}
impl Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("derived index reservation exceeded"));
        }
        let written = self.file.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn read_index(dir: &Path, input_digest: &str, max_bytes: u64) -> Result<Option<File>> {
    ensure!(valid_hash(input_digest), "invalid index digest");
    ensure!((1..=MAX_INDEX).contains(&max_bytes), "invalid index limit");
    check_private_dir(&dir.join("indexes"))?;
    let Some(mut file) = checked_open(
        &dir.join("indexes").join(format!("{input_digest}.bin")),
        false,
    )?
    else {
        return Ok(None);
    };
    ensure!(
        file.metadata()?.len() <= max_bytes,
        "derived index exceeds limit"
    );
    ensure!(
        owned_index(&mut file, input_digest)?,
        "invalid derived index ownership"
    );
    Ok(Some(file))
}
fn owned_index(file: &mut File, input_digest: &str) -> Result<bool> {
    if file.metadata()?.len() > MAX_INDEX {
        return Ok(false);
    }
    file.rewind()?;
    let mut prefix = [0u8; 48];
    let read = file.read_exact(&mut prefix);
    file.rewind()?;
    if let Err(error) = read {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(false);
        }
        return Err(error.into());
    }
    Ok(&prefix[..16] == crate::policy::radix::DERIVED_PREFIX
        && prefix[16..]
            .iter()
            .enumerate()
            .all(|(i, byte)| u8::from_str_radix(&input_digest[i * 2..i * 2 + 2], 16) == Ok(*byte)))
}
fn check_private_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir(),
        "subscription directory must not be a symlink"
    );
    private_permissions(&metadata, 0o700)?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("synchronize subscription directory")
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn random_hex() -> String {
    format!("{:x}", Sha256::digest(rand::random::<[u8; 32]>()))
}
fn hash_file(file: &mut File) -> Result<String> {
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    let mut total = 0_u64;
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        total += size as u64;
        ensure!(total <= MAX_OBJECT, "source object too large");
        hash.update(&buffer[..size]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(format!("{:x}", hash.finalize()))
}
fn verify_file(file: &mut File, record: &SourceRecord) -> Result<()> {
    ensure!(
        file.metadata()?.len() == record.bytes,
        "source object size mismatch"
    );
    ensure!(
        hash_file(file)? == record.sha256,
        "source object digest mismatch"
    );
    Ok(())
}
fn verified_object(dir: &Path, record: &SourceRecord) -> Result<File> {
    check_private_dir(&dir.join("objects"))?;
    let mut file = checked_open(
        &dir.join("objects").join(format!("{}.txt", record.sha256)),
        false,
    )?
    .context("subscription_material_missing")?;
    verify_file(&mut file, record)?;
    Ok(file)
}
fn referenced(catalog: &Catalog, fingerprint: &str) -> bool {
    catalog
        .current
        .iter()
        .chain(&catalog.previous)
        .any(|f| f == fingerprint)
}
fn unreferenced(catalog: &Catalog) -> impl Iterator<Item = &SourceRecord> {
    catalog
        .records
        .iter()
        .filter(|r| !referenced(catalog, &r.fingerprint))
}
fn validate_record(record: &SourceRecord) -> Result<()> {
    ensure!(
        valid_hash(&record.fingerprint) && valid_hash(&record.sha256),
        "invalid source fingerprint or digest"
    );
    ensure!(
        (1..=MAX_OBJECT).contains(&record.bytes) && (1..=5_000_000).contains(&record.rules),
        "invalid source size or rule count"
    );
    record.validators.validate()?;
    ensure!(
        record.validators.content_sha256 == record.sha256,
        "validator does not bind selected content"
    );
    ensure!(
        record
            .status
            .last_error
            .as_ref()
            .is_none_or(|error| error.len() <= 512),
        "subscription error exceeds 512 bytes"
    );
    ensure!(
        record_heap_bytes(record) <= 16 * 1024,
        "source metadata allocation limit"
    );
    Ok(())
}
fn validate_catalog(catalog: &Catalog) -> Result<()> {
    ensure!(
        catalog.schema == 1
            && catalog.records.len() <= 48
            && catalog.current.len() <= 16
            && catalog.previous.len() <= 16
            && unreferenced(catalog).count() <= 16,
        "invalid catalog schema or counts"
    );
    let mut unique = BTreeSet::new();
    for record in &catalog.records {
        validate_record(record)?;
        ensure!(
            unique.insert(&record.fingerprint),
            "duplicate catalog fingerprint"
        );
    }
    for group in [&catalog.current, &catalog.previous] {
        validate_references(group)?;
    }
    let metadata_bytes = std::mem::size_of::<Catalog>()
        + catalog.records.capacity() * std::mem::size_of::<SourceRecord>()
        + catalog.records.iter().map(record_heap_bytes).sum::<usize>()
        + [&catalog.current, &catalog.previous]
            .into_iter()
            .map(|group| {
                group.capacity() * std::mem::size_of::<String>()
                    + group.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>();
    ensure!(
        metadata_bytes <= 2 * MAX_CATALOG as usize,
        "catalog metadata allocation limit"
    );
    Ok(())
}
fn record_heap_bytes(record: &SourceRecord) -> usize {
    record.fingerprint.capacity()
        + record.sha256.capacity()
        + record.validators.final_url.capacity()
        + record.validators.representation.capacity()
        + record.validators.content_sha256.capacity()
        + record.validators.etag.as_ref().map_or(0, String::capacity)
        + record
            .validators
            .last_modified
            .as_ref()
            .map_or(0, String::capacity)
        + record
            .status
            .last_error
            .as_ref()
            .map_or(0, String::capacity)
}
fn validate_references(group: &[String]) -> Result<()> {
    ensure!(group.len() <= 16, "too many catalog references");
    let mut unique = BTreeSet::new();
    for fingerprint in group {
        ensure!(
            valid_hash(fingerprint) && unique.insert(fingerprint),
            "invalid catalog references"
        );
    }
    Ok(())
}
fn read_catalog(file: File) -> Result<Catalog> {
    ensure!(
        file.metadata()?.len() <= MAX_CATALOG,
        "catalog exceeds 256 KiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CATALOG + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_CATALOG, "catalog exceeds 256 KiB");
    let catalog =
        serde_json::from_slice(&bytes).context("invalid catalog; manual recovery required")?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
