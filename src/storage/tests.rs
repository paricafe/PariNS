use super::*;
use crate::{metrics::Counter, query_log::Entry, runtime_services::RuntimeServices};

fn settings() -> RuntimeSettings {
    let mut settings = RuntimeSettings::default();
    settings.query_log.enabled = true;
    settings
}
fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
fn entry(cache: &str) -> Entry {
    let mut entry = Entry::request(None, "192.0.2.10".parse().unwrap(), "udp");
    entry.name = "example.test.".into();
    entry.cache = cache.into();
    entry.status = if cache == "blocked" {
        "blocked"
    } else {
        "success"
    }
    .into();
    entry
}
async fn inspect(
    handle: &Handle,
    f: impl FnOnce(&mut rusqlite::Connection) -> Result<()> + Send + 'static,
) -> Result<()> {
    handle
        .request(|reply| Command::Inspect(Box::new(f), reply))
        .await
}
async fn pause(handle: &Handle) -> (mpsc::Sender<()>, tokio::task::JoinHandle<Result<()>>) {
    let (entered, wait) = oneshot::channel();
    let (release, blocked) = mpsc::channel();
    let handle = handle.clone();
    let task = tokio::spawn(async move {
        inspect(&handle, move |_| {
            let _ = entered.send(());
            blocked
                .recv_timeout(Duration::from_secs(3))
                .expect("release SQLite worker");
            Ok(())
        })
        .await
    });
    wait.await.unwrap();
    (release, task)
}

#[tokio::test]
async fn committed_logs_totals_and_points_survive_reopen_without_double_counting() {
    let dir = private_dir();
    let runtime = RuntimeServices::open(dir.path(), settings(), 1).unwrap();
    runtime.set_dns_state(true, 1);
    runtime.storage.record_log(0, entry("fresh"));
    runtime.metrics.inc(Counter::Requests);
    runtime.metrics.inc(Counter::CacheHits);
    runtime.flush().await.unwrap();
    let before = runtime.statistics(HistoryOptions::default()).await.unwrap();
    assert_eq!(before.totals.metrics.counters["requests"], 1);
    assert!(!before.samples.is_empty());
    let id = runtime
        .list_logs(ListOptions::default())
        .await
        .unwrap()
        .entries[0]
        .id;
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let runtime = RuntimeServices::open(dir.path(), settings(), 2).unwrap();
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .entries[0]
            .id,
        id
    );
    let recovered = runtime.statistics(HistoryOptions::default()).await.unwrap();
    assert_eq!(recovered.totals.metrics.counters["requests"], 1);
    assert_eq!(recovered.process_metrics.counters["requests"], 0);
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    runtime.flush().await.unwrap();
    assert_eq!(
        runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .totals
            .metrics
            .counters["requests"],
        2
    );
    runtime.shutdown().await.unwrap();
    let runtime = RuntimeServices::open(dir.path(), settings(), 3).unwrap();
    assert_eq!(
        runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .totals
            .metrics
            .counters["requests"],
        2
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn clear_epoch_rejects_queued_and_inflight_records_and_ids_never_reuse() {
    let runtime = RuntimeServices::ephemeral(settings());
    runtime.storage.record_log(0, entry("fresh"));
    runtime.flush().await.unwrap();
    let old_id = runtime
        .list_logs(ListOptions::default())
        .await
        .unwrap()
        .entries[0]
        .id;
    let (release, paused) = pause(&runtime.storage).await;
    for _ in 0..8 {
        runtime.storage.record_log(0, entry("stale"));
    }
    let handle = runtime.storage.clone();
    let clear = tokio::spawn(async move { handle.clear_logs(0).await });
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    paused.await.unwrap().unwrap();
    assert_eq!(clear.await.unwrap().unwrap().epoch, 1);
    runtime.storage.record_log(0, entry("fresh"));
    runtime.flush().await.unwrap();
    assert!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    runtime.storage.record_log(1, entry("upstream"));
    runtime.flush().await.unwrap();
    assert!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .entries[0]
            .id
            > old_id
    );
    assert!(matches!(
        runtime.clear_logs(0).await,
        Err(Error::Conflict { current_epoch: 1 })
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn classifications_are_anded_before_pagination_and_disabled_history_remains() {
    let runtime = RuntimeServices::ephemeral(settings());
    for cache in [
        "fresh",
        "stale",
        "upstream",
        "blocked",
        "error",
        "cancelled",
    ] {
        runtime.storage.record_log(0, entry(cache));
    }
    runtime.flush().await.unwrap();
    let first = runtime
        .list_logs(ListOptions {
            cache: Some("cached".into()),
            status: Some("success".into()),
            search: Some("EXAMPLE".into()),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.total, 6);
    assert_eq!(first.entries[0].cache, "stale");
    let next = runtime
        .list_logs(ListOptions {
            cache: Some("cached".into()),
            before_id: first.next_cursor,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(next.entries.len(), 1);
    assert_eq!(next.entries[0].cache, "fresh");
    let non_cached = runtime
        .list_logs(ListOptions {
            cache: Some("non_cached".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(non_cached.entries.len(), 4);
    let mut updated = settings();
    updated.query_log.enabled = false;
    runtime.publish_settings(updated, 1);
    assert!(runtime.query_log.begin().is_none());
    let page = runtime.list_logs(ListOptions::default()).await.unwrap();
    assert!(!page.enabled);
    assert_eq!(page.total, 6);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_limits_drop_new_logs_without_losing_independent_totals() {
    let mut config = settings();
    config.storage.queue_max_entries = 128;
    let runtime = RuntimeServices::ephemeral(config);
    let (release, paused) = pause(&runtime.storage).await;
    for _ in 0..300 {
        runtime.metrics.inc(Counter::Requests);
        runtime.storage.record_log(0, entry("fresh"));
    }
    let status = runtime.status();
    assert_eq!(status.pending_entries, 128);
    assert_eq!(status.dropped_logs, 172);
    assert!(status.pending_bytes <= settings().storage.queue_max_bytes);
    release.send(()).unwrap();
    paused.await.unwrap().unwrap();
    runtime.flush().await.unwrap();
    assert_eq!(
        runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .totals
            .metrics
            .counters["requests"],
        300
    );
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total,
        128
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn byte_limit_and_reliable_settings_survive_full_queues() {
    let mut config = settings();
    config.storage.queue_max_bytes = 1_048_576;
    config.storage.queue_max_entries = 65536;
    let runtime = RuntimeServices::ephemeral(config.clone());
    let (release, paused) = pause(&runtime.storage).await;
    let mut large = entry("fresh");
    large.name = "x".repeat(60_000);
    for _ in 0..100 {
        runtime.storage.record_log(0, large.clone());
    }
    assert!(runtime.status().pending_bytes <= 1_048_576);
    assert!(runtime.status().dropped_logs > 0);
    config.query_log.enabled = false;
    runtime.publish_settings(config, 37);
    assert!(runtime.query_log.begin().is_none());
    assert_eq!(runtime.status().configured_revision, 37);
    release.send(()).unwrap();
    paused.await.unwrap().unwrap();
    runtime.flush().await.unwrap();
    assert_eq!(runtime.status().applied_revision, 37);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn independent_epochs_and_read_only_failure_preserve_committed_state() {
    let runtime = RuntimeServices::ephemeral(settings());
    runtime.metrics.inc(Counter::Requests);
    runtime.storage.record_log(0, entry("fresh"));
    runtime.flush().await.unwrap();
    inspect(&runtime.storage, |conn| {
        conn.execute_batch("PRAGMA query_only=ON")?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(matches!(
        runtime.reset_totals(0).await,
        Err(Error::StorageUnavailable(_))
    ));
    assert!(matches!(
        runtime.clear_history(0).await,
        Err(Error::StorageUnavailable(_))
    ));
    assert!(matches!(
        runtime.clear_logs(0).await,
        Err(Error::StorageUnavailable(_))
    ));
    let view = runtime.statistics(HistoryOptions::default()).await.unwrap();
    assert_eq!(view.totals.epoch, 0);
    assert_eq!(view.history_epoch, 0);
    assert_eq!(view.totals.metrics.counters["requests"], 1);
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total,
        1
    );
    inspect(&runtime.storage, |conn| {
        conn.execute_batch("PRAGMA query_only=OFF")?;
        Ok(())
    })
    .await
    .unwrap();
    runtime.reset_totals(0).await.unwrap();
    runtime.metrics.inc(Counter::Requests);
    runtime.clear_history(0).await.unwrap();
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    let view = runtime.statistics(HistoryOptions::default()).await.unwrap();
    assert_eq!(view.totals.epoch, 1);
    assert_eq!(view.history_epoch, 1);
    assert_eq!(view.totals.metrics.counters["requests"], 2);
    assert_eq!(view.samples.iter().map(|p| p.requests).sum::<u64>(), 1);
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total,
        1
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_database_rolls_back_batch_and_retains_prior_rows() {
    let runtime = RuntimeServices::ephemeral(settings());
    runtime.storage.record_log(0, entry("fresh"));
    runtime.flush().await.unwrap();
    inspect(&runtime.storage, |conn| {
        let pages: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        conn.pragma_update(None, "max_page_count", pages)?;
        Ok(())
    })
    .await
    .unwrap();
    let mut large = entry("upstream");
    large.name = "x".repeat(60_000);
    runtime.storage.record_log(0, large);
    assert!(runtime.flush().await.is_err());
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total,
        1
    );
    assert!(runtime.status().dropped_logs >= 1);
    inspect(&runtime.storage, |conn| {
        conn.pragma_update(None, "max_page_count", 32768)?;
        Ok(())
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_database_is_preserved_unavailable_and_keeps_single_writer_lock() {
    let dir = private_dir();
    let path = dir.path().join("observability.sqlite3");
    let mut file = crate::manage::store::create_private(&path).unwrap();
    use std::io::Write;
    file.write_all(b"not a SQLite database").unwrap();
    drop(file);
    let runtime = RuntimeServices::open(dir.path(), settings(), 1).unwrap();
    assert_eq!(runtime.status().health, "unavailable");
    assert!(matches!(
        runtime.list_logs(ListOptions::default()).await,
        Err(Error::StorageUnavailable(_))
    ));
    assert!(RuntimeServices::open(dir.path(), settings(), 1).is_err());
    runtime.metrics.inc(Counter::Requests);
    assert_eq!(runtime.metrics.snapshot().counters["requests"], 1);
    assert_eq!(std::fs::read(path).unwrap(), b"not a SQLite database");
    assert!(runtime.shutdown().await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn private_component_permissions_links_and_database_pragmas() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let runtime = RuntimeServices::open(&dir.path().join("runtime"), settings(), 1).unwrap();
    assert_eq!(
        std::fs::metadata(dir.path().join("runtime"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("runtime/observability.sqlite3"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    inspect(&runtime.storage, |conn| {
        assert_eq!(
            conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
            "delete"
        );
        assert_eq!(
            conn.query_row("PRAGMA synchronous", [], |r| r.get::<_, u64>(0))?,
            2
        );
        assert_eq!(
            conn.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, u64>(0))?,
            2
        );
        Ok(())
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let external = dir.path().join("external");
    crate::manage::store::create_private(&external).unwrap();
    let unsafe_dir = dir.path().join("unsafe");
    std::fs::create_dir(&unsafe_dir).unwrap();
    std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&external, unsafe_dir.join("observability.sqlite3")).unwrap();
    assert!(RuntimeServices::open(&unsafe_dir, settings(), 1).is_err());
    std::fs::remove_file(unsafe_dir.join("observability.sqlite3")).unwrap();
    std::fs::hard_link(&external, unsafe_dir.join("observability.sqlite3")).unwrap();
    assert!(RuntimeServices::open(&unsafe_dir, settings(), 1).is_err());
}

#[tokio::test]
async fn smaller_limits_remain_pending_until_background_cleanup_finishes() {
    let runtime = RuntimeServices::ephemeral(settings());
    for _ in 0..2000 {
        runtime.storage.record_log(0, entry("fresh"));
    }
    runtime.flush().await.unwrap();
    let original = runtime
        .list_logs(ListOptions::default())
        .await
        .unwrap()
        .total;
    assert!(original > 512);
    let mut updated = settings();
    updated.query_log.max_entries = 3;
    runtime.publish_settings(updated, 12);
    let page = runtime.list_logs(ListOptions::default()).await.unwrap();
    assert_eq!(page.storage.configured_revision, 12);
    assert!(page.storage.capacity_pending);
    assert_ne!(page.storage.applied_revision, 12);
    let deadline = Instant::now() + Duration::from_secs(3);
    while runtime.status().capacity_pending && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = runtime.status();
    assert!(!status.capacity_pending);
    assert_eq!(status.applied_revision, 12);
    assert!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total
            <= 3
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_checkpoint_cannot_replace_newer_totals_or_duplicate_minute_points() {
    let mut config = settings();
    config.storage.flush_interval_ms = 100;
    let runtime = RuntimeServices::ephemeral(config);
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    let old_view = runtime.statistics(HistoryOptions::default()).await.unwrap();
    let old_point = old_view.samples.last().unwrap().clone();
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    let before = runtime.status().history_samples;
    {
        let mut owner = runtime.storage.0.shared.stats.lock().unwrap();
        owner.latest = Some(statistics::Checkpoint {
            totals: old_view.totals,
            run_id: owner.run_id.clone(),
            seq: 1,
        });
        owner.points.push_back(old_point);
    }
    tokio::time::sleep(Duration::from_millis(180)).await;
    inspect(&runtime.storage, |conn| {
        let bytes: Vec<u8> =
            conn.query_row("SELECT payload FROM statistics_totals", [], |r| r.get(0))?;
        let totals: Totals = serde_json::from_slice(&bytes)?;
        assert_eq!(totals.metrics.counters["requests"], 2);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(runtime.status().history_samples, before);
    runtime.shutdown().await.unwrap();
}

#[test]
fn abrupt_exit_child() {
    let Some(path) = std::env::var_os("PARINS_STORAGE_CRASH_FIXTURE") else {
        return;
    };
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio.block_on(async {
        let runtime = RuntimeServices::open(Path::new(&path),settings(),1).unwrap();
        runtime.storage.record_log(0,entry("fresh")); runtime.metrics.inc(Counter::Requests); runtime.flush().await.unwrap();
        inspect(&runtime.storage, |conn| {
            conn.execute_batch("PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE query_log SET name='not-committed.test'; UPDATE metadata SET value=999 WHERE key='log_epoch';")?;
            // Exit without dropping the connection, worker or runtime. A hot
            // rollback journal, if spilled, belongs to SQLite recovery.
            std::process::exit(23);
        }).await.unwrap();
    });
}

#[tokio::test]
async fn abrupt_process_exit_keeps_committed_data_and_rolls_back_partial_transaction() {
    let directory = private_dir();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "storage::tests::abrupt_exit_child",
            "--nocapture",
        ])
        .env("PARINS_STORAGE_CRASH_FIXTURE", directory.path())
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let runtime = RuntimeServices::open(directory.path(), settings(), 2).unwrap();
    let page = runtime.list_logs(ListOptions::default()).await.unwrap();
    assert_eq!(page.log_epoch, 0);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "example.test.");
    assert_eq!(
        runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .totals
            .metrics
            .counters["requests"],
        1
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn indexed_coverage_reports_twenty_six_seconds_not_six_hour_configuration() {
    let mut settings = settings();
    settings.query_log.max_entries = 1000;
    settings.query_log.retention_secs = 21_600;
    let runtime = RuntimeServices::ephemeral(settings);
    let end = now_ms();
    for index in 0..2000 {
        let mut record = entry("fresh");
        record.time_ms = end - ((1999 - index) as f64 / 38.1 * 1000.0) as u64;
        runtime.storage.record_log(0, record);
    }
    runtime.flush().await.unwrap();
    let status = runtime.status();
    assert_eq!(status.query_log_entries, 1000);
    assert_eq!(status.cleanup.removed.entry_limit, 1000);
    assert!((26_000..=26_300).contains(&status.query_log_coverage.span_ms.unwrap()));
    assert_eq!(status.query_log_coverage.latest_time_ms, Some(end));
    assert_eq!(status.dropped_by_reason.queue_full, 0);
    runtime.clear_logs(0).await.unwrap();
    let status = runtime.status();
    assert_eq!(status.query_log_coverage.span_ms, None);
    assert_eq!(status.cleanup.removed.manual, 1000);
    assert!(matches!(
        status.cleanup.last_reason,
        Some(CleanupReason::Manual)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn byte_age_cleanup_and_io_drops_have_distinct_reasons() {
    let mut config = settings();
    config.query_log.max_bytes = 1_048_576;
    let runtime = RuntimeServices::ephemeral(config);
    let mut old = entry("fresh");
    old.time_ms = 1;
    old.duration_ms = 86_401_000.0;
    runtime.storage.record_log(0, old);
    runtime.flush().await.unwrap();
    assert_eq!(runtime.status().cleanup.removed.age, 1);
    for _ in 0..40 {
        let mut record = entry("fresh");
        record.name = "x".repeat(60_000);
        runtime.storage.record_log(0, record);
    }
    runtime.flush().await.unwrap();
    let status = runtime.status();
    assert!(status.query_log_bytes <= 1_048_576);
    assert!(status.query_log_entries > 0);
    assert!(status.cleanup.removed.byte_limit > 0);
    assert_eq!(status.cleanup.removed.entry_limit, 0);
    inspect(&runtime.storage, |conn| {
        conn.execute_batch("PRAGMA query_only=ON")?;
        Ok(())
    })
    .await
    .unwrap();
    runtime.storage.record_log(0, entry("fresh"));
    assert!(runtime.flush().await.is_err());
    let status = runtime.status();
    assert_eq!(status.dropped_by_reason.io_failure, 1);
    assert_eq!(status.dropped_by_reason.queue_full, 0);
    inspect(&runtime.storage, |conn| {
        conn.execute_batch("PRAGMA query_only=OFF")?;
        Ok(())
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn filesystem_sample_is_cached_and_snapshot_path_does_not_block_diagnostics() {
    let dir = private_dir();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        dir.path().join("absent"),
        dir.path().join("dns-cache-clean.snapshot"),
    )
    .unwrap();
    let runtime = RuntimeServices::open(dir.path(), settings(), 1).unwrap();
    let first = runtime.status().filesystem;
    #[cfg(unix)]
    {
        assert!(first.total_bytes.unwrap() > 0);
        assert!(first.available_bytes.is_some());
        assert!(first.error.is_none());
    }
    assert!(!first.stale);
    runtime.flush().await.unwrap();
    assert_eq!(
        runtime.status().filesystem.sampled_at_ms,
        first.sampled_at_ms
    );
    assert_eq!(first.warning_threshold_bytes, 268_435_456);
    runtime.shutdown().await.unwrap();
    let unavailable = Filesystem::sample(Some(&dir.path().join("absent")), &settings());
    assert!(unavailable.available_bytes.is_none());
    assert!(unavailable.total_bytes.is_none());
    assert!(unavailable.error.is_some());
}

#[tokio::test]
async fn unknown_format_is_not_migrated_or_erased() {
    let dir = private_dir();
    let runtime = RuntimeServices::open(dir.path(), settings(), 1).unwrap();
    inspect(&runtime.storage, |conn| {
        conn.execute("UPDATE metadata SET value=999 WHERE key='format'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let runtime = RuntimeServices::open(dir.path(), settings(), 1).unwrap();
    assert_eq!(runtime.status().health, "unavailable");
    assert!(
        runtime
            .status()
            .error
            .unwrap()
            .contains("unsupported runtime database format")
    );
    assert!(runtime.shutdown().await.is_err());
    let conn = rusqlite::Connection::open(dir.path().join("observability.sqlite3")).unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM metadata WHERE key='format'", [], |r| r
            .get::<_, u64>(
            0
        ))
        .unwrap(),
        999
    );
}

#[tokio::test]
async fn clock_rollback_keeps_new_log_and_minute_points_with_monotonic_retention() {
    let directory = private_dir();
    let runtime = RuntimeServices::open(directory.path(), settings(), 1).unwrap();
    runtime.storage.record_log(0, entry("fresh"));
    runtime.flush().await.unwrap();
    runtime.shutdown().await.unwrap();
    let lower = now_ms() + 172_800_000;
    let conn = rusqlite::Connection::open(directory.path().join("observability.sqlite3")).unwrap();
    conn.execute(
        "UPDATE metadata SET value=? WHERE key='clock_lower_ms'",
        [lower],
    )
    .unwrap();
    drop(conn);
    let runtime = RuntimeServices::open(directory.path(), settings(), 2).unwrap();
    assert!(runtime.status().clock_rollback);
    assert_eq!(
        runtime
            .list_logs(ListOptions::default())
            .await
            .unwrap()
            .total,
        0
    );
    runtime.storage.record_log(0, entry("upstream"));
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    let page = runtime.list_logs(ListOptions::default()).await.unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].cache, "upstream");
    assert!(page.entries[0].time_ms < lower);
    assert!(
        runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .samples
            .iter()
            .any(|p| p.requests == 1)
    );
    inspect(&runtime.storage, move |conn| {
        let retained: u64 =
            conn.query_row("SELECT retention_ms FROM query_log", [], |r| r.get(0))?;
        assert!(retained >= lower - 1000);
        Ok(())
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_statistics_payload_returns_storage_error_without_killing_worker() {
    let runtime = RuntimeServices::ephemeral(settings());
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    inspect(&runtime.storage, |conn| {
        let bytes: Vec<u8> =
            conn.query_row("SELECT payload FROM statistics_points LIMIT 1", [], |r| {
                r.get(0)
            })?;
        let mut point: serde_json::Value = serde_json::from_slice(&bytes)?;
        point["metrics"]["counters"]
            .as_object_mut()
            .unwrap()
            .remove("requests");
        conn.execute(
            "UPDATE statistics_points SET payload=?",
            [serde_json::to_vec(&point)?],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(matches!(
        runtime.statistics(HistoryOptions::default()).await,
        Err(Error::StorageUnavailable(_))
    ));
    assert!(runtime.list_logs(ListOptions::default()).await.is_ok());
    runtime.clear_history(0).await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn rollback_past_reset_period_resets_once_and_preserves_new_counters_after_reopen() {
    let directory = private_dir();
    let mut settings = settings();
    settings.statistics.reset_interval_days = 1;
    let runtime = RuntimeServices::open(directory.path(), settings.clone(), 1).unwrap();
    runtime.metrics.inc(Counter::Requests);
    runtime.flush().await.unwrap();
    runtime.shutdown().await.unwrap();
    let lower = now_ms() + 172_800_000;
    let conn = rusqlite::Connection::open(directory.path().join("observability.sqlite3")).unwrap();
    conn.execute(
        "UPDATE metadata SET value=? WHERE key='clock_lower_ms'",
        [lower],
    )
    .unwrap();
    drop(conn);
    let runtime = RuntimeServices::open(directory.path(), settings.clone(), 2).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.status().totals_epoch == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.metrics.inc(Counter::Requests);
    for _ in 0..8 {
        // Each request yields a worker turn where the automatic scheduler runs.
        runtime.flush().await.unwrap();
        let totals = runtime
            .statistics(HistoryOptions::default())
            .await
            .unwrap()
            .totals;
        assert_eq!(totals.epoch, 1);
        assert_eq!(totals.metrics.counters["requests"], 1);
        assert!(totals.since_ms < lower, "display remains wall clock");
    }
    runtime.shutdown().await.unwrap();
    let reopened = RuntimeServices::open(directory.path(), settings, 3).unwrap();
    reopened.metrics.inc(Counter::Requests);
    reopened.flush().await.unwrap();
    let totals = reopened
        .statistics(HistoryOptions::default())
        .await
        .unwrap()
        .totals;
    assert_eq!(totals.epoch, 1, "reset cut survives process restart");
    assert_eq!(totals.metrics.counters["requests"], 2);
    reopened.shutdown().await.unwrap();
}
