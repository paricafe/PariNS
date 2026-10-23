//! One-use clean-cache file lifecycle. All methods do synchronous filesystem IO
//! and belong on a blocking worker. The process runtime owns the directory lock;
//! the top-level terminal shutdown owner alone may call `save_terminal`.

use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result, ensure};

use crate::{
    cache::{Cache, persistence::SnapshotReport},
    config::CachePersistenceConfig,
    private_files::{checked_open, create_private},
};

pub const CLEAN_FILE: &str = "dns-cache-clean.snapshot";

pub struct CachePersistence {
    directory: PathBuf,
    consumed: AtomicBool,
}

/// An open but already unlinked file cannot become a future startup candidate.
/// Keeping only the descriptor avoids allocating the entire snapshot in memory.
pub struct ConsumedSnapshot {
    file: Option<File>,
    max_bytes: usize,
    report: SnapshotReport,
}

impl ConsumedSnapshot {
    pub fn report(&self) -> &SnapshotReport {
        &self.report
    }

    /// Admits records until `deadline`; see `Cache::restore_clean_snapshot`.
    pub fn restore_into(
        mut self,
        cache: &Cache,
        fingerprint: &str,
        wall: SystemTime,
        now: Instant,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> SnapshotReport {
        let Some(ref mut file) = self.file else {
            return self.report;
        };
        match cache.restore_clean_snapshot(
            file,
            fingerprint,
            wall,
            now,
            self.max_bytes,
            deadline,
            cancelled,
        ) {
            Ok(report) => report,
            Err(error) => SnapshotReport {
                bytes: self.report.bytes,
                reason: Some(format!("{error:#}")),
                ..SnapshotReport::default()
            },
        }
    }
}

impl CachePersistence {
    /// The caller has already established private permissions and exclusive
    /// runtime-directory ownership. This constructor performs no IO.
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: directory.into(),
            consumed: AtomicBool::new(false),
        }
    }

    /// Must finish successfully before any DNS listener starts accepting. Even a
    /// disabled or oversized snapshot is durably consumed; the restore reports
    /// any other defect.
    /// A failed unlink/directory fsync is a hard startup error, never a cold-start
    /// success with an old recoverable file remaining beside a running cache.
    pub fn consume_startup(&self, settings: &CachePersistenceConfig) -> Result<ConsumedSnapshot> {
        ensure!(
            !self.consumed.load(Ordering::Acquire),
            "clean snapshot startup already consumed"
        );
        let path = self.directory.join(CLEAN_FILE);
        let Some(file) = checked_open(&path, false).context("open clean cache snapshot")? else {
            sync_directory(&self.directory).context("confirm consumed cache runtime directory")?;
            self.consumed.store(true, Ordering::Release);
            return Ok(ConsumedSnapshot {
                file: None,
                max_bytes: settings.max_bytes,
                report: SnapshotReport::skipped("no_clean_snapshot"),
            });
        };
        let length = file.metadata()?.len();
        // The restore reads and verifies the complete file within the same budget.
        let rejected = if !settings.enabled {
            Some("persistence_disabled")
        } else if length > settings.max_bytes as u64 {
            Some("snapshot exceeds byte budget")
        } else {
            None
        };
        // This step is unconditional after identifying an owned regular file.
        fs::remove_file(&path).context("consume clean cache snapshot before DNS startup")?;
        sync_directory(&self.directory)
            .context("durably consume clean cache snapshot before DNS startup")?;
        self.consumed.store(true, Ordering::Release);
        Ok(ConsumedSnapshot {
            file: rejected.is_none().then_some(file),
            max_bytes: settings.max_bytes,
            report: SnapshotReport {
                bytes: length,
                reason: rejected.map(Into::into),
                ..SnapshotReport::default()
            },
        })
    }

    /// The caller must have stopped every foreground/flight/adapter/refresh
    /// writer, frozen management mutations, and selected the current generation.
    /// An aborted task is not proof of that condition. Never call on ordinary
    /// Manager.stop, apply, rollback, or periodic timers. Writing stops at
    /// `deadline`; the published snapshot is then complete but partial.
    #[allow(clippy::too_many_arguments)]
    pub fn save_terminal(
        &self,
        cache: &Cache,
        fingerprint: &str,
        settings: &CachePersistenceConfig,
        wall: SystemTime,
        now: Instant,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<SnapshotReport> {
        ensure!(
            self.consumed.load(Ordering::Acquire),
            "startup snapshot has not been durably consumed"
        );
        if !settings.enabled {
            return Ok(SnapshotReport::skipped("persistence_disabled"));
        }
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "cache snapshot cancelled"
        );
        let clean = self.directory.join(CLEAN_FILE);
        match fs::symlink_metadata(&clean) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("clean snapshot path already exists; refusing terminal save"),
        }
        let temporary = self.directory.join(format!(
            "dns-cache.tmp.{}.{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let file =
            create_private(&temporary).context("create clean cache snapshot temporary file")?;
        let prepared = (|| -> Result<SnapshotReport> {
            // One write per record would dominate the save time of a large cache.
            let mut writer = BufWriter::with_capacity(1 << 20, file);
            let report = cache.write_clean_snapshot(
                &mut writer,
                fingerprint,
                wall,
                now,
                settings.max_bytes,
                deadline,
                cancelled,
            )?;
            let file = writer
                .into_inner()
                .map_err(|error| error.into_error())
                .context("write clean cache snapshot")?;
            publish_clean(&file, &temporary, &clean, cancelled, || {
                sync_directory(&self.directory)
            })?;
            Ok(report)
        })();
        if prepared.is_err() {
            // Only our unique temporary file belongs to this attempt. Never
            // delete the published file or an unrelated stale temporary file.
            match fs::remove_file(&temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(prepared
                        .err()
                        .unwrap()
                        .context(format!("temporary cleanup failed: {error}")));
                }
            }
        }
        prepared
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("fsync cache runtime directory")
}

fn publish_clean(
    file: &File,
    temporary: &Path,
    clean: &Path,
    cancelled: &AtomicBool,
    sync_parent: impl FnOnce() -> Result<()>,
) -> Result<()> {
    file.sync_all().context("fsync clean cache snapshot")?;
    ensure!(
        !cancelled.load(Ordering::Acquire),
        "cache snapshot cancelled"
    );
    fs::rename(temporary, clean).context("publish clean cache snapshot")?;
    // The rename has succeeded; a directory-sync failure cannot be described as
    // a rollback, even though the power-loss durability is still uncertain.
    sync_parent()
        .context("clean cache snapshot published but directory sync failed; durability uncertain")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_failure_leaves_only_unrecoverable_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("dns-cache.tmp.test");
        let file = create_private(&temporary).unwrap();
        let clean = directory.path().join(CLEAN_FILE);
        fs::create_dir(&clean).unwrap();
        let result = publish_clean(&file, &temporary, &clean, &AtomicBool::new(false), || {
            panic!("must not sync after failed rename")
        });
        assert!(result.unwrap_err().to_string().contains("publish"));
        assert!(temporary.is_file());
        assert!(clean.is_dir());
    }

    #[test]
    fn directory_sync_failure_reports_published_uncertain_file() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("dns-cache.tmp.test");
        let file = create_private(&temporary).unwrap();
        let clean = directory.path().join(CLEAN_FILE);
        let result = publish_clean(&file, &temporary, &clean, &AtomicBool::new(false), || {
            anyhow::bail!("injected directory fsync failure")
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("published but directory sync failed")
        );
        assert!(clean.is_file());
        assert!(!temporary.exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_sync_failure_cannot_publish_clean_file() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("dns-cache.tmp.test");
        let clean = directory.path().join(CLEAN_FILE);
        let not_syncable = File::open("/dev/null").unwrap();
        let result = publish_clean(
            &not_syncable,
            &temporary,
            &clean,
            &AtomicBool::new(false),
            || panic!("must not sync after failed file fsync"),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fsync clean cache snapshot")
        );
        assert!(!clean.exists());
    }
}
