//! Wall-clock time as Unix timestamps, and the retry schedule of background fetches.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn retry_backoff_ms(failures: u32, jitter: impl FnOnce(u64) -> u64) -> u64 {
    jitter(match failures {
        1 => 300_000,
        2 => 900_000,
        _ => 3_600_000,
    })
}
