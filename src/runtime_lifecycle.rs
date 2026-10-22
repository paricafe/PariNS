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
    let work = tokio::task::spawn_blocking(move || persistence.consume_startup(&settings));
    match tokio::time::timeout(Duration::from_secs(5), work).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("cache startup consumption timed out; DNS will not start"),
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

/// Cache snapshot save and runtime-data drain run in parallel, each bounded by
/// this budget, so a slow save cannot starve the drain.
const TERMINAL_BUDGET: Duration = Duration::from_secs(10);
/// The final storage close, after the save, drains only what arrived since.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// No caller may enter serving again after this boundary. A save failure must
/// still let independent log/statistics persistence finish. Storage releases
/// the runtime directory lock only after the snapshot save has finished.
pub async fn terminal(
    services: Arc<RuntimeServices>,
    persistence: Arc<CachePersistence>,
    stopped: Option<(Arc<Resolver>, Config, bool)>,
) -> SnapshotReport {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let save = async move {
        let mut report = SnapshotReport {
            reason: Some("shutdown_not_quiescent".into()),
            ..Default::default()
        };
        if let Some((resolver, _, false)) = &stopped {
            // Listener causes are bounded names; other drain failures (filters,
            // reload, managed task) are reported generically.
            let cause = resolver
                .forced_shutdown()
                .map_or("drain_incomplete", |cause| cause.as_str());
            eprintln!("cache snapshot not saved: shutdown not quiescent ({cause})");
            report.reason = Some(format!("shutdown_not_quiescent: {cause}"));
        }
        if let Some((resolver, config, true)) = stopped {
            let started = Instant::now();
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
                Ok(saved) => {
                    // Skipped entries were expired or over the per-entry or
                    // total byte limits when the snapshot was written.
                    eprintln!(
                        "cache snapshot saved: {} entries, {} skipped, {} bytes in {} ms{}",
                        saved.saved,
                        saved.skipped,
                        saved.bytes,
                        started.elapsed().as_millis(),
                        saved
                            .reason
                            .as_deref()
                            .map_or(String::new(), |reason| format!(" ({reason})"))
                    );
                    report = saved;
                }
                Err(error) => {
                    eprintln!("cache save incomplete: {error:#}");
                    report.reason = Some(format!("cache save incomplete: {error:#}"));
                }
            }
        }
        report
    };
    let save = async {
        match tokio::time::timeout(TERMINAL_BUDGET, save).await {
            Ok(report) => Some(report),
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                eprintln!("cache snapshot save exceeded 10 seconds; not saved");
                None
            }
        }
    };
    let drain = tokio::time::timeout(TERMINAL_BUDGET, services.storage.flush());
    let (report, drained) = tokio::join!(save, drain);
    let Some(report) = report else {
        // The cancelled save may still be finishing its blocking work; keep the
        // directory lock until the process exits.
        match drained {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("runtime data flush incomplete: {error}"),
            Err(_) => eprintln!("runtime data flush exceeded 10 seconds"),
        }
        return SnapshotReport {
            reason: Some("terminal_persistence_timeout".into()),
            ..Default::default()
        };
    };
    // Close reports the final storage outcome; drain failures are counted as
    // dropped records in storage status.
    match tokio::time::timeout(CLOSE_BUDGET, services.storage.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("runtime data flush incomplete: {error}"),
        Err(_) => eprintln!("runtime data close exceeded 2 seconds; persistence incomplete"),
    }
    report
}
