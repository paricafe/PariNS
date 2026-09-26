//! Low-privilege source objects and their sole selection authority.
//! This foundation exposes preparation only, never active policy publication.
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
const PREPARED_TTL: u64 = 24 * 60 * 60;
const CATALOG: &str = "catalog.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRecord {
    pub fingerprint: String,
    pub sha256: String,
    pub bytes: u64,
    pub rules: u64,
    pub validators: Validators,
    pub prepared_at: u64,
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

impl Store {
    pub(crate) fn open(path: &Path, max_disk: u64, now: u64) -> Result<Self> {
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
        store.collect(now, false)?;
        ensure!(
            store.disk_bytes()? <= store.quota,
            "subscription disk budget exceeded"
        );
        Ok(store)
    }

    pub(crate) fn begin_staging(&mut self, reserve_bytes: u64, now: u64) -> Result<Staging> {
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            !*self.staging.lock().unwrap(),
            "subscription preparation already running"
        );
        ensure!(
            (1..=MAX_OBJECT).contains(&reserve_bytes),
            "invalid staging reservation"
        );
        self.collect(now, false)?;
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        if self
            .disk_bytes()?
            .checked_add(reserve_bytes + MAX_CATALOG)
            .is_none_or(|sum| sum > self.quota)
        {
            self.collect(now, true)?;
        }
        ensure!(
            !self.uncertain,
            "catalog durability requires reconciliation"
        );
        ensure!(
            self.disk_bytes()?
                .checked_add(reserve_bytes + MAX_CATALOG)
                .is_some_and(|sum| sum <= self.quota),
            "subscription disk budget exceeded"
        );
        let path = self
            .dir
            .join("staging")
            .join(format!("download-{}.tmp", random_hex()));
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

    pub(crate) fn prepare(
        &mut self,
        mut stage: Staging,
        metadata: PreparedMetadata,
        now: u64,
    ) -> Result<CommitOutcome> {
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
        self.ensure_preparable(&metadata.fingerprint)?;
        let metadata = SourceRecord {
            fingerprint: metadata.fingerprint,
            sha256: metadata.sha256,
            bytes: metadata.bytes,
            rules: metadata.rules,
            validators: metadata.validators,
            prepared_at: now,
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
        let mut candidate = self.catalog.clone();
        candidate
            .records
            .retain(|record| record.fingerprint != metadata.fingerprint);
        if !referenced(&candidate, &metadata.fingerprint) && unreferenced(&candidate).count() >= 16
        {
            let evict = candidate
                .records
                .iter()
                .filter(|r| {
                    !referenced(&candidate, &r.fingerprint)
                        && !self.pins.lock().unwrap().contains_key(&r.sha256)
                })
                .min_by_key(|r| r.prepared_at)
                .map(|r| r.fingerprint.clone())
                .context("prepared slots are pinned")?;
            candidate.records.retain(|r| r.fingerprint != evict);
        }
        candidate.records.push(metadata);
        candidate.content_revision = candidate
            .content_revision
            .checked_add(1)
            .context("catalog revision overflow")?;
        let outcome = self.commit_catalog(candidate)?;
        drop(stage);
        // Maintenance belongs to the next bounded operation. A GC failure
        // after rename must never disguise this already-committed result.
        Ok(outcome)
    }

    pub(crate) fn ensure_preparable(&self, fingerprint: &str) -> Result<()> {
        ensure!(valid_hash(fingerprint), "invalid source fingerprint");
        ensure!(
            !self.catalog.current.iter().any(|f| f == fingerprint),
            "active source requires refresh publication transaction"
        );
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
        let mut file = checked_open(&self.object_path(&record.sha256), false)?
            .context("subscription material missing")?;
        verify_file(&mut file, record)?;
        Ok(Some((record.clone(), file)))
    }

    pub(crate) fn pin(&self, sha256: &str) -> Result<WorkPin> {
        ensure!(valid_hash(sha256), "invalid object digest");
        ensure!(
            self.catalog.records.iter().any(|r| r.sha256 == sha256),
            "cannot pin unselected object"
        );
        let mut pins = self.pins.lock().unwrap();
        let count = pins.entry(sha256.to_owned()).or_default();
        *count = count.checked_add(1).context("pin count overflow")?;
        Ok(WorkPin {
            hash: sha256.to_owned(),
            pins: self.pins.clone(),
            _lease: self._lock.clone(),
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

    fn collect(&mut self, now: u64, pressure: bool) -> Result<()> {
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
        Ok(())
    }

    fn cleanup_staging(&self) -> Result<()> {
        for (directory, prefix) in [
            (self.dir.join("staging"), "download-"),
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

    fn disk_bytes(&self) -> Result<u64> {
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
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir(),
        "subscription directory must not be a symlink"
    );
    private_permissions(&metadata, 0o700)?;
    if created {
        sync_dir(
            path.parent()
                .context("subscription directory needs a parent")?,
        )?;
    }
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
        let mut refs = BTreeSet::new();
        for fingerprint in group {
            ensure!(
                unique.contains(fingerprint) && refs.insert(fingerprint),
                "invalid catalog references"
            );
        }
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
