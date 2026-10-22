//! Wall-clock time as Unix timestamps, and the retry schedule of background fetches.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, or 0 when the clock is set before it.
pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Seconds since the Unix epoch, or 0 when the clock is set before it.
pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Milliseconds to wait after the given number of consecutive failed fetches:
/// 5 minutes after the first, 15 after the second, then one hour. `jitter`
/// adjusts that delay; pass [`std::convert::identity`] to keep it exact.
pub(crate) fn retry_backoff_ms(failures: u32, jitter: impl FnOnce(u64) -> u64) -> u64 {
    jitter(match failures {
        1 => 300_000,
        2 => 900_000,
        _ => 3_600_000,
    })
}
