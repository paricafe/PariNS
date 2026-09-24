//! Indexed coverage and fixed-cardinality cleanup/drop/filesystem observations.
use super::*;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Coverage {
    pub earliest_time_ms: Option<u64>,
    pub latest_time_ms: Option<u64>,
    pub span_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupReason {
    Age,
    EntryLimit,
    ByteLimit,
    DatabaseLimit,
    Manual,
}
impl CleanupReason {
    pub(super) const ALL: [Self; 5] = [
        Self::Age,
        Self::EntryLimit,
        Self::ByteLimit,
        Self::DatabaseLimit,
        Self::Manual,
    ];
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Age => "cleanup_age",
            Self::EntryLimit => "cleanup_entry_limit",
            Self::ByteLimit => "cleanup_byte_limit",
            Self::DatabaseLimit => "cleanup_database_limit",
            Self::Manual => "cleanup_manual",
        }
    }
    pub(super) fn code(self) -> u64 {
        self as u64 + 1
    }
    pub(super) fn from_code(code: u64) -> Option<Self> {
        code.checked_sub(1)
            .and_then(|i| Self::ALL.get(i as usize))
            .copied()
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Removed {
    pub age: u64,
    pub entry_limit: u64,
    pub byte_limit: u64,
    pub database_limit: u64,
    pub manual: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Cleanup {
    pub last_reason: Option<CleanupReason>,
    pub last_at_ms: Option<u64>,
    pub removed: Removed,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct DropCounts {
    pub queue_full: u64,
    pub io_failure: u64,
    pub entry_too_large: u64,
    pub stopped: u64,
}
impl DropCounts {
    pub(super) fn total(&self) -> u64 {
        self.queue_full
            .saturating_add(self.io_failure)
            .saturating_add(self.entry_too_large)
            .saturating_add(self.stopped)
    }
}
#[derive(Default)]
pub(super) struct Drops {
    pub queue_full: AtomicU64,
    pub io_failure: AtomicU64,
    pub entry_too_large: AtomicU64,
    pub stopped: AtomicU64,
}
impl Drops {
    pub fn snapshot(&self) -> DropCounts {
        DropCounts {
            queue_full: self.queue_full.load(Ordering::Relaxed),
            io_failure: self.io_failure.load(Ordering::Relaxed),
            entry_too_large: self.entry_too_large.load(Ordering::Relaxed),
            stopped: self.stopped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Filesystem {
    pub sampled_at_ms: Option<u64>,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub error: Option<String>,
    pub stale: bool,
    pub low_space: Option<bool>,
    pub warning_threshold_bytes: u64,
}
impl Filesystem {
    pub(super) fn apply_settings(&mut self, settings: &RuntimeSettings) {
        self.warning_threshold_bytes = settings
            .storage
            .max_database_bytes
            .saturating_add(settings.cache_snapshot_max_bytes.saturating_mul(2))
            .saturating_add(67_108_864);
        self.low_space = self
            .available_bytes
            .zip(self.total_bytes)
            .map(|(available, total)| {
                available < self.warning_threshold_bytes || available < total / 10
            });
    }
    pub(super) fn sample(directory: Option<&Path>, settings: &RuntimeSettings) -> Self {
        let mut result = Self {
            sampled_at_ms: Some(now_ms()),
            ..Default::default()
        };
        result.apply_settings(settings);
        let Some(path) = directory else {
            result.error = Some("ephemeral_storage".into());
            return result;
        };
        #[cfg(unix)]
        {
            match rustix::fs::statvfs(path) {
                Ok(stat) => {
                    let available = stat.f_bavail.saturating_mul(stat.f_frsize);
                    let total = stat.f_blocks.saturating_mul(stat.f_frsize);
                    result.total_bytes = Some(total);
                    result.available_bytes = Some(available);
                    result.apply_settings(settings);
                }
                Err(_) => {
                    result.error = Some("filesystem_unavailable".into());
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            result.error = Some("platform_unsupported".into());
        }
        result
    }
}
