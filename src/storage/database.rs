use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

pub(crate) const DATABASE_FORMAT: u32 = 2;

const SCHEMA: &str = "
CREATE TABLE metadata(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO metadata VALUES('log_epoch',0),('totals_epoch',0),('history_epoch',0),('clock_lower_ms',0),('log_count',0),('log_bytes',0),('point_count',0),('cleanup_age',0),('cleanup_entry_limit',0),('cleanup_byte_limit',0),('cleanup_database_limit',0),('cleanup_manual',0),('cleanup_last_reason',0),('cleanup_last_at_ms',0);
CREATE TABLE query_log(id INTEGER PRIMARY KEY AUTOINCREMENT, epoch INTEGER NOT NULL, time_ms INTEGER NOT NULL, retention_ms INTEGER NOT NULL, name TEXT NOT NULL, client TEXT NOT NULL, qtype TEXT NOT NULL, upstream TEXT, status TEXT NOT NULL, cache TEXT NOT NULL, charge INTEGER NOT NULL, payload BLOB NOT NULL);
CREATE INDEX query_log_time ON query_log(time_ms);
CREATE INDEX query_log_retention ON query_log(retention_ms);
CREATE INDEX query_log_cache ON query_log(cache,id);
CREATE INDEX query_log_status ON query_log(status,id);
CREATE TRIGGER log_insert AFTER INSERT ON query_log BEGIN UPDATE metadata SET value=value+1 WHERE key='log_count'; UPDATE metadata SET value=value+new.charge WHERE key='log_bytes'; END;
CREATE TRIGGER log_delete AFTER DELETE ON query_log BEGIN UPDATE metadata SET value=value-1 WHERE key='log_count'; UPDATE metadata SET value=value-old.charge WHERE key='log_bytes'; END;
CREATE TABLE statistics_totals(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL, since_ms INTEGER NOT NULL, run_id TEXT NOT NULL, seq INTEGER NOT NULL, payload BLOB NOT NULL, reset_clock_ms INTEGER NOT NULL);
CREATE TABLE statistics_points(history_epoch INTEGER NOT NULL, run_id TEXT NOT NULL, seq INTEGER NOT NULL, time_ms INTEGER NOT NULL, retention_ms INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(history_epoch,run_id,seq));
CREATE INDEX statistics_points_time ON statistics_points(time_ms);
CREATE INDEX statistics_points_retention ON statistics_points(retention_ms);
CREATE TRIGGER point_insert AFTER INSERT ON statistics_points BEGIN UPDATE metadata SET value=value+1 WHERE key='point_count'; END;
CREATE TRIGGER point_delete AFTER DELETE ON statistics_points BEGIN UPDATE metadata SET value=value-1 WHERE key='point_count'; END;
";

struct Database {
    conn: Connection,
    directory: Option<PathBuf>,
    settings: RuntimeSettings,
    revision: u64,
    cleanup_anchor: Instant,
    cleanup_wall: u64,
    last_cleanup: Instant,
    run_id: String,
    settings_pending: bool,
    age_pending: bool,
    reset_clock_ms: u64,
    /// Records queued before the asynchronous open learned the stored epoch
    /// belong to it until the first clear after open.
    opened_log_epoch: u64,
    coverage: Coverage,
}

/// SQLite VM operations between cancellation checks of the existing-file check.
const CHECK_PROGRESS_OPS: std::ffi::c_int = if cfg!(test) { 1 } else { 1000 };
#[cfg(test)]
pub(super) static CHECK_DELAYS: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());
#[cfg(test)]
fn test_check_delay(directory: Option<&Path>) -> Option<Duration> {
    let directory = directory?;
    CHECK_DELAYS
        .lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path == directory)
        .map(|(_, delay)| *delay)
}

pub(super) fn run(
    directory: Option<(PathBuf, File)>,
    shared: Arc<Shared>,
    logs: mpsc::Receiver<LogItem>,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<()>,
) {
    let (directory, _lock) = match directory {
        Some((dir, lock)) => (Some(dir), Some(lock)),
        None => (None, None),
    };
    // An existing database is opened and checked off the worker loop, so
    // startup never waits for it and controls are answered meanwhile.
    let mut checking = None;
    let mut database = match prepare(directory.as_deref()) {
        Ok(true) => opened(&shared, Database::open(directory.clone(), true, &shared)),
        Ok(false) => {
            shared.status.lock().expect("storage status").health = "checking".into();
            let (check_directory, check_shared) = (directory.clone(), shared.clone());
            match thread::Builder::new()
                .name("parins-storage-check".into())
                .spawn(move || Database::open(check_directory, false, &check_shared))
            {
                Ok(join) => checking = Some(join),
                Err(error) => {
                    opened(&shared, Err(Error::StorageUnavailable(error.to_string())));
                }
            }
            None
        }
        Err(error) => opened(&shared, Err(error)),
    };
    sample_filesystem(&shared, directory.as_deref());
    let _ = ready.send(());
    let mut last_flush = Instant::now();
    let mut last_status = Instant::now();
    let mut last_filesystem = Instant::now();
    loop {
        if let Some(join) = checking.take_if(|join| join.is_finished()) {
            database = finish_check(&shared, join);
        }
        // One bounded control operation between batches prevents log floods
        // from starving management, without allowing controls to starve writes.
        if let Ok(command) = commands.try_recv() {
            if let Command::Shutdown(reply) = command {
                shared.stop.store(true, Ordering::Release);
                // Stop cancels an unfinished check at its next progress callback.
                if let Some(join) = checking.take() {
                    database = finish_check(&shared, join);
                }
                // The terminal flush already cleaned up; closing commits only
                // what arrived since.
                let result = if let Some(db) = database.as_mut() {
                    let result = db
                        .drain(&shared, &logs, Instant::now() + Duration::from_secs(8))
                        .and_then(|()| db.checkpoint(true));
                    db.report_error(&shared, &result);
                    db.update_status(&shared);
                    result
                } else {
                    Err(Error::StorageUnavailable("database unavailable".into()))
                };
                drop(database);
                // close alone retains flock while a duplicated/inherited descriptor lives.
                let unlocked = _lock
                    .as_ref()
                    .map_or(Ok(()), File::unlock)
                    .map_err(|error| {
                        Error::StorageUnavailable(format!(
                            "release runtime directory lock: {error}"
                        ))
                    });
                drop(_lock);
                let _ = reply.send(result.and(unlocked));
                return;
            }
            if let Some(db) = database.as_mut() {
                db.apply_desired(&shared);
                db.command(command, &shared, &logs);
            } else {
                reject(command, &shared);
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if let Some(db) = database.as_mut() {
            db.apply_desired(&shared);
            if last_flush.elapsed() >= Duration::from_millis(db.settings.storage.flush_interval_ms)
                || shared.queue_entries.load(Ordering::Relaxed) >= 256
            {
                let result = db.flush_batch(&shared, &logs);
                db.report_error(&shared, &result);
                last_flush = Instant::now();
            }
            let cleanup_due = db.last_cleanup.elapsed()
                >= Duration::from_secs(db.settings.storage.cleanup_interval_secs);
            if cleanup_due
                || db.age_pending
                || shared
                    .status
                    .lock()
                    .expect("storage status")
                    .capacity_pending
            {
                let result = if cleanup_due || db.settings_pending || db.age_pending {
                    db.cleanup()
                } else {
                    db.trim_capacity()
                };
                db.report_error(&shared, &result);
                if cleanup_due {
                    db.last_cleanup = Instant::now();
                }
                let refreshed = db.refresh_log_state(&shared);
                db.report_error(&shared, &refreshed);
                // Publish the outcome at once: a stale capacity_pending would
                // repeat the cleanup on every pass until the next refresh.
                db.update_status(&shared);
                last_status = Instant::now();
            }
            let reset_days = db.settings.statistics.reset_interval_days;
            if reset_days > 0 {
                let owner = shared.stats.lock().expect("statistics owner");
                let epoch = owner.totals_epoch;
                let due =
                    db.cleanup_now().saturating_sub(db.reset_clock_ms) >= reset_days * 86_400_000;
                drop(owner);
                if due {
                    let result = db.reset_totals(&shared, epoch).map(|_| ());
                    db.report_error(&shared, &result);
                }
            }
            if last_status.elapsed() >= Duration::from_secs(1) {
                db.update_status(&shared);
                last_status = Instant::now();
            }
        } else if checking.is_none() {
            for _ in 0..256 {
                let Ok(item) = logs.try_recv() else {
                    break;
                };
                release(&shared, &item);
                shared.dropped.record(&shared.dropped.io_failure);
            }
        }
        if last_filesystem.elapsed() >= Duration::from_secs(60) {
            sample_filesystem(&shared, directory.as_deref());
            last_filesystem = Instant::now();
        }
        thread::park_timeout(Duration::from_millis(20));
    }
    if let Some(join) = checking {
        let _ = join.join();
    }
}

/// Publishes the outcome on the worker, once it owns the database: an earlier
/// "healthy" from the checking thread would admit commands it cannot yet serve.
fn opened(shared: &Shared, result: Result<Database>) -> Option<Database> {
    match result {
        Ok(database) => {
            let mut s = shared.status.lock().expect("storage status");
            if matches!(s.health.as_str(), "starting" | "checking") {
                s.health = "healthy".into();
            }
            Some(database)
        }
        Err(error) => {
            let mut s = shared.status.lock().expect("storage status");
            s.health = "unavailable".into();
            s.error = Some(error.to_string());
            None
        }
    }
}
fn finish_check(shared: &Shared, join: thread::JoinHandle<Result<Database>>) -> Option<Database> {
    let result = join.join().unwrap_or_else(|_| {
        Err(Error::StorageUnavailable(
            "runtime database check panicked".into(),
        ))
    });
    opened(shared, result)
}
/// Creates a missing database file; returns whether it is new and empty.
fn prepare(directory: Option<&Path>) -> Result<bool> {
    let Some(dir) = directory else {
        return Ok(true);
    };
    let path = dir.join("observability.sqlite3");
    match crate::private_files::create_private(&path) {
        Ok(file) => {
            file.sync_all()
                .map_err(|e| Error::StorageUnavailable(e.to_string()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(std::fs::metadata(&path)
            .map_err(|e| Error::StorageUnavailable(e.to_string()))?
            .len()
            == 0),
        Err(e) => Err(Error::StorageUnavailable(e.to_string())),
    }
}

fn sample_filesystem(shared: &Shared, directory: Option<&Path>) {
    let settings = shared
        .desired
        .lock()
        .expect("storage settings")
        .settings
        .clone();
    let observation = Filesystem::sample(directory, &settings);
    shared.status.lock().expect("storage status").filesystem = observation;
}

fn record_cleanup(conn: &Connection, reason: CleanupReason, removed: u64, at: u64) -> Result<()> {
    if removed == 0 {
        return Ok(());
    }
    conn.prepare_cached("UPDATE metadata SET value=value+?1 WHERE key=?2")?
        .execute(params![removed, reason.key()])?;
    conn.prepare_cached("UPDATE metadata SET value=? WHERE key='cleanup_last_reason'")?
        .execute([reason.code()])?;
    conn.prepare_cached("UPDATE metadata SET value=? WHERE key='cleanup_last_at_ms'")?
        .execute([at])?;
    Ok(())
}

fn reject(command: Command, shared: &Shared) {
    let error = {
        let status = shared.status.lock().expect("storage status");
        Error::StorageUnavailable(status.error.clone().unwrap_or_else(|| {
            if status.health == "checking" {
                "runtime database check in progress".into()
            } else {
                "database unavailable".into()
            }
        }))
    };
    match command {
        Command::List(_, r) => {
            let _ = r.send(Err(error));
        }
        Command::Newer(_, _, r) => {
            let _ = r.send(Err(error));
        }
        Command::ClearLogs(_, r) | Command::ClearHistory(_, r) | Command::ResetTotals(_, r) => {
            let _ = r.send(Err(error));
        }
        Command::Statistics(_, r) => {
            let _ = r.send(Err(error));
        }
        Command::Flush(r) | Command::Shutdown(r) => {
            let _ = r.send(Err(error));
        }
        #[cfg(test)]
        Command::Inspect(_, r) => {
            let _ = r.send(Err(error));
        }
    }
}
fn release(shared: &Shared, item: &LogItem) {
    shared.queue_entries.fetch_sub(1, Ordering::Relaxed);
    shared.queue_bytes.fetch_sub(item.charge, Ordering::Relaxed);
}
fn epoch(conn: &Connection, name: &str) -> Result<u64> {
    Ok(
        conn.query_row("SELECT value FROM metadata WHERE key=?", [name], |r| {
            r.get(0)
        })?,
    )
}
fn check_epoch(conn: &Connection, name: &str, expected: u64) -> Result<u64> {
    let current = epoch(conn, name)?;
    if current != expected {
        return Err(Error::Conflict {
            current_epoch: current,
        });
    }
    current
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| Error::StorageUnavailable("epoch exhausted".into()))
}

impl Database {
    fn open(directory: Option<PathBuf>, fresh: bool, shared: &Arc<Shared>) -> Result<Self> {
        let mut conn = match &directory {
            Some(dir) => Connection::open(dir.join("observability.sqlite3"))?,
            None => Connection::open_in_memory()?,
        };
        conn.busy_timeout(Duration::from_millis(250))?;
        use rusqlite::limits::Limit;
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 262_144)?;
        conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 16_384)?;
        conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 64)?;
        conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 64)?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        // A crash during the first commit can leave a non-empty file whose hot
        // journal rolls it back to no tables on open. Such a file holds no data,
        // so it is initialized like a new one instead of failing the format check.
        let fresh = fresh
            || conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })? == 0;
        if !fresh {
            // No deadline: a large file on a cold page cache may take long.
            // Only process shutdown interrupts the check.
            let cancel = shared.clone();
            #[cfg(test)]
            let hold = test_check_delay(directory.as_deref()).map(|d| Instant::now() + d);
            conn.progress_handler(
                CHECK_PROGRESS_OPS,
                Some(move || {
                    #[cfg(test)]
                    while hold.is_some_and(|until| Instant::now() < until)
                        && !cancel.stop.load(Ordering::Acquire)
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    cancel.stop.load(Ordering::Acquire)
                }),
            )?;
            let checked = (|| {
                if epoch(&conn, "format")? != u64::from(DATABASE_FORMAT) {
                    return Err(Error::StorageUnavailable(
                        "unsupported runtime database format; file retained".into(),
                    ));
                }
                let check: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
                if check != "ok" {
                    return Err(Error::StorageUnavailable(
                        "runtime database integrity check failed; file retained".into(),
                    ));
                }
                Ok(())
            })();
            if checked.is_err() && shared.stop.load(Ordering::Acquire) {
                return Err(Error::StorageUnavailable(
                    "runtime database check cancelled by shutdown".into(),
                ));
            }
            checked?;
            conn.progress_handler(0, None::<fn() -> bool>)?;
        }
        if fresh {
            // WAL initializes the database header, so set auto-vacuum first.
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
        if fresh {
            // auto_vacuum must precede the first table. Tables and the format
            // row commit together, so a crash never leaves an unformatted file.
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.prepare_cached("INSERT INTO metadata VALUES('format',?1)")?
                .execute([DATABASE_FORMAT])?;
            tx.commit()?;
        }
        // Read stored state first; publish it below under short locks.
        let lower = epoch(&conn, "clock_lower_ms")?;
        let log_epoch = epoch(&conn, "log_epoch")?;
        let totals_epoch = epoch(&conn, "totals_epoch")?;
        let history_epoch = epoch(&conn, "history_epoch")?;
        let stored = conn
            .query_row(
                "SELECT payload,reset_clock_ms FROM statistics_totals WHERE id=1",
                [],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?)),
            )
            .optional()?
            .map(|(bytes, reset_clock_ms)| {
                let totals: Totals = serde_json::from_slice(&bytes)?;
                validate_metrics(&totals.metrics, &shared.metrics.snapshot())?;
                if totals.epoch != totals_epoch {
                    return Err(Error::StorageUnavailable(
                        "statistics epoch mismatch".into(),
                    ));
                }
                Ok((totals, reset_clock_ms))
            })
            .transpose()?;
        let (settings, revision) = {
            let desired = shared.desired.lock().expect("storage settings");
            (desired.settings.clone(), desired.revision)
        };
        let mut database = Self {
            conn,
            directory,
            settings,
            revision,
            cleanup_anchor: Instant::now(),
            cleanup_wall: unix_ms().max(lower),
            last_cleanup: Instant::now(),
            run_id: String::new(),
            settings_pending: true,
            age_pending: false,
            reset_clock_ms: 0,
            opened_log_epoch: log_epoch,
            coverage: Coverage::default(),
        };
        let initial = {
            let mut owner = shared.stats.lock().expect("statistics owner");
            owner.totals_epoch = totals_epoch;
            owner.history_epoch = history_epoch;
            // Points sampled while checking belong to the stored epochs. A
            // checkpoint taken before the stored base was loaded must not persist.
            for point in &mut owner.points {
                point.history_epoch = history_epoch;
                point.totals_epoch = totals_epoch;
            }
            owner.latest = None;
            database.run_id = owner.run_id.clone();
            match stored {
                Some((totals, reset_clock_ms)) => {
                    database.reset_clock_ms = reset_clock_ms;
                    owner.base = totals.metrics;
                    owner.since_ms = totals.since_ms;
                    None
                }
                None => Some(owner.totals(&shared.metrics.snapshot())),
            }
        };
        if let Some(totals) = initial {
            database.reset_clock_ms = database.cleanup_now();
            database
                .conn
                .prepare_cached("INSERT INTO statistics_totals VALUES(1,?1,?2,?3,0,?4,?5)")?
                .execute(params![
                    totals.epoch,
                    totals.since_ms,
                    database.run_id,
                    serde_json::to_vec(&totals)?,
                    database.reset_clock_ms
                ])?;
        }
        {
            let mut status = shared.status.lock().expect("storage status");
            status.clock_rollback = unix_ms() < lower;
            status.log_epoch = log_epoch;
            status.totals_epoch = totals_epoch;
            status.history_epoch = history_epoch;
        }
        shared.log_epoch.store(log_epoch, Ordering::Release);
        database.refresh_log_state(shared)?;
        database.set_page_cap()?;
        let cleanup = database.cleanup();
        database.report_error(shared, &cleanup);
        database.refresh_log_state(shared)?;
        database.update_status(shared);
        Ok(database)
    }

    fn apply_desired(&mut self, shared: &Shared) {
        let desired = shared.desired.lock().expect("storage settings");
        if self.revision == desired.revision {
            return;
        }
        self.settings = desired.settings.clone();
        self.revision = desired.revision;
        self.settings_pending = true;
        drop(desired);
        let result = self.set_page_cap().and_then(|_| self.cleanup());
        self.report_error(shared, &result);
        let refreshed = self.refresh_log_state(shared);
        self.report_error(shared, &refreshed);
        self.update_status(shared);
    }
    fn cleanup_now(&mut self) -> u64 {
        let advancing = self.cleanup_wall.saturating_add(
            self.cleanup_anchor
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        let value = advancing.max(unix_ms());
        self.cleanup_wall = value;
        self.cleanup_anchor = Instant::now();
        value
    }
    fn page_info(&self) -> Result<(u64, u64, u64)> {
        Ok((
            self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?,
            self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?,
            self.conn
                .query_row("PRAGMA freelist_count", [], |r| r.get(0))?,
        ))
    }
    fn disk_bytes(&self) -> Result<u64> {
        if let Some(dir) = &self.directory {
            Ok([
                "observability.sqlite3",
                "observability.sqlite3-wal",
                "observability.sqlite3-shm",
            ]
            .iter()
            .map(|name| {
                std::fs::metadata(dir.join(name))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum())
        } else {
            let (size, pages, _) = self.page_info()?;
            Ok(size * pages)
        }
    }
    fn set_page_cap(&self) -> Result<()> {
        let (size, count, _) = self.page_info()?;
        // SQLite cannot reduce below current size. Freeze growth until bounded
        // cleanup reaches the desired cap; status retains capacity_pending.
        let cap = (self
            .settings
            .storage
            .max_database_bytes
            .saturating_sub(1_048_576)
            / size)
            .max(count);
        self.conn.pragma_update(None, "max_page_count", cap)?;
        Ok(())
    }
    fn checkpoint(&self, truncate: bool) -> Result<()> {
        if self.directory.is_some() {
            let mode = if truncate { "TRUNCATE" } else { "PASSIVE" };
            let (busy, _, _): (u64, u64, u64) =
                self.conn
                    .query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
            if truncate && busy != 0 {
                return Err(Error::StorageUnavailable("WAL checkpoint is busy".into()));
            }
        }
        Ok(())
    }
    fn refresh_log_state(&mut self, shared: &Shared) -> Result<()> {
        let first: Option<u64> = self
            .conn
            .query_row(
                "SELECT time_ms FROM query_log ORDER BY time_ms LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let last: Option<u64> = self
            .conn
            .query_row(
                "SELECT time_ms FROM query_log ORDER BY time_ms DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        self.coverage = Coverage {
            earliest_time_ms: first,
            latest_time_ms: last,
            span_ms: first.zip(last).map(|(a, b)| b.saturating_sub(a)),
        };
        let latest: u64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(id),0) FROM query_log", [], |r| {
                    r.get(0)
                })?;
        shared.latest_log_id.store(latest, Ordering::Release);
        Ok(())
    }
    /// Capacity trims on the write path never vacuum the database.
    fn trim_capacity(&mut self) -> Result<()> {
        let now = self.cleanup_now();
        let tx = self.conn.transaction()?;
        let count = epoch(&tx, "log_count")?;
        let bytes = epoch(&tx, "log_bytes")?;
        let (n, reason) = if count > self.settings.query_log.max_entries as u64 {
            (
                (count - self.settings.query_log.max_entries as u64).min(256),
                CleanupReason::EntryLimit,
            )
        } else if bytes > self.settings.query_log.max_bytes {
            (
                oldest_log_count(&tx, bytes - self.settings.query_log.max_bytes)? as u64,
                CleanupReason::ByteLimit,
            )
        } else {
            (0, CleanupReason::DatabaseLimit)
        };
        if n > 0 {
            let removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)")?.execute([n])?;
            record_cleanup(&tx, reason, removed as u64, now)?;
        }
        let points = epoch(&tx, "point_count")?;
        if points > self.settings.statistics.max_samples as u64 {
            let removed = tx.prepare_cached("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT ?)")?.execute([(points - self.settings.statistics.max_samples as u64).min(256)])?;
            record_cleanup(&tx, CleanupReason::EntryLimit, removed as u64, now)?;
        }
        tx.commit()?;
        let (size, pages, free) = self.page_info()?;
        let cap = self
            .settings
            .storage
            .max_database_bytes
            .saturating_sub(1_048_576)
            / size;
        if pages.saturating_sub(free) + 32 >= cap {
            let tx = self.conn.transaction()?;
            let mut removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT 256)")?.execute([])?;
            if removed == 0 {
                removed = tx.prepare_cached("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT 256)")?.execute([])?;
            }
            record_cleanup(&tx, CleanupReason::DatabaseLimit, removed as u64, now)?;
            tx.commit()?;
        }
        // Reclaim the WAL before treating the database itself as over capacity.
        self.checkpoint(true)?;
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        let now = self.cleanup_now();
        let cutoff = now.saturating_sub(self.settings.query_log.retention_secs * 1000);
        let point_cutoff = now.saturating_sub(self.settings.statistics.retention_secs * 1000);
        let tx = self.conn.transaction()?;
        let removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log WHERE retention_ms <= ? ORDER BY retention_ms LIMIT 256)")?.execute( [cutoff])?;
        record_cleanup(&tx, CleanupReason::Age, removed as u64, now)?;
        let (count, bytes) = (epoch(&tx, "log_count")?, epoch(&tx, "log_bytes")?);
        let overflow = count
            .saturating_sub(self.settings.query_log.max_entries as u64)
            .min(256);
        if overflow > 0 {
            let removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)")?.execute(
                [overflow],
            )?;
            record_cleanup(&tx, CleanupReason::EntryLimit, removed as u64, now)?;
        } else if bytes > self.settings.query_log.max_bytes {
            let count = oldest_log_count(&tx, bytes - self.settings.query_log.max_bytes)?;
            let removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)")?.execute(
                [count],
            )?;
            record_cleanup(&tx, CleanupReason::ByteLimit, removed as u64, now)?;
        }
        let removed = tx.prepare_cached("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points WHERE retention_ms <= ? ORDER BY retention_ms LIMIT 256)")?.execute( [point_cutoff])?;
        record_cleanup(&tx, CleanupReason::Age, removed as u64, now)?;
        let points = epoch(&tx, "point_count")?;
        if points > self.settings.statistics.max_samples as u64 {
            let removed = tx.prepare_cached("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT ?)")?.execute( [(points - self.settings.statistics.max_samples as u64).min(256)])?;
            record_cleanup(&tx, CleanupReason::EntryLimit, removed as u64, now)?;
        }
        tx.prepare_cached("UPDATE metadata SET value=? WHERE key='clock_lower_ms'")?
            .execute([now])?;
        let age_pending = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM query_log WHERE retention_ms <= ?1) OR EXISTS(SELECT 1 FROM statistics_points WHERE retention_ms <= ?2)",
            params![cutoff, point_cutoff],
            |row| row.get(0),
        )?;
        tx.commit()?;
        self.age_pending = age_pending;
        self.conn.execute_batch("PRAGMA incremental_vacuum(256)")?;
        let (size, pages, free) = self.page_info()?;
        let desired = self.settings.storage.max_database_bytes / size;
        if (pages > desired && free == 0)
            || (pages <= desired && pages.saturating_sub(free) + 32 >= desired && free < 32)
        {
            let tx = self.conn.transaction()?;
            let mut removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT 256)")?.execute( [])?;
            if removed == 0 {
                removed = tx.prepare_cached("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT 256)")?.execute( [])?;
            }
            record_cleanup(&tx, CleanupReason::DatabaseLimit, removed as u64, now)?;
            tx.commit()?;
        }
        self.conn.execute_batch("PRAGMA incremental_vacuum(256)")?;
        self.checkpoint(true)?;
        self.set_page_cap()?;
        Ok(())
    }
    fn flush_batch(&mut self, shared: &Shared, logs: &mpsc::Receiver<LogItem>) -> Result<()> {
        let mut batch = Vec::with_capacity(256);
        for _ in 0..256 {
            let Ok(item) = logs.try_recv() else {
                break;
            };
            batch.push(item);
        }
        let (latest, points) = {
            let mut owner = shared.stats.lock().expect("statistics owner");
            (owner.latest.take(), std::mem::take(&mut owner.points))
        };
        if batch.is_empty() && latest.is_none() && points.is_empty() {
            return Ok(());
        }
        let result = self.write_batch(shared, &batch, latest.as_ref(), &points);
        for item in &batch {
            release(shared, item);
        }
        if result.is_err() {
            shared
                .dropped
                .record_many(&shared.dropped.io_failure, batch.len() as u64);
            let mut owner = shared.stats.lock().expect("statistics owner");
            if owner.latest.is_none() {
                owner.latest = latest;
            }
            for point in points.into_iter().rev() {
                if owner.points.len() < 120 {
                    owner.points.push_front(point);
                } else {
                    owner.dropped_points = owner.dropped_points.saturating_add(1);
                }
            }
        } else {
            self.refresh_log_state(shared)?;
            self.committed(shared);
        }
        // Age expiry waits for the cleanup interval; only a write that leaves
        // a bound exceeded, or the file near its cap, cleans up right away.
        result.and_then(|_| {
            if self.capacity_pressure()? {
                self.trim_capacity()?;
                self.refresh_log_state(shared)
            } else {
                Ok(())
            }
        })
    }
    fn capacity_pressure(&self) -> Result<bool> {
        let limits = &self.settings;
        if epoch(&self.conn, "log_count")? > limits.query_log.max_entries as u64
            || epoch(&self.conn, "log_bytes")? > limits.query_log.max_bytes
            || epoch(&self.conn, "point_count")? > limits.statistics.max_samples as u64
        {
            return Ok(true);
        }
        let (size, pages, free) = self.page_info()?;
        Ok(pages.saturating_sub(free) + 32
            >= limits.storage.max_database_bytes.saturating_sub(1_048_576) / size
            || self.disk_bytes()? > limits.storage.max_database_bytes)
    }
    fn write_batch(
        &mut self,
        shared: &Shared,
        batch: &[LogItem],
        latest: Option<&statistics::Checkpoint>,
        points: &std::collections::VecDeque<Point>,
    ) -> Result<()> {
        let retention_now = self.cleanup_now();
        let tx = self.conn.transaction()?;
        let log_epoch = epoch(&tx, "log_epoch")?;
        let history_epoch = epoch(&tx, "history_epoch")?;
        let mut inserted = 0u64;
        {
            let mut insert = tx.prepare_cached("INSERT INTO query_log(epoch,time_ms,retention_ms,name,client,qtype,upstream,status,cache,charge,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
            for item in batch {
                if item.epoch != log_epoch
                    && !(item.epoch == PENDING_LOG_EPOCH && log_epoch == self.opened_log_epoch)
                {
                    continue;
                }
                let e = &item.entry;
                let payload = serde_json::to_vec(e)?;
                if payload.len() > 131072 {
                    shared.dropped.record(&shared.dropped.entry_too_large);
                    continue;
                }
                let retention_ms = retention_now
                    .saturating_sub(
                        item.enqueued_at.elapsed().as_millis().min(u64::MAX as u128) as u64
                    )
                    .saturating_sub(e.duration_ms.max(0.0) as u64);
                insert.execute(params![
                    log_epoch,
                    e.time_ms,
                    retention_ms,
                    e.name,
                    e.client,
                    e.qtype,
                    e.upstream,
                    e.status,
                    e.cache,
                    payload.len(),
                    payload
                ])?;
                inserted += 1;
            }
        }
        let overflow = epoch(&tx, "log_count")?
            .saturating_sub(self.settings.query_log.max_entries as u64)
            .min(inserted);
        if overflow > 0 {
            let removed = tx.prepare_cached("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)")?.execute([overflow])?;
            record_cleanup(
                &tx,
                CleanupReason::EntryLimit,
                removed as u64,
                retention_now,
            )?;
        }
        if let Some(cp) = latest
            && cp.run_id == self.run_id
        {
            // Adopt this exclusive process's run only as part of its first
            // committed checkpoint. Merely opening history need not write it.
            tx.prepare_cached("UPDATE statistics_totals SET run_id=?1,seq=0 WHERE id=1 AND epoch=?2 AND run_id<>?1")?.execute(params![cp.run_id,cp.totals.epoch])?;
            tx.prepare_cached("UPDATE statistics_totals SET since_ms=?1,seq=?2,payload=?3 WHERE id=1 AND epoch=?4 AND run_id=?5 AND seq<?2")?.execute( params![cp.totals.since_ms,cp.seq,serde_json::to_vec(&cp.totals)?,cp.totals.epoch,cp.run_id])?;
        }
        for point in points {
            if point.history_epoch != history_epoch {
                continue;
            }
            let retention_ms =
                retention_now.saturating_sub(
                    point.sampled_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
            tx.prepare_cached("INSERT OR IGNORE INTO statistics_points VALUES(?1,?2,?3,?4,?5,?6)")?
                .execute(params![
                    point.history_epoch,
                    point.run_id,
                    point.seq,
                    point.timestamp_ms,
                    retention_ms,
                    serde_json::to_vec(point)?
                ])?;
        }
        tx.commit()?;
        shared.log_epoch.store(log_epoch, Ordering::Release);
        Ok(())
    }

    fn command(&mut self, command: Command, shared: &Shared, logs: &mpsc::Receiver<LogItem>) {
        match command {
            #[cfg(test)]
            Command::Inspect(action, reply) => {
                let _ = reply.send(action(&mut self.conn));
            }
            Command::List(options, reply) => {
                let result = self.list(shared, options);
                self.report_error(shared, &result);
                let _ = reply.send(result);
            }
            Command::Newer(after_id, filter, reply) => {
                let result = self.newer(after_id, filter);
                self.report_error(shared, &result);
                let _ = reply.send(result);
            }
            Command::ClearLogs(epoch, reply) => {
                let result = self.clear_logs(shared, epoch);
                self.report_error(shared, &result);
                self.update_status(shared);
                let _ = reply.send(result);
            }
            Command::Statistics(options, reply) => {
                let result = self.statistics(shared, options);
                self.report_error(shared, &result);
                let _ = reply.send(result);
            }
            Command::ResetTotals(epoch, reply) => {
                let result = self.reset_totals(shared, epoch);
                self.report_error(shared, &result);
                self.update_status(shared);
                let _ = reply.send(result);
            }
            Command::ClearHistory(epoch, reply) => {
                let result = self.clear_history(shared, epoch);
                self.report_error(shared, &result);
                self.update_status(shared);
                let _ = reply.send(result);
            }
            Command::Flush(reply) => {
                let deadline = Instant::now() + Duration::from_secs(8);
                let result = self.drain(shared, logs, deadline).and_then(|()| {
                    loop {
                        self.cleanup()?;
                        if !self.age_pending {
                            break;
                        }
                        if Instant::now() >= deadline {
                            return Err(Error::StorageUnavailable(
                                "flush deadline exceeded; expired records remain".into(),
                            ));
                        }
                    }
                    self.last_cleanup = Instant::now();
                    Ok(())
                });
                self.report_error(shared, &result);
                let refreshed = self.refresh_log_state(shared);
                self.report_error(shared, &refreshed);
                self.update_status(shared);
                let _ = reply.send(result);
            }
            Command::Shutdown(_) => unreachable!("run handles shutdown"),
        }
    }
    /// Commits every queued record and a final statistics checkpoint. An
    /// explicit flush then cleans up, leaving the stored data within its
    /// bounds, including age retention.
    fn drain(
        &mut self,
        shared: &Shared,
        logs: &mpsc::Receiver<LogItem>,
        deadline: Instant,
    ) -> Result<()> {
        shared
            .stats
            .lock()
            .expect("statistics owner")
            .sample(&shared.metrics, true);
        loop {
            self.flush_batch(shared, logs)?;
            if shared.queue_entries.load(Ordering::Acquire) == 0 {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::StorageUnavailable(
                    "flush deadline exceeded; pending records remain".into(),
                ));
            }
        }
        Ok(())
    }
    /// A slow scan yields to writes and uses the existing busy response.
    fn bounded<T>(&self, budget: Duration, read: impl FnOnce() -> Result<T>) -> Result<T> {
        let deadline = Instant::now() + budget;
        self.conn
            .progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let result = read();
        self.conn.progress_handler(0, None::<fn() -> bool>)?;
        result
    }
    /// Newest first; returns at most `limit` entries plus one to detect more.
    fn select(
        &self,
        before_id: Option<u64>,
        after_id: Option<u64>,
        filter: ListOptions,
        limit: usize,
    ) -> Result<Vec<Entry>> {
        let mut clauses = Vec::new();
        let mut values = Vec::<rusqlite::types::Value>::new();
        if let Some(before) = before_id {
            values.push((before as i64).into());
            clauses.push(format!("id < ?{}", values.len()));
        }
        if let Some(after) = after_id {
            values.push((after as i64).into());
            clauses.push(format!("id > ?{}", values.len()));
        }
        if let Some(status) = filter.status {
            values.push(status.into());
            clauses.push(format!("status = ?{}", values.len()));
        }
        let cached = filter.cache.as_deref() == Some("cached");
        if let Some(cache) = filter.cache {
            match cache.as_str() {
                "cached" => {}
                "non_cached" => {
                    clauses.push("cache NOT IN ('fresh','stale','servfail')".to_owned())
                }
                _ => {
                    values.push(cache.into());
                    clauses.push(format!("cache = ?{}", values.len()));
                }
            }
        }
        if let Some(search) = filter.search.filter(|s| !s.is_empty()) {
            // Reuse one numbered parameter for the four searchable columns.
            let p = values.len() + 1;
            clauses.push(format!("(instr(lower(name),?{p})>0 OR instr(client,?{p})>0 OR instr(lower(coalesce(upstream,'')),?{p})>0 OR instr(lower(qtype),?{p})>0)"));
            values.push(search.to_ascii_lowercase().into());
        }
        let select = |extra: Option<&str>| {
            let mut terms = clauses.clone();
            if let Some(extra) = extra {
                terms.push(extra.to_owned());
            }
            let predicate = if terms.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", terms.join(" AND "))
            };
            format!(
                "SELECT id,payload FROM query_log{predicate} ORDER BY id DESC LIMIT {}",
                limit + 1
            )
        };
        // Bound each cache-key lookup before merging. A single IN query sorts
        // every cached row because its index starts with cache, not id.
        let sql = if cached {
            let branches = ["cache='fresh'", "cache='stale'", "cache='servfail'"]
                .map(|term| format!("SELECT id,payload FROM ({})", select(Some(term))));
            format!(
                "SELECT id,payload FROM ({}) ORDER BY id DESC LIMIT {}",
                branches.join(" UNION ALL "),
                limit + 1
            )
        } else {
            select(None)
        };
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values))?;
        let mut entries = Vec::with_capacity(limit + 1);
        loop {
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(rusqlite::Error::SqliteFailure(code, _))
                    if code.code == rusqlite::ErrorCode::OperationInterrupted =>
                {
                    return Err(Error::Busy);
                }
                Err(error) => return Err(error.into()),
            };
            let id = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            let mut entry: Entry = serde_json::from_slice(&bytes)?;
            entry.id = id;
            entries.push(entry);
        }
        Ok(entries)
    }
    fn list(&self, shared: &Shared, options: ListOptions) -> Result<Page> {
        self.bounded(Duration::from_millis(500), || {
            let total = epoch(&self.conn, "log_count")? as usize;
            let log_epoch = epoch(&self.conn, "log_epoch")?;
            let limit = options.limit.unwrap_or(50);
            let before_id = options.before_id;
            let mut entries = self.select(before_id, None, options, limit)?;
            let next_cursor = if entries.len() > limit {
                entries.truncate(limit);
                entries.last().map(|e| e.id)
            } else {
                None
            };
            Ok(Page {
                enabled: shared.enabled.load(Ordering::Acquire),
                total,
                entries,
                next_cursor,
                log_epoch,
                storage: status(shared),
            })
        })
    }
    fn newer(&self, after_id: Option<u64>, filter: ListOptions) -> Result<Newer> {
        self.bounded(Duration::from_millis(500), || {
            let log_epoch = epoch(&self.conn, "log_epoch")?;
            let scanned_to: u64 =
                self.conn
                    .query_row("SELECT COALESCE(MAX(id),0) FROM query_log", [], |r| {
                        r.get::<_, i64>(0)
                    })? as u64;
            // Without a cursor the page already holds history: start from now.
            let Some(after_id) = after_id else {
                return Ok(Newer {
                    entries: Vec::new(),
                    more: false,
                    log_epoch,
                    scanned_to,
                });
            };
            let mut entries = if scanned_to <= after_id {
                Vec::new()
            } else {
                self.select(None, Some(after_id), filter, NEWER_LIMIT)?
            };
            let more = entries.len() > NEWER_LIMIT;
            entries.truncate(NEWER_LIMIT);
            Ok(Newer {
                entries,
                more,
                log_epoch,
                scanned_to,
            })
        })
    }
    fn clear_logs(&mut self, shared: &Shared, expected: u64) -> Result<ClearResult> {
        let tx = self.conn.transaction()?;
        let next = check_epoch(&tx, "log_epoch", expected)?;
        let removed = tx.prepare_cached("DELETE FROM query_log")?.execute([])? as u64;
        record_cleanup(&tx, CleanupReason::Manual, removed, unix_ms())?;
        tx.prepare_cached("UPDATE metadata SET value=? WHERE key='log_epoch'")?
            .execute([next])?;
        tx.commit()?;
        shared.log_epoch.store(next, Ordering::Release);
        self.refresh_log_state(shared)?;
        let mut status = shared.status.lock().expect("storage status");
        status.log_epoch = next;
        status.last_commit_at_ms = Some(unix_ms());
        Ok(ClearResult {
            epoch: next,
            removed,
        })
    }
    fn reset_totals(&mut self, shared: &Shared, expected: u64) -> Result<ClearResult> {
        // The worker serializes resets; capture a cut under the sampler lock,
        // but never hold that lock over IO. During commit the sampler may queue
        // old-epoch checkpoints, which reset_committed discards after success.
        let (cut, since, run_id) = {
            let owner = shared.stats.lock().expect("statistics owner");
            (shared.metrics.snapshot(), unix_ms(), owner.run_id.clone())
        };
        // The reset schedule and retention floor share one logical clock.
        // Displayed since_ms remains wall time, which may have moved backwards.
        let reset_clock_ms = self.cleanup_now();
        let tx = self.conn.transaction()?;
        let next = check_epoch(&tx, "totals_epoch", expected)?;
        let totals = Totals {
            scope: "persistent".into(),
            epoch: next,
            since_ms: since,
            metrics: Metrics::default().snapshot(),
        };
        tx.prepare_cached("UPDATE metadata SET value=? WHERE key='totals_epoch'")?
            .execute([next])?;
        tx.prepare_cached("UPDATE statistics_totals SET epoch=?1,since_ms=?2,run_id=?3,seq=0,payload=?4,reset_clock_ms=?5 WHERE id=1")?.execute( params![next,since,run_id,serde_json::to_vec(&totals)?,reset_clock_ms])?;
        tx.prepare_cached("UPDATE metadata SET value=? WHERE key='clock_lower_ms'")?
            .execute([reset_clock_ms])?;
        tx.commit()?;
        self.reset_clock_ms = reset_clock_ms;
        shared
            .stats
            .lock()
            .expect("statistics owner")
            .reset_committed(cut, since, next);
        let mut status = shared.status.lock().expect("storage status");
        status.totals_epoch = next;
        status.last_commit_at_ms = Some(unix_ms());
        Ok(ClearResult {
            epoch: next,
            removed: 0,
        })
    }
    fn clear_history(&mut self, shared: &Shared, expected: u64) -> Result<ClearResult> {
        let (cut, at, since) = {
            let _owner = shared.stats.lock().expect("statistics owner");
            (shared.metrics.snapshot(), Instant::now(), unix_ms())
        };
        let tx = self.conn.transaction()?;
        let next = check_epoch(&tx, "history_epoch", expected)?;
        let removed = tx
            .prepare_cached("DELETE FROM statistics_points")?
            .execute([])? as u64;
        record_cleanup(&tx, CleanupReason::Manual, removed, unix_ms())?;
        tx.prepare_cached("UPDATE metadata SET value=? WHERE key='history_epoch'")?
            .execute([next])?;
        tx.commit()?;
        shared
            .stats
            .lock()
            .expect("statistics owner")
            .history_cleared(cut, since, at, next);
        let mut status = shared.status.lock().expect("storage status");
        status.history_epoch = next;
        status.last_commit_at_ms = Some(unix_ms());
        Ok(ClearResult {
            epoch: next,
            removed,
        })
    }
    fn statistics(&self, shared: &Shared, options: HistoryOptions) -> Result<StatisticsView> {
        let now = unix_ms();
        let duration = match options.range.as_deref() {
            Some("1h") => 3_600_000,
            Some("7d") => 604_800_000,
            _ => 86_400_000,
        };
        let to = options.to_ms.unwrap_or(now).min(now);
        let from = options
            .from_ms
            .unwrap_or(to.saturating_sub(duration))
            .max(now.saturating_sub(self.settings.statistics.retention_secs * 1000));
        if to < from {
            return Err(Error::Invalid(
                "statistics range is outside configured retention".into(),
            ));
        }
        let width = (to.saturating_sub(from).max(1))
            .div_ceil(options.max_points.unwrap_or(1000) as u64)
            .max(1);
        let samples = self.bounded(Duration::from_secs(2), || -> Result<Vec<Point>> {
            let shape = shared.metrics.snapshot();
            let mut points: BTreeMap<u64, Point> = BTreeMap::new();
            let mut statement = self.conn.prepare_cached("SELECT payload FROM statistics_points WHERE time_ms>=?1 AND time_ms<=?2 ORDER BY time_ms")?;
            let mut rows = statement.query(params![from, to])?;
            while let Some(row) = rows.next()? {
                let bytes: Vec<u8> = row.get(0)?;
                let point: Point = serde_json::from_slice(&bytes)?;
                validate_metrics(&point.metrics, &shape)?;
                let bucket = (point.timestamp_ms.saturating_sub(from) / width)
                    .min(options.max_points.unwrap_or(1000) as u64 - 1);
                match points.entry(bucket) {
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().merge(point)
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(point);
                    }
                }
            }
            Ok(points.into_values().collect())
        });
        let process_metrics = shared.metrics.snapshot();
        let owner = shared.stats.lock().expect("statistics owner");
        let totals = owner.totals(&process_metrics);
        let history_epoch = owner.history_epoch;
        drop(owner);
        Ok(StatisticsView {
            totals,
            process_scope: "process".into(),
            process_metrics,
            history_epoch,
            interval_seconds: 60,
            retention_seconds: self.settings.statistics.retention_secs,
            samples: samples?,
            storage: status(shared),
        })
    }
    fn report_error<T>(&self, shared: &Shared, result: &Result<T>) {
        if let Err(Error::StorageUnavailable(error)) = result {
            let mut s = shared.status.lock().expect("storage status");
            s.health = "degraded".into();
            s.error = Some(error.clone());
        }
    }
    /// A write ends a degraded state, but pending cleanup stays visible.
    fn committed(&self, shared: &Shared) {
        let mut status = shared.status.lock().expect("storage status");
        status.last_commit_at_ms = Some(unix_ms());
        status.health = if status.capacity_pending {
            "cleaning"
        } else {
            "healthy"
        }
        .into();
        status.error = None;
    }
    /// Reads the database and files first, then publishes under one short lock.
    fn update_status(&mut self, shared: &Shared) {
        let pages = self.page_info().ok();
        let files = self.directory.as_ref().map(|dir| {
            (
                std::fs::metadata(dir.join("observability.sqlite3"))
                    .map(|m| m.len())
                    .ok(),
                [
                    "observability.sqlite3-journal",
                    "observability.sqlite3-wal",
                    "observability.sqlite3-shm",
                ]
                .iter()
                .map(|name| {
                    std::fs::metadata(dir.join(name))
                        .map(|m| m.len())
                        .unwrap_or(0)
                })
                .sum::<u64>(),
                std::fs::symlink_metadata(dir.join("dns-cache-clean.snapshot"))
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .unwrap_or(0),
            )
        });
        let log_count = epoch(&self.conn, "log_count").ok();
        let log_bytes = epoch(&self.conn, "log_bytes").ok();
        let point_count = epoch(&self.conn, "point_count").ok();
        let coverage = self.coverage.clone();
        let cleanup = self.cleanup_status().ok();
        let mut status = shared.status.lock().expect("storage status");
        if let Some((size, pages, _)) = pages {
            status.database_bytes = size * pages;
            status.capacity_pending = pages > self.settings.storage.max_database_bytes / size;
        }
        if let Some((database, journal, snapshot)) = files {
            status.database_bytes = database.unwrap_or(status.database_bytes);
            status.journal_bytes = journal;
            status.capacity_pending |= status.database_bytes.saturating_add(journal)
                > self.settings.storage.max_database_bytes;
            status.cache_snapshot_bytes = snapshot;
        }
        if let Some(count) = log_count {
            status.query_log_entries = count;
        }
        if let Some(bytes) = log_bytes {
            status.query_log_bytes = bytes;
        }
        if let Some(count) = point_count {
            status.history_samples = count;
        }
        status.query_log_coverage = coverage;
        if let Some(cleanup) = cleanup {
            status.cleanup = cleanup;
        }
        status.capacity_pending |= status.query_log_entries
            > self.settings.query_log.max_entries as u64
            || status.query_log_bytes > self.settings.query_log.max_bytes
            || status.history_samples > self.settings.statistics.max_samples as u64
            || self.age_pending;
        if !status.capacity_pending {
            status.applied_revision = self.revision;
            self.settings_pending = false;
            if status.health == "cleaning" {
                status.health = "healthy".into();
            }
        } else if status.health == "healthy" {
            status.health = "cleaning".into();
        }
    }
    fn cleanup_status(&self) -> Result<Cleanup> {
        let at = epoch(&self.conn, "cleanup_last_at_ms")?;
        Ok(Cleanup {
            last_reason: CleanupReason::from_code(epoch(&self.conn, "cleanup_last_reason")?),
            last_at_ms: (at > 0).then_some(at),
            removed: observation::Removed {
                age: epoch(&self.conn, "cleanup_age")?,
                entry_limit: epoch(&self.conn, "cleanup_entry_limit")?,
                byte_limit: epoch(&self.conn, "cleanup_byte_limit")?,
                database_limit: epoch(&self.conn, "cleanup_database_limit")?,
                manual: epoch(&self.conn, "cleanup_manual")?,
            },
        })
    }
}

fn oldest_log_count(conn: &Connection, excess_bytes: u64) -> Result<usize> {
    let mut statement = conn.prepare("SELECT charge FROM query_log ORDER BY id LIMIT 256")?;
    let mut rows = statement.query([])?;
    let mut removed_bytes = 0u64;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        removed_bytes = removed_bytes.saturating_add(row.get::<_, u64>(0)?);
        count += 1;
        if removed_bytes >= excess_bytes {
            break;
        }
    }
    Ok(count)
}

fn validate_metrics(
    snapshot: &crate::metrics::Snapshot,
    shape: &crate::metrics::Snapshot,
) -> Result<()> {
    if !snapshot.counters.keys().eq(shape.counters.keys())
        || !snapshot
            .request_latency
            .buckets
            .iter()
            .zip(&shape.request_latency.buckets)
            .all(|(a, b)| a.upper_bound_micros == b.upper_bound_micros)
        || !snapshot
            .upstream_latency
            .buckets
            .iter()
            .zip(&shape.upstream_latency.buckets)
            .all(|(a, b)| a.upper_bound_micros == b.upper_bound_micros)
    {
        return Err(Error::StorageUnavailable(
            "invalid fixed-cardinality statistics payload".into(),
        ));
    }
    Ok(())
}
