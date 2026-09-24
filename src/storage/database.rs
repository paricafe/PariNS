use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

const SCHEMA: &str = "
CREATE TABLE metadata(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO metadata VALUES('format',2),('log_epoch',0),('totals_epoch',0),('history_epoch',0),('clock_lower_ms',0),('log_count',0),('log_bytes',0),('point_count',0),('cleanup_age',0),('cleanup_entry_limit',0),('cleanup_byte_limit',0),('cleanup_database_limit',0),('cleanup_manual',0),('cleanup_last_reason',0),('cleanup_last_at_ms',0);
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
    reset_clock_ms: u64,
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
    let mut database = match Database::open(directory.clone(), &shared) {
        Ok(database) => Some(database),
        Err(error) => {
            let mut s = shared.status.lock().expect("storage status");
            s.health = "unavailable".into();
            s.error = Some(error.to_string());
            None
        }
    };
    sample_filesystem(&shared, directory.as_deref());
    let _ = ready.send(());
    let mut last_flush = Instant::now();
    let mut last_status = Instant::now();
    let mut last_filesystem = Instant::now();
    loop {
        // One bounded control operation between batches prevents log floods
        // from starving management, without allowing controls to starve writes.
        if let Ok(command) = commands.try_recv() {
            if let Command::Shutdown(reply) = command {
                shared.stop.store(true, Ordering::Release);
                let result = if let Some(db) = database.as_mut() {
                    shared
                        .stats
                        .lock()
                        .expect("statistics owner")
                        .sample(&shared.metrics, true);
                    let result = db.drain(&shared, &logs);
                    db.report(&shared, &result);
                    db.update_status(&shared);
                    result
                } else {
                    Err(Error::StorageUnavailable("database unavailable".into()))
                };
                drop(database);
                drop(_lock);
                let _ = reply.send(result);
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
                db.report(&shared, &result);
                last_flush = Instant::now();
            }
            if db.last_cleanup.elapsed()
                >= Duration::from_secs(db.settings.storage.cleanup_interval_secs)
                || shared
                    .status
                    .lock()
                    .expect("storage status")
                    .capacity_pending
            {
                let result = db.cleanup();
                db.report(&shared, &result);
                db.last_cleanup = Instant::now();
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
                    db.report(&shared, &result);
                }
            }
            if last_status.elapsed() >= Duration::from_secs(1) {
                db.update_status(&shared);
                last_status = Instant::now();
            }
        } else {
            for _ in 0..256 {
                let Ok(item) = logs.try_recv() else {
                    break;
                };
                release(&shared, &item);
                shared.dropped.io_failure.fetch_add(1, Ordering::Relaxed);
            }
        }
        if last_filesystem.elapsed() >= Duration::from_secs(60) {
            sample_filesystem(&shared, directory.as_deref());
            last_filesystem = Instant::now();
        }
        thread::park_timeout(Duration::from_millis(20));
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
    conn.execute(
        "UPDATE metadata SET value=value+?1 WHERE key=?2",
        params![removed, reason.key()],
    )?;
    conn.execute(
        "UPDATE metadata SET value=? WHERE key='cleanup_last_reason'",
        [reason.code()],
    )?;
    conn.execute(
        "UPDATE metadata SET value=? WHERE key='cleanup_last_at_ms'",
        [at],
    )?;
    Ok(())
}

fn reject(command: Command, shared: &Shared) {
    let error = Error::StorageUnavailable(
        shared
            .status
            .lock()
            .expect("storage status")
            .error
            .clone()
            .unwrap_or_else(|| "database unavailable".into()),
    );
    match command {
        Command::List(_, r) => {
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
    fn open(directory: Option<PathBuf>, shared: &Shared) -> Result<Self> {
        let (conn, directory, fresh) = if let Some(dir) = directory {
            let path = dir.join("observability.sqlite3");
            let fresh = match crate::manage::store::create_private(&path) {
                Ok(file) => {
                    file.sync_all()
                        .map_err(|e| Error::StorageUnavailable(e.to_string()))?;
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::fs::metadata(&path)
                        .map_err(|e| Error::StorageUnavailable(e.to_string()))?
                        .len()
                        == 0
                }
                Err(e) => return Err(Error::StorageUnavailable(e.to_string())),
            };
            (Connection::open(&path)?, Some(dir), fresh)
        } else {
            (Connection::open_in_memory()?, None, true)
        };
        conn.busy_timeout(Duration::from_millis(250))?;
        use rusqlite::limits::Limit;
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 262_144)?;
        conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 16_384)?;
        conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 64)?;
        conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 64)?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        if !fresh {
            let deadline = Instant::now() + Duration::from_secs(5);
            conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
            if epoch(&conn, "format")? != 2 {
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
            conn.progress_handler(0, None::<fn() -> bool>)?;
        }
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048;")?;
        if fresh {
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
            conn.execute_batch(SCHEMA)?;
        }
        let desired = shared.desired.lock().expect("storage settings");
        let mut database = Self {
            conn,
            directory,
            settings: desired.settings.clone(),
            revision: desired.revision,
            cleanup_anchor: Instant::now(),
            cleanup_wall: now_ms(),
            last_cleanup: Instant::now(),
            run_id: String::new(),
            settings_pending: true,
            reset_clock_ms: 0,
        };
        drop(desired);
        let lower = epoch(&database.conn, "clock_lower_ms")?;
        database.cleanup_wall = database.cleanup_wall.max(lower);
        let mut status = shared.status.lock().expect("storage status");
        status.clock_rollback = now_ms() < lower;
        status.log_epoch = epoch(&database.conn, "log_epoch")?;
        status.totals_epoch = epoch(&database.conn, "totals_epoch")?;
        status.history_epoch = epoch(&database.conn, "history_epoch")?;
        status.health = "healthy".into();
        shared.log_epoch.store(status.log_epoch, Ordering::Release);
        let mut owner = shared.stats.lock().expect("statistics owner");
        owner.totals_epoch = status.totals_epoch;
        owner.history_epoch = status.history_epoch;
        let stored: Option<(Vec<u8>, u64)> = database
            .conn
            .query_row(
                "SELECT payload,reset_clock_ms FROM statistics_totals WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((bytes, reset_clock_ms)) = stored {
            database.reset_clock_ms = reset_clock_ms;
            let totals: Totals = serde_json::from_slice(&bytes)?;
            validate_metrics(&totals.metrics, &shared.metrics.snapshot())?;
            if totals.epoch != owner.totals_epoch {
                return Err(Error::StorageUnavailable(
                    "statistics epoch mismatch".into(),
                ));
            }
            owner.base = totals.metrics;
            owner.since_ms = totals.since_ms;
        } else {
            let totals = owner.totals(&shared.metrics.snapshot());
            database.reset_clock_ms = database.cleanup_now();
            database.conn.execute(
                "INSERT INTO statistics_totals VALUES(1,?1,?2,?3,0,?4,?5)",
                params![
                    totals.epoch,
                    totals.since_ms,
                    owner.run_id,
                    serde_json::to_vec(&totals)?,
                    database.reset_clock_ms
                ],
            )?;
        }
        database.run_id = owner.run_id.clone();
        drop(owner);
        drop(status);
        database.set_page_cap()?;
        let cleanup = database.cleanup();
        database.report_error(shared, &cleanup);
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
        self.report(shared, &result);
        self.update_status(shared);
    }
    fn cleanup_now(&mut self) -> u64 {
        let advancing = self.cleanup_wall.saturating_add(
            self.cleanup_anchor
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        let value = advancing.max(now_ms());
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
    fn set_page_cap(&self) -> Result<()> {
        let (size, count, _) = self.page_info()?;
        // SQLite cannot reduce below current size. Freeze growth until bounded
        // cleanup reaches the desired cap; status retains capacity_pending.
        let cap = (self.settings.storage.max_database_bytes / size).max(count);
        self.conn.pragma_update(None, "max_page_count", cap)?;
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        let now = self.cleanup_now();
        let cutoff = now.saturating_sub(self.settings.query_log.retention_secs * 1000);
        let point_cutoff = now.saturating_sub(self.settings.statistics.retention_secs * 1000);
        let tx = self.conn.transaction()?;
        let removed = tx.execute("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log WHERE retention_ms <= ? ORDER BY retention_ms LIMIT 256)", [cutoff])?;
        record_cleanup(&tx, CleanupReason::Age, removed as u64, now)?;
        let (count, bytes) = (epoch(&tx, "log_count")?, epoch(&tx, "log_bytes")?);
        let overflow = count
            .saturating_sub(self.settings.query_log.max_entries as u64)
            .min(256);
        if overflow > 0 {
            let removed = tx.execute(
                "DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)",
                [overflow],
            )?;
            record_cleanup(&tx, CleanupReason::EntryLimit, removed as u64, now)?;
        } else if bytes > self.settings.query_log.max_bytes {
            let count = oldest_log_count(&tx, bytes - self.settings.query_log.max_bytes)?;
            let removed = tx.execute(
                "DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT ?)",
                [count],
            )?;
            record_cleanup(&tx, CleanupReason::ByteLimit, removed as u64, now)?;
        }
        let removed = tx.execute("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points WHERE retention_ms <= ? ORDER BY retention_ms LIMIT 256)", [point_cutoff])?;
        record_cleanup(&tx, CleanupReason::Age, removed as u64, now)?;
        let points = epoch(&tx, "point_count")?;
        if points > self.settings.statistics.max_samples as u64 {
            let removed = tx.execute("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT ?)", [(points - self.settings.statistics.max_samples as u64).min(256)])?;
            record_cleanup(&tx, CleanupReason::EntryLimit, removed as u64, now)?;
        }
        tx.execute(
            "UPDATE metadata SET value=? WHERE key='clock_lower_ms'",
            [now],
        )?;
        tx.commit()?;
        self.conn.execute_batch("PRAGMA incremental_vacuum(256)")?;
        let (size, pages, free) = self.page_info()?;
        let desired = self.settings.storage.max_database_bytes / size;
        if (pages > desired && free == 0)
            || (pages <= desired && pages.saturating_sub(free) + 32 >= desired && free < 32)
        {
            let tx = self.conn.transaction()?;
            let mut removed = tx.execute("DELETE FROM query_log WHERE id IN (SELECT id FROM query_log ORDER BY id LIMIT 256)", [])?;
            if removed == 0 {
                removed = tx.execute("DELETE FROM statistics_points WHERE rowid IN (SELECT rowid FROM statistics_points ORDER BY time_ms LIMIT 256)", [])?;
            }
            record_cleanup(&tx, CleanupReason::DatabaseLimit, removed as u64, now)?;
            tx.commit()?;
        }
        self.conn.execute_batch("PRAGMA incremental_vacuum(256)")?;
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
                .io_failure
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
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
            self.committed(shared);
        }
        result.and_then(|_| self.cleanup())
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
        for item in batch {
            if item.epoch != log_epoch {
                continue;
            }
            let e: Entry = serde_json::from_slice(&item.payload)?;
            let retention_ms = retention_now
                .saturating_sub(item.enqueued_at.elapsed().as_millis().min(u64::MAX as u128) as u64)
                .saturating_sub(e.duration_ms.max(0.0) as u64);
            tx.execute("INSERT INTO query_log(epoch,time_ms,retention_ms,name,client,qtype,upstream,status,cache,charge,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![item.epoch,e.time_ms,retention_ms,e.name,e.client,e.qtype,e.upstream,e.status,e.cache,item.payload.len(),item.payload])?;
        }
        if let Some(cp) = latest
            && cp.run_id == self.run_id
        {
            // Adopt this exclusive process's run only as part of its first
            // committed checkpoint. Merely opening history need not write it.
            tx.execute("UPDATE statistics_totals SET run_id=?1,seq=0 WHERE id=1 AND epoch=?2 AND run_id<>?1",params![cp.run_id,cp.totals.epoch])?;
            tx.execute("UPDATE statistics_totals SET since_ms=?1,seq=?2,payload=?3 WHERE id=1 AND epoch=?4 AND run_id=?5 AND seq<?2", params![cp.totals.since_ms,cp.seq,serde_json::to_vec(&cp.totals)?,cp.totals.epoch,cp.run_id])?;
        }
        for point in points {
            if point.history_epoch != history_epoch {
                continue;
            }
            let retention_ms =
                retention_now.saturating_sub(
                    point.sampled_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
            tx.execute(
                "INSERT OR IGNORE INTO statistics_points VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    point.history_epoch,
                    point.run_id,
                    point.seq,
                    point.timestamp_ms,
                    retention_ms,
                    serde_json::to_vec(point)?
                ],
            )?;
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
                shared
                    .stats
                    .lock()
                    .expect("statistics owner")
                    .sample(&shared.metrics, true);
                let result = self.drain(shared, logs);
                self.report(shared, &result);
                self.update_status(shared);
                let _ = reply.send(result);
            }
            Command::Shutdown(reply) => {
                shared.stop.store(true, Ordering::Release);
                shared
                    .stats
                    .lock()
                    .expect("statistics owner")
                    .sample(&shared.metrics, true);
                let result = self.drain(shared, logs);
                self.report(shared, &result);
                self.update_status(shared);
                let _ = reply.send(result);
            }
        }
    }
    fn drain(&mut self, shared: &Shared, logs: &mpsc::Receiver<LogItem>) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
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
    fn list(&self, shared: &Shared, options: ListOptions) -> Result<Page> {
        let deadline = Instant::now() + Duration::from_secs(2);
        self.conn
            .progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let result = (|| {
            let total = epoch(&self.conn, "log_count")? as usize;
            let log_epoch = epoch(&self.conn, "log_epoch")?;
            let limit = options.limit.unwrap_or(50);
            let search = options.search.unwrap_or_default().to_ascii_lowercase();
            let mut statement = self.conn.prepare("SELECT id,payload FROM query_log WHERE (?1 IS NULL OR id<?1) AND (?2 IS NULL OR status=?2) AND (?3 IS NULL OR (?3='cached' AND cache IN ('fresh','stale')) OR (?3='non_cached' AND cache NOT IN ('fresh','stale')) OR cache=?3) AND (?4='' OR instr(lower(name),?4)>0 OR instr(client,?4)>0 OR instr(lower(coalesce(upstream,'')),?4)>0 OR instr(lower(qtype),?4)>0) ORDER BY id DESC LIMIT ?5")?;
            let mut rows = statement.query(params![
                options.before_id,
                options.status,
                options.cache,
                search,
                limit + 1
            ])?;
            let mut entries = Vec::with_capacity(limit + 1);
            while let Some(row) = rows.next()? {
                let id = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                let mut entry: Entry = serde_json::from_slice(&bytes)?;
                entry.id = id;
                entries.push(entry);
            }
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
        })();
        self.conn.progress_handler(0, None::<fn() -> bool>)?;
        result
    }
    fn clear_logs(&mut self, shared: &Shared, expected: u64) -> Result<ClearResult> {
        let tx = self.conn.transaction()?;
        let next = check_epoch(&tx, "log_epoch", expected)?;
        let removed = tx.execute("DELETE FROM query_log", [])? as u64;
        record_cleanup(&tx, CleanupReason::Manual, removed, now_ms())?;
        tx.execute("UPDATE metadata SET value=? WHERE key='log_epoch'", [next])?;
        tx.commit()?;
        shared.log_epoch.store(next, Ordering::Release);
        let mut status = shared.status.lock().expect("storage status");
        status.log_epoch = next;
        status.last_commit_at_ms = Some(now_ms());
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
            (shared.metrics.snapshot(), now_ms(), owner.run_id.clone())
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
        tx.execute(
            "UPDATE metadata SET value=? WHERE key='totals_epoch'",
            [next],
        )?;
        tx.execute("UPDATE statistics_totals SET epoch=?1,since_ms=?2,run_id=?3,seq=0,payload=?4,reset_clock_ms=?5 WHERE id=1", params![next,since,run_id,serde_json::to_vec(&totals)?,reset_clock_ms])?;
        tx.execute(
            "UPDATE metadata SET value=? WHERE key='clock_lower_ms'",
            [reset_clock_ms],
        )?;
        tx.commit()?;
        self.reset_clock_ms = reset_clock_ms;
        shared
            .stats
            .lock()
            .expect("statistics owner")
            .reset_committed(cut, since, next);
        let mut status = shared.status.lock().expect("storage status");
        status.totals_epoch = next;
        status.last_commit_at_ms = Some(now_ms());
        Ok(ClearResult {
            epoch: next,
            removed: 0,
        })
    }
    fn clear_history(&mut self, shared: &Shared, expected: u64) -> Result<ClearResult> {
        let (cut, at, since) = {
            let _owner = shared.stats.lock().expect("statistics owner");
            (shared.metrics.snapshot(), Instant::now(), now_ms())
        };
        let tx = self.conn.transaction()?;
        let next = check_epoch(&tx, "history_epoch", expected)?;
        let removed = tx.execute("DELETE FROM statistics_points", [])? as u64;
        record_cleanup(&tx, CleanupReason::Manual, removed, now_ms())?;
        tx.execute(
            "UPDATE metadata SET value=? WHERE key='history_epoch'",
            [next],
        )?;
        tx.commit()?;
        shared
            .stats
            .lock()
            .expect("statistics owner")
            .history_cleared(cut, since, at, next);
        let mut status = shared.status.lock().expect("storage status");
        status.history_epoch = next;
        status.last_commit_at_ms = Some(now_ms());
        Ok(ClearResult {
            epoch: next,
            removed,
        })
    }
    fn statistics(&self, shared: &Shared, options: HistoryOptions) -> Result<StatisticsView> {
        let now = now_ms();
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
        let deadline = Instant::now() + Duration::from_secs(2);
        self.conn
            .progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        let samples = (|| -> Result<Vec<Point>> {
            let shape = shared.metrics.snapshot();
            let mut points: BTreeMap<u64, Point> = BTreeMap::new();
            let mut statement = self.conn.prepare("SELECT payload FROM statistics_points WHERE time_ms>=?1 AND time_ms<=?2 ORDER BY time_ms")?;
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
        })();
        self.conn.progress_handler(0, None::<fn() -> bool>)?;
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
    fn report(&self, shared: &Shared, result: &Result<()>) {
        self.report_error(shared, result);
    }
    fn committed(&self, shared: &Shared) {
        let mut status = shared.status.lock().expect("storage status");
        status.last_commit_at_ms = Some(now_ms());
        status.health = "healthy".into();
        status.error = None;
    }
    fn update_status(&mut self, shared: &Shared) {
        let mut status = shared.status.lock().expect("storage status");
        if let Ok((size, pages, _)) = self.page_info() {
            status.database_bytes = size * pages;
            status.capacity_pending = pages > self.settings.storage.max_database_bytes / size;
        }
        if let Some(dir) = &self.directory {
            status.database_bytes = std::fs::metadata(dir.join("observability.sqlite3"))
                .map(|m| m.len())
                .unwrap_or(status.database_bytes);
            status.journal_bytes = std::fs::metadata(dir.join("observability.sqlite3-journal"))
                .map(|m| m.len())
                .unwrap_or(0);
            status.cache_snapshot_bytes =
                std::fs::symlink_metadata(dir.join("dns-cache-clean.snapshot"))
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .unwrap_or(0);
        }
        if let Ok(count) = epoch(&self.conn, "log_count") {
            status.query_log_entries = count;
        }
        if let Ok(bytes) = epoch(&self.conn, "log_bytes") {
            status.query_log_bytes = bytes;
        }
        if let Ok(count) = epoch(&self.conn, "point_count") {
            status.history_samples = count;
        }
        let earliest = self
            .conn
            .query_row(
                "SELECT time_ms FROM query_log ORDER BY time_ms LIMIT 1",
                [],
                |r| r.get::<_, u64>(0),
            )
            .optional();
        let latest = self
            .conn
            .query_row(
                "SELECT time_ms FROM query_log ORDER BY time_ms DESC LIMIT 1",
                [],
                |r| r.get::<_, u64>(0),
            )
            .optional();
        if let (Ok(earliest), Ok(latest)) = (earliest, latest) {
            status.query_log_coverage = Coverage {
                earliest_time_ms: earliest,
                latest_time_ms: latest,
                span_ms: earliest.zip(latest).map(|(a, b)| b.saturating_sub(a)),
            };
        }
        if let Ok(cleanup) = self.cleanup_status() {
            status.cleanup = cleanup;
        }
        status.capacity_pending |= status.query_log_entries
            > self.settings.query_log.max_entries as u64
            || status.query_log_bytes > self.settings.query_log.max_bytes
            || status.history_samples > self.settings.statistics.max_samples as u64;
        if self.settings_pending {
            let now = self
                .cleanup_wall
                .saturating_add(
                    self.cleanup_anchor
                        .elapsed()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                )
                .max(now_ms());
            let oldest_log = self
                .conn
                .query_row(
                    "SELECT retention_ms FROM query_log ORDER BY retention_ms LIMIT 1",
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .optional();
            let oldest_point = self
                .conn
                .query_row(
                    "SELECT retention_ms FROM statistics_points ORDER BY retention_ms LIMIT 1",
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .optional();
            status.capacity_pending |= oldest_log.ok().flatten().is_some_and(|at| {
                at <= now.saturating_sub(self.settings.query_log.retention_secs * 1000)
            }) || oldest_point.ok().flatten().is_some_and(|at| {
                at <= now.saturating_sub(self.settings.statistics.retention_secs * 1000)
            });
        }
        if !status.capacity_pending {
            status.applied_revision = self.revision;
            self.settings_pending = false;
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
