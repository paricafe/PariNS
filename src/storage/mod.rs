//! Private runtime storage. One SQLite owner; DNS producers never wait for IO.
mod database;
pub(crate) use database::DATABASE_FORMAT;
mod observation;
mod statistics;
#[cfg(test)]
mod tests;

use crate::{
    metrics::Metrics,
    query_log::{Entry, ListOptions, Page},
};
pub use observation::{Cleanup, CleanupReason, Coverage, DropCounts, Filesystem};
use serde::{Deserialize, Serialize};
pub use statistics::{HistoryOptions, Point, StatisticsView, Totals};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub enum Error {
    Conflict { current_epoch: u64 },
    StorageUnavailable(String),
    Busy,
    Invalid(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { current_epoch } => {
                write!(f, "storage epoch conflict; current epoch {current_epoch}")
            }
            Self::StorageUnavailable(message) => write!(f, "STORAGE_UNAVAILABLE: {message}"),
            Self::Busy => write!(f, "storage control queue is full"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::StorageUnavailable(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::StorageUnavailable(error.to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub max_database_bytes: u64,
    pub flush_interval_ms: u64,
    pub cleanup_interval_secs: u64,
    pub queue_max_entries: usize,
    pub queue_max_bytes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_database_bytes: 134_217_728,
            flush_interval_ms: 1000,
            cleanup_interval_secs: 60,
            queue_max_entries: 4096,
            queue_max_bytes: 8_388_608,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (16_777_216..=4_294_967_296).contains(&self.max_database_bytes),
            "storage.max_database_bytes must be 16777216..=4294967296"
        );
        anyhow::ensure!(
            (100..=5000).contains(&self.flush_interval_ms),
            "storage.flush_interval_ms must be 100..=5000"
        );
        anyhow::ensure!(
            (10..=3600).contains(&self.cleanup_interval_secs),
            "storage.cleanup_interval_secs must be 10..=3600"
        );
        anyhow::ensure!(
            (128..=65536).contains(&self.queue_max_entries),
            "storage.queue_max_entries must be 128..=65536"
        );
        anyhow::ensure!(
            (1_048_576..=67_108_864).contains(&self.queue_max_bytes),
            "storage.queue_max_bytes must be 1048576..=67108864"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct StatisticsSettings {
    pub retention_secs: u64,
    pub max_samples: usize,
    pub reset_interval_days: u64,
}
impl Default for StatisticsSettings {
    fn default() -> Self {
        Self {
            retention_secs: 604800,
            max_samples: 10080,
            reset_interval_days: 0,
        }
    }
}
impl StatisticsSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (3600..=31_536_000).contains(&self.retention_secs),
            "statistics.retention_secs must be 3600..=31536000"
        );
        anyhow::ensure!(
            (60..=525600).contains(&self.max_samples),
            "statistics.max_samples must be 60..=525600"
        );
        anyhow::ensure!(
            self.reset_interval_days <= 3650,
            "statistics.reset_interval_days must be 0..=3650"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeSettings {
    pub storage: Settings,
    pub query_log: crate::query_log::Settings,
    pub statistics: StatisticsSettings,
    pub cache_snapshot_max_bytes: u64,
}
impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            storage: Default::default(),
            query_log: Default::default(),
            statistics: Default::default(),
            cache_snapshot_max_bytes: 33_554_432,
        }
    }
}
impl RuntimeSettings {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            storage: config.storage.clone(),
            query_log: config.query_log.clone(),
            statistics: config.statistics.clone(),
            cache_snapshot_max_bytes: config.cache.persistence.max_bytes as u64,
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        self.storage.validate()?;
        self.query_log.validate()?;
        self.statistics.validate()?;
        anyhow::ensure!(
            self.query_log.max_bytes <= self.storage.max_database_bytes,
            "query_log.max_bytes must not exceed storage.max_database_bytes"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub health: String,
    pub error: Option<String>,
    pub configured_revision: u64,
    pub applied_revision: u64,
    pub capacity_pending: bool,
    pub database_bytes: u64,
    pub journal_bytes: u64,
    pub cache_snapshot_bytes: u64,
    pub query_log_entries: u64,
    pub query_log_bytes: u64,
    pub history_samples: u64,
    pub pending_entries: usize,
    pub pending_bytes: u64,
    pub pending_points: usize,
    pub dropped_logs: u64,
    pub dropped_points: u64,
    pub last_commit_at_ms: Option<u64>,
    pub persist_lag_ms: u64,
    pub log_epoch: u64,
    pub totals_epoch: u64,
    pub history_epoch: u64,
    pub clock_rollback: bool,
    pub query_log_coverage: Coverage,
    pub cleanup: Cleanup,
    pub dropped_by_reason: DropCounts,
    pub filesystem: Filesystem,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            health: "starting".into(),
            error: None,
            configured_revision: 0,
            applied_revision: 0,
            capacity_pending: false,
            database_bytes: 0,
            journal_bytes: 0,
            cache_snapshot_bytes: 0,
            query_log_entries: 0,
            query_log_bytes: 0,
            history_samples: 0,
            pending_entries: 0,
            pending_bytes: 0,
            pending_points: 0,
            dropped_logs: 0,
            dropped_points: 0,
            last_commit_at_ms: None,
            persist_lag_ms: 0,
            log_epoch: 0,
            totals_epoch: 0,
            history_epoch: 0,
            clock_rollback: false,
            query_log_coverage: Default::default(),
            cleanup: Default::default(),
            dropped_by_reason: Default::default(),
            filesystem: Default::default(),
        }
    }
}
#[derive(Debug, Serialize)]
pub struct ClearResult {
    pub epoch: u64,
    pub removed: u64,
}

struct Desired {
    settings: RuntimeSettings,
    revision: u64,
}
pub(super) struct Shared {
    desired: Mutex<Desired>,
    status: Mutex<Status>,
    enabled: AtomicBool,
    log_epoch: AtomicU64,
    queue_entries: AtomicUsize,
    queue_bytes: AtomicU64,
    limit_entries: AtomicUsize,
    limit_bytes: AtomicU64,
    dropped: observation::Drops,
    stop: AtomicBool,
    stats: Mutex<statistics::Owner>,
    metrics: Arc<Metrics>,
}
pub(super) struct LogItem {
    epoch: u64,
    payload: Vec<u8>,
    charge: u64,
    enqueued_at: Instant,
}
type Reply<T> = oneshot::Sender<Result<T>>;
#[cfg(test)]
type Inspection = Box<dyn FnOnce(&mut rusqlite::Connection) -> Result<()> + Send>;
enum Command {
    List(ListOptions, Reply<Page>),
    ClearLogs(u64, Reply<ClearResult>),
    Statistics(HistoryOptions, Reply<StatisticsView>),
    ResetTotals(u64, Reply<ClearResult>),
    ClearHistory(u64, Reply<ClearResult>),
    Flush(Reply<()>),
    Shutdown(Reply<()>),
    #[cfg(test)]
    Inspect(Inspection, Reply<()>),
}
struct Inner {
    shared: Arc<Shared>,
    logs: mpsc::SyncSender<LogItem>,
    controls: mpsc::SyncSender<Command>,
    worker: thread::Thread,
}
impl Drop for Inner {
    fn drop(&mut self) {
        // No joining on a Tokio worker. Explicit shutdown is the durable boundary.
        self.shared.stop.store(true, Ordering::Release);
        self.worker.unpark();
    }
}
#[derive(Clone)]
pub struct Handle(Arc<Inner>);

impl Handle {
    pub fn open(
        dir: &Path,
        settings: RuntimeSettings,
        revision: u64,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<Self> {
        settings.validate()?;
        let (dir, lock) = private_directory(dir)?;
        Self::start(Some((dir, lock)), settings, revision, metrics)
    }
    pub fn ephemeral(settings: RuntimeSettings, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        settings.validate()?;
        Self::start(None, settings, 0, metrics)
    }
    fn start(
        directory: Option<(PathBuf, File)>,
        settings: RuntimeSettings,
        revision: u64,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            enabled: AtomicBool::new(settings.query_log.enabled),
            log_epoch: AtomicU64::new(0),
            limit_entries: AtomicUsize::new(settings.storage.queue_max_entries),
            limit_bytes: AtomicU64::new(settings.storage.queue_max_bytes),
            desired: Mutex::new(Desired { settings, revision }),
            status: Mutex::new(Status::default()),
            queue_entries: AtomicUsize::new(0),
            queue_bytes: AtomicU64::new(0),
            dropped: Default::default(),
            stop: AtomicBool::new(false),
            stats: Mutex::new(statistics::Owner::new(&metrics)),
            metrics,
        });
        let (logs, incoming) = mpsc::sync_channel(65536);
        let (controls, commands) = mpsc::sync_channel(16);
        let (ready, receive) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let join = thread::Builder::new()
            .name("parins-storage".into())
            .spawn(move || {
                database::run(directory, worker_shared, incoming, commands, ready);
            })?;
        receive
            .recv()
            .map_err(|_| anyhow::anyhow!("storage thread failed to initialize"))?;
        let handle = Self(Arc::new(Inner {
            shared: shared.clone(),
            logs,
            controls,
            worker: join.thread().clone(),
        }));
        let sampler_shared = shared;
        thread::Builder::new()
            .name("parins-statistics".into())
            .spawn(move || {
                while !sampler_shared.stop.load(Ordering::Acquire) {
                    sampler_shared
                        .stats
                        .lock()
                        .expect("statistics owner")
                        .sample(&sampler_shared.metrics, false);
                    thread::sleep(Duration::from_millis(100));
                }
            })?;
        Ok(handle)
    }
    /// Called only after the authoritative configuration transaction commits.
    pub fn publish_settings(&self, settings: RuntimeSettings, revision: u64) {
        let mut desired = self.0.shared.desired.lock().expect("storage settings");
        if revision < desired.revision {
            return;
        }
        self.0
            .shared
            .enabled
            .store(settings.query_log.enabled, Ordering::Release);
        self.0
            .shared
            .limit_entries
            .store(settings.storage.queue_max_entries, Ordering::Relaxed);
        self.0
            .shared
            .limit_bytes
            .store(settings.storage.queue_max_bytes, Ordering::Relaxed);
        *desired = Desired { settings, revision };
        self.0.worker.unpark();
    }
    pub fn status(&self) -> Status {
        status(&self.0.shared)
    }
    pub fn begin_log(&self) -> Option<u64> {
        self.0
            .shared
            .enabled
            .load(Ordering::Acquire)
            .then(|| self.0.shared.log_epoch.load(Ordering::Acquire))
    }
    pub(crate) fn record_log(&self, epoch: u64, entry: Entry) {
        let s = &self.0.shared;
        if !s.enabled.load(Ordering::Acquire) || epoch != s.log_epoch.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut payload) = serde_json::to_vec(&entry) else {
            s.dropped.entry_too_large.fetch_add(1, Ordering::Relaxed);
            return;
        };
        payload.shrink_to_fit();
        let charge = (payload.capacity() + std::mem::size_of::<LogItem>()) as u64;
        if charge > 131072 {
            s.dropped.entry_too_large.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if s.stop.load(Ordering::Acquire) {
            s.dropped.stopped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if s.queue_entries
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
                (n < s.limit_entries.load(Ordering::Relaxed)).then_some(n + 1)
            })
            .is_err()
        {
            s.dropped.queue_full.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if s.queue_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
                n.checked_add(charge)
                    .filter(|total| *total <= s.limit_bytes.load(Ordering::Relaxed))
            })
            .is_err()
        {
            s.queue_entries.fetch_sub(1, Ordering::Relaxed);
            s.dropped.queue_full.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self
            .0
            .logs
            .try_send(LogItem {
                epoch,
                payload,
                charge,
                enqueued_at: Instant::now(),
            })
            .is_err()
        {
            s.queue_entries.fetch_sub(1, Ordering::Relaxed);
            s.queue_bytes.fetch_sub(charge, Ordering::Relaxed);
            s.dropped.queue_full.fetch_add(1, Ordering::Relaxed);
        }
        if s.queue_entries.load(Ordering::Relaxed) >= 256 {
            self.0.worker.unpark();
        }
    }
    pub fn set_dns_state(&self, running: bool, generation: u64) {
        self.0
            .shared
            .stats
            .lock()
            .expect("statistics owner")
            .set_dns_state(&self.0.shared.metrics, running, generation);
    }
    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (send, receive) = oneshot::channel();
        self.0.controls.try_send(make(send)).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::Busy,
            mpsc::TrySendError::Disconnected(_) => {
                Error::StorageUnavailable("worker stopped".into())
            }
        })?;
        self.0.worker.unpark();
        tokio::time::timeout(Duration::from_secs(10), receive)
            .await
            .map_err(|_| {
                Error::StorageUnavailable("operation timed out; outcome may be unknown".into())
            })?
            .map_err(|_| {
                Error::StorageUnavailable("worker stopped; outcome may be unknown".into())
            })?
    }
    pub async fn list_logs(&self, options: ListOptions) -> Result<Page> {
        options
            .validate()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.request(|reply| Command::List(options, reply)).await
    }
    pub async fn clear_logs(&self, epoch: u64) -> Result<ClearResult> {
        self.request(|r| Command::ClearLogs(epoch, r)).await
    }
    pub async fn statistics(&self, options: HistoryOptions) -> Result<StatisticsView> {
        options.validate()?;
        self.request(|r| Command::Statistics(options, r)).await
    }
    pub async fn reset_totals(&self, epoch: u64) -> Result<ClearResult> {
        self.request(|r| Command::ResetTotals(epoch, r)).await
    }
    pub async fn clear_history(&self, epoch: u64) -> Result<ClearResult> {
        self.request(|r| Command::ClearHistory(epoch, r)).await
    }
    pub async fn flush(&self) -> Result<()> {
        self.request(Command::Flush).await
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.request(Command::Shutdown).await
    }
}

fn status(shared: &Shared) -> Status {
    let mut result = shared.status.lock().expect("storage status").clone();
    {
        let desired = shared.desired.lock().expect("storage settings");
        result.configured_revision = desired.revision;
        result.filesystem.apply_settings(&desired.settings);
    }
    result.pending_entries = shared.queue_entries.load(Ordering::Relaxed);
    result.pending_bytes = shared.queue_bytes.load(Ordering::Relaxed);
    result.dropped_by_reason = shared.dropped.snapshot();
    result.dropped_logs = result.dropped_by_reason.total();
    result.filesystem.stale = result
        .filesystem
        .sampled_at_ms
        .is_some_and(|at| now_ms().saturating_sub(at) > 120_000);
    let stats = shared.stats.lock().expect("statistics owner");
    result.pending_points = stats.points.len();
    result.dropped_points = stats.dropped_points;
    result.persist_lag_ms =
        if result.pending_entries > 0 || !stats.points.is_empty() || stats.latest.is_some() {
            now_ms().saturating_sub(result.last_commit_at_ms.unwrap_or(stats.since_ms))
        } else {
            0
        };
    result
}
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}

fn private_directory(path: &Path) -> anyhow::Result<(PathBuf, File)> {
    use crate::manage::store::{checked_open, create_private, private_permissions};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    // The caller-specified root can be systemd's DynamicUser logical link.
    // Managed mode validates its owned `runtime` child before calling us.
    // All component files are checked only inside the resolved private root.
    let dir = path.canonicalize()?;
    anyhow::ensure!(
        dir.parent().is_some(),
        "runtime directory cannot be filesystem root: {}",
        path.display()
    );
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(home) = Path::new(&home).canonicalize()
    {
        anyhow::ensure!(
            dir != home,
            "runtime directory cannot be HOME: {}; use a dedicated private subdirectory",
            path.display()
        );
    }
    private_permissions(&std::fs::metadata(&dir)?, 0o700)?;
    let lock = match create_private(&dir.join("lock")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            checked_open(&dir.join("lock"), true)?
                .ok_or_else(|| anyhow::anyhow!("runtime lock disappeared"))?
        }
        Err(e) => return Err(e.into()),
    };
    lock.try_lock().map_err(|e| {
        anyhow::anyhow!("runtime directory {} is already in use: {e}", dir.display())
    })?;
    for name in ["observability.sqlite3", "observability.sqlite3-journal"] {
        checked_open(&dir.join(name), false)?;
    }
    Ok((dir, lock))
}
