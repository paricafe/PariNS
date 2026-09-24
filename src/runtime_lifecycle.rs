//! Shared startup/terminal boundaries for file and managed processes.
use crate::{
    cache::{
        Cache,
        persistence::{SnapshotReport, semantic_fingerprint},
    },
    cache_persistence::{CachePersistence, ConsumedSnapshot},
    config::{CachePersistenceConfig, Config},
    resolver::Resolver,
    runtime_services::RuntimeServices,
};
use anyhow::Result;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

pub async fn consume(
    persistence: Arc<CachePersistence>,
    settings: CachePersistenceConfig,
) -> Result<ConsumedSnapshot> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let work = tokio::task::spawn_blocking(move || persistence.consume_startup(&settings, &flag));
    match tokio::time::timeout(Duration::from_secs(5), work).await {
        Ok(result) => result?,
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            anyhow::bail!("cache startup consumption timed out; DNS will not start")
        }
    }
}

pub async fn restore(
    snapshot: ConsumedSnapshot,
    cache: Arc<Cache>,
    fingerprint: String,
) -> Result<SnapshotReport> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let work = tokio::task::spawn_blocking(move || {
        snapshot.restore_into(
            &cache,
            &fingerprint,
            SystemTime::now(),
            Instant::now(),
            &flag,
        )
    });
    match tokio::time::timeout(Duration::from_secs(5), work).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            anyhow::bail!("cache restore timed out; candidate DNS will not start")
        }
    }
}

/// No caller may enter serving again after this boundary. A save failure must
/// still let independent log/statistics persistence finish.
pub async fn terminal(
    services: Arc<RuntimeServices>,
    persistence: Arc<CachePersistence>,
    stopped: Option<(Arc<Resolver>, Config, bool)>,
) -> SnapshotReport {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let closing = async move {
        let mut report = SnapshotReport {
            reason: Some("shutdown_not_quiescent".into()),
            ..Default::default()
        };
        if let Some((resolver, config, true)) = stopped {
            let prepared = semantic_fingerprint(&config, &resolver.policy_digest());
            let result = match prepared {
                Ok(fingerprint) => {
                    let cache = resolver.cache();
                    tokio::task::spawn_blocking(move || {
                        persistence.save_terminal(
                            &cache,
                            &fingerprint,
                            &config.cache.persistence,
                            SystemTime::now(),
                            Instant::now(),
                            &flag,
                        )
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(saved) => report = saved,
                Err(error) => {
                    eprintln!("cache save incomplete: {error:#}");
                    report.reason = Some(format!("cache save incomplete: {error:#}"));
                }
            }
        }
        if let Err(error) = services.shutdown().await {
            eprintln!("runtime data flush incomplete: {error}");
        }
        report
    };
    match tokio::time::timeout(Duration::from_secs(10), closing).await {
        Ok(report) => report,
        Err(_) => {
            cancelled.store(true, Ordering::Release);
            eprintln!("runtime shutdown exceeded 10 seconds; persistence incomplete");
            SnapshotReport {
                reason: Some("terminal_persistence_timeout".into()),
                ..Default::default()
            }
        }
    }
}
