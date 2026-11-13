//! One monotonic launch budget around verified process observations.
use super::journal::Readiness;
use crate::update::ipc::{Health, PendingLaunch};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const LAUNCH_WAIT: Duration = Duration::from_secs(180);

pub(super) async fn wait(
    pending: &PendingLaunch,
    revision: u64,
    mut observe: impl AsyncFnMut() -> Result<(Option<Health>, u32, String)>,
) -> Result<()> {
    let start = Instant::now();
    let deadline = start + LAUNCH_WAIT;
    let mut readiness = Readiness::default();
    let mut invocation = String::new();
    while Instant::now() < deadline {
        let (health, pid, current) = tokio::time::timeout_at(deadline, observe())
            .await
            .context("readiness_failed")??;
        if Instant::now() >= deadline {
            break;
        }
        if invocation != current {
            readiness = Readiness::default();
            invocation = current.clone();
        }
        if readiness.observe(
            start.elapsed().as_millis() as u64,
            health.as_ref(),
            pending,
            pid,
            &current,
            revision,
        ) {
            return Ok(());
        }
        tokio::time::sleep_until((Instant::now() + Duration::from_millis(500)).min(deadline)).await;
    }
    bail!("readiness_failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{
        build_info::BuildInfo,
        ipc::{InstalledIdentity, Phase},
    };

    fn sample() -> (PendingLaunch, Health) {
        let identity = InstalledIdentity {
            build: BuildInfo::current(),
            sha256: "a".repeat(64),
        };
        let pending = PendingLaunch {
            operation_id: "1".repeat(32),
            launch_nonce: "2".repeat(32),
            identity: identity.clone(),
            skip_cache_restore: false,
            phase: Phase::Starting,
            phase_nonce: "3".repeat(32),
            initialized: true,
        };
        let health = Health {
            operation_id: pending.operation_id.clone(),
            launch_nonce: pending.launch_nonce.clone(),
            identity,
            config_revision: 7,
            dns_generation: 1,
            dns_ready: true,
            storage_healthy: true,
            storage_settings_applied: true,
            sample_seq: 1,
            pid: 123,
            invocation_id: "4".repeat(32),
        };
        (pending, health)
    }

    #[tokio::test(start_paused = true)]
    async fn slow_storage_start_and_restoration_require_ten_fresh_seconds() {
        for restored in [false, true] {
            let (mut pending, mut health) = sample();
            if restored {
                pending.phase = Phase::AwaitingReadiness;
                pending.skip_cache_restore = true;
                pending.identity.build.helper_protocol -= 1;
                health.identity = pending.identity.clone();
            }
            let start = Instant::now();
            wait(&pending, 7, async || {
                let ready = start.elapsed() >= Duration::from_secs(109);
                health.storage_healthy = ready;
                health.storage_settings_applied = ready;
                health.sample_seq += 1;
                Ok((
                    Some(health.clone()),
                    health.pid,
                    health.invocation_id.clone(),
                ))
            })
            .await
            .unwrap();
            assert!(start.elapsed() >= Duration::from_secs(119));
            assert!(start.elapsed() < Duration::from_secs(120));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unhealthy_mismatched_and_stalled_receipts_exhaust_the_budget() {
        for case in 0..4 {
            let (pending, mut health) = sample();
            match case {
                0 => health.storage_settings_applied = false,
                1 => health.launch_nonce = "5".repeat(32),
                _ => (),
            }
            let start = Instant::now();
            let result = wait(&pending, 7, async || {
                if case == 3 {
                    health.storage_healthy = start.elapsed() >= Duration::from_secs(175);
                }
                if case != 2 {
                    health.sample_seq += 1;
                }
                Ok((
                    Some(health.clone()),
                    health.pid,
                    health.invocation_id.clone(),
                ))
            })
            .await;
            assert_eq!(result.unwrap_err().to_string(), "readiness_failed");
            assert_eq!(start.elapsed(), LAUNCH_WAIT);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hung_observation_cannot_extend_the_launch_budget() {
        let (pending, _) = sample();
        let start = Instant::now();
        let result = wait(&pending, 7, async || std::future::pending().await).await;
        assert_eq!(result.unwrap_err().to_string(), "readiness_failed");
        assert_eq!(start.elapsed(), LAUNCH_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_replacement_process_starts_a_new_stability_window() {
        let (pending, mut health) = sample();
        let start = Instant::now();
        wait(&pending, 7, async || {
            if start.elapsed() >= Duration::from_secs(9) {
                health.pid = 456;
                health.invocation_id = "6".repeat(32);
            }
            health.sample_seq += 1;
            Ok((
                Some(health.clone()),
                health.pid,
                health.invocation_id.clone(),
            ))
        })
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(19));
    }
}
