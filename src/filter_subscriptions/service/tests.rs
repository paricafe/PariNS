use super::super::{
    download::{REPRESENTATION, Validators},
    settings::SourceSettings,
    store::{PreparedMetadata, SourceStatus as StoredStatus},
};
use super::*;
use crate::https_reader::WireResponse;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, time::Duration};

fn source(id: &str) -> SourceSettings {
    SourceSettings {
        id: id.into(),
        name: id.into(),
        url: format!("https://example.com/{id}"),
        format: Format::DomainList,
        enabled: true,
        auto_update: true,
        update_interval_hours: 24,
        block_response: None,
        block_ipv4: None,
        block_ipv6: None,
    }
}
fn install(store: &mut Store, settings: &SourceSettings, text: &[u8]) {
    let mut stage = store.begin_staging(text.len() as u64).unwrap();
    stage.file.write_all(text).unwrap();
    let sha256 = format!("{:x}", Sha256::digest(text));
    store
        .prepare(
            stage,
            PreparedMetadata {
                fingerprint: settings.identity().unwrap().fingerprint,
                sha256: sha256.clone(),
                bytes: text.len() as u64,
                rules: 1,
                validators: Validators {
                    final_url: settings.url.clone(),
                    representation: REPRESENTATION.into(),
                    content_sha256: sha256,
                    etag: Some("\"old\"".into()),
                    last_modified: None,
                },
            },
            unix_secs() - 120,
        )
        .unwrap();
}
async fn fixture(sources: Vec<SourceSettings>) -> (tempfile::TempDir, Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let mut store = Store::open(&path, 256 * 1024 * 1024).unwrap();
    for source in &sources {
        install(
            &mut store,
            source,
            format!(".{}.test\n", source.id).as_bytes(),
        );
    }
    drop(store);
    let service = Service::open(
        path,
        Policy::default(),
        Settings {
            sources,
            ..Default::default()
        },
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    (dir, service)
}
fn response(bytes: &[u8]) -> WireResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.len().to_string()).unwrap(),
    );
    headers.insert(header::ETAG, HeaderValue::from_static("\"new\""));
    WireResponse {
        status: StatusCode::OK,
        headers,
        body: Box::pin(futures_util::stream::iter(vec![Ok(
            Bytes::copy_from_slice(bytes),
        )])),
        _connection: None,
    }
}
fn respond(service: &Service, bodies: &[&[u8]]) {
    *service.responses.lock().unwrap() =
        Some(bodies.iter().map(|body| vec![response(body)]).collect());
}
fn refresh(service: &Service) -> WorkRequest {
    WorkRequest::Refresh {
        config_revision: service.config_revision(),
        source_id: None,
        automatic: true,
    }
}

#[tokio::test]
async fn refresh_reclaims_young_unreferenced_preparation_when_staging_needs_space() {
    let mut active = source("one");
    active.auto_update = false;
    let current_fingerprint = active.identity().unwrap().fingerprint;
    let previous = source("previous");
    let previous_fingerprint = previous.identity().unwrap().fingerprint;
    let (dir, service) = fixture(vec![active, previous]).await;
    let pinned = source("pinned");
    let pinned_fingerprint = pinned.identity().unwrap().fingerprint;
    let small = source("small");
    let small_fingerprint = small.identity().unwrap().fingerprint;
    let _pin = {
        let mut locked = service.store.lock().unwrap();
        let store = locked.as_mut().unwrap();
        install(store, &pinned, b"pinned.test\n");
        install(store, &small, b"small.test\n");
        store
            .pin(&store.record(&pinned_fingerprint).unwrap().sha256)
            .unwrap()
    };
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.max_disk_bytes = 32 * 1024 * 1024;
    settings.sources.pop();
    let candidate = service
        .prepare_config(settings, Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);

    let mut comment = vec![b'#'; 4095];
    comment.push(b'\n');
    let mut body = comment.repeat(4095);
    body.extend_from_slice(b"unused.test\n");
    let unused = source("unused");
    let fingerprint = unused.identity().unwrap().fingerprint;
    respond(&service, &[&body]);
    let begin = service
        .begin(WorkRequest::Prepare {
            config_revision: 2,
            source: DraftSource {
                id: unused.id.clone(),
                url: unused.url.clone(),
                format: unused.format,
            },
        })
        .await
        .unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    assert_eq!(
        service.snapshot().recent_operation.unwrap().status,
        "succeeded"
    );
    assert!(
        service
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .record(&fingerprint)
            .is_some()
    );
    assert!(
        service
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .record(&small_fingerprint)
            .is_some()
    );

    respond(&service, &[]);
    for request in [
        WorkRequest::Prepare {
            config_revision: 2,
            source: DraftSource {
                id: unused.id,
                url: unused.url,
                format: unused.format,
            },
        },
        refresh(&service),
    ] {
        let begin = service.begin(request).await.unwrap();
        let candidate = service.execute(begin.work.unwrap()).await.unwrap();
        service.commit(candidate).await.unwrap();
        assert_eq!(
            service.snapshot().recent_operation.unwrap().status,
            "succeeded"
        );
        assert!(
            service
                .store
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .record(&fingerprint)
                .is_some()
        );
    }

    respond(&service, &[b".new.test\n"]);
    let begin = service
        .begin(WorkRequest::Refresh {
            config_revision: 2,
            source_id: Some("one".into()),
            automatic: false,
        })
        .await
        .unwrap();
    {
        let locked = service.store.lock().unwrap();
        let store = locked.as_ref().unwrap();
        for fingerprint in [
            &current_fingerprint,
            &previous_fingerprint,
            &pinned_fingerprint,
        ] {
            assert!(store.verified_content(fingerprint).is_ok());
        }
        assert!(store.record(&small_fingerprint).is_none());
        assert!(store.record(&fingerprint).is_none());
    }
    let admitted_catalog = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        admitted_catalog
    );
    service.commit(candidate).await.unwrap();
    assert_eq!(
        service.snapshot().recent_operation.unwrap().status,
        "succeeded"
    );
    assert!(
        service
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .record(&fingerprint)
            .is_none()
    );
    assert_eq!(
        service
            .explain(&Name::from_ascii("new.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Blocked
    );
}

#[tokio::test]
async fn local_configuration_and_read_only_do_not_require_subscription_material() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::default();
    let local = Policy::default();
    let material = read_only(&dir.path().join("missing"), &settings, &local).unwrap();
    assert_eq!(material.policy.semantic_digest(), local.semantic_digest());
    assert!(!dir.path().join("missing").exists());
    let service = Service::open(
        dir.path().join("sources"),
        local.clone(),
        settings.clone(),
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    assert!(service.ready());
    let candidate = service.prepare_config(settings, local, 1).await.unwrap();
    service.validate_candidate(&candidate).unwrap();
    service.publish_config(candidate, 2);
    assert_eq!(service.snapshot().config_revision, 2);
    service.close();
    assert!(
        service
            .prepare_config(Settings::default(), Policy::default(), 2)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refresh_installs_before_commit_but_catalog_and_policy_stay_unchanged() {
    let (dir, service) = fixture(vec![source("one")]).await;
    respond(&service, &[b".new.test\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let before = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        before
    );
    assert_eq!(
        service
            .explain(&Name::from_ascii("new.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Unmatched
    );
    service.commit(candidate).await.unwrap();
    assert_ne!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        before
    );
    assert_eq!(
        service
            .explain(&Name::from_ascii("new.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Blocked
    );
    assert_eq!(
        service.snapshot().recent_operation.unwrap().status,
        "succeeded"
    );
}

#[tokio::test]
async fn identical_semantics_update_counts_without_publishing_another_generation() {
    let (_dir, service) = fixture(vec![source("one")]).await;
    let generation = service.snapshot().generation;
    respond(&service, &[b"# comment\n.one.test\n.one.test\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    let state = service.snapshot();
    assert_eq!(state.generation, generation);
    assert_eq!(state.input_rules, 2);
    assert_eq!(state.sources[0].input_rules, 2);
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources[0].name = "Metadata edit".into();
    let candidate = service
        .prepare_config(settings, Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    assert_eq!(service.snapshot().input_rules, 2);
    assert_eq!(service.snapshot().generation, generation);
}

#[tokio::test]
async fn revision_change_rejects_late_publication_and_duplicate_work_is_coalesced() {
    let (dir, service) = fixture(vec![source("one")]).await;
    respond(&service, &[b".new.test\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let same = service.begin(refresh(&service)).await.unwrap();
    assert_eq!(same.operation_id, begin.operation_id);
    assert!(same.work.is_none());
    let before = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.sync_updates_only(2);
    let error = service.commit(candidate).await.unwrap_err();
    assert_eq!(error_code(&error), "revision_conflict");
    service.fail(&begin.operation_id, &error);
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        before
    );
    assert_eq!(
        service.snapshot().recent_operation.unwrap().status,
        "failed"
    );
}

#[tokio::test]
async fn parse_failures_keep_old_content_and_report_failed_not_success() {
    let (_dir, service) = fixture(vec![source("one")]).await;
    respond(&service, &[b"good.test\n||unsupported.test^\n"]);
    let old = service.snapshot();
    let begin = service.begin(refresh(&service)).await.unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    let state = service.snapshot();
    assert_eq!(state.content_revision, old.content_revision);
    assert_eq!(state.generation, old.generation);
    assert_eq!(state.sources[0].failures, 1);
    assert_eq!(state.sources[0].error.as_ref().unwrap().line, Some(2));
    assert_eq!(state.recent_operation.unwrap().status, "failed");
}

#[tokio::test]
async fn partial_failure_publishes_one_complete_aggregate_and_reports_source_failure() {
    let (_dir, service) = fixture(vec![source("one"), source("two")]).await;
    respond(&service, &[b".new.test\n", b"||bad^\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    assert_eq!(
        service
            .explain(&Name::from_ascii("new.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Blocked
    );
    assert_eq!(
        service
            .explain(&Name::from_ascii("two.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Blocked
    );
    let state = service.snapshot();
    assert_eq!(state.sources[1].failures, 1);
    assert_eq!(state.recent_operation.unwrap().status, "failed");
}

#[tokio::test]
async fn metadata_validation_is_read_only_and_allowed_with_retired_requests() {
    let (dir, service) = fixture(vec![source("one")]).await;
    let old = service.handle.snapshot();
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources[0].id = "renamed".into();
    let candidate = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    assert!(service.handle.ensure_available().is_err());
    let before = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    settings.sources[0].name = "Readable name".into();
    let candidate = service
        .prepare_config(settings, Policy::default(), 2)
        .await
        .unwrap();
    let generation = service.snapshot().generation;
    service.publish_config(candidate, 3);
    assert_eq!(service.snapshot().generation, generation);
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        before
    );
    drop(old);
}

#[tokio::test]
async fn insufficient_local_budget_is_not_ready_and_disabled_sources_can_be_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::open(
        dir.path().join("tiny"),
        Policy::default(),
        Settings {
            max_memory_bytes: 1,
            ..Default::default()
        },
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    assert!(!service.ready());
    let mut disabled = source("one");
    disabled.enabled = false;
    let (_dir, service) = fixture(vec![disabled.clone()]).await;
    let begin = service
        .begin(WorkRequest::Prepare {
            config_revision: 1,
            source: DraftSource {
                id: disabled.id,
                url: disabled.url,
                format: disabled.format,
            },
        })
        .await
        .unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    assert!(!service.snapshot().sources[0].active);
}

#[tokio::test]
async fn close_before_scheduler_first_poll_and_during_download_is_prompt() {
    let (_dir, service) = fixture(vec![source("one")]).await;
    let mut reply = response(b"");
    reply.body = Box::pin(futures_util::stream::pending());
    *service.responses.lock().unwrap() = Some(std::collections::VecDeque::from([vec![reply]]));
    let begin = service.begin(refresh(&service)).await.unwrap();
    let owner = service.clone();
    let task = tokio::spawn(async move { owner.execute(begin.work.unwrap()).await });
    tokio::task::yield_now().await;
    service.close();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(1), service.clone().run_file_scheduler())
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_reclaims_young_removed_source_for_manual_only_download() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bootstrap");
    let old = source("old");
    let old_fingerprint = old.identity().unwrap().fingerprint;
    let mut settings = Settings {
        max_disk_bytes: 32 * 1024 * 1024,
        sources: vec![old],
        ..Default::default()
    };
    let service = Service::open(
        path.clone(),
        Policy::default(),
        settings.clone(),
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    let mut comment = vec![b'#'; 4095];
    comment.push(b'\n');
    let mut body = comment.repeat(4095);
    body.extend_from_slice(b".old.test\n");
    let old_object = path
        .join("objects")
        .join(format!("{:x}.txt", Sha256::digest(&body)));
    respond(&service, &[&body]);
    service.bootstrap_new().await.unwrap();
    assert!(service.ready());
    service.close();
    drop(service);

    let mut new = source("new");
    new.auto_update = false;
    settings.sources = vec![new];
    let service = Service::open(path, Policy::default(), settings, 1, vec![], false)
        .await
        .unwrap();
    assert!(!service.ready());
    assert!(old_object.exists());
    assert!(
        service
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .record(&old_fingerprint)
            .is_some()
    );
    respond(&service, &[b".new.test\n"]);
    service.bootstrap_new().await.unwrap();
    assert!(service.ready());
    assert!(!old_object.exists());
    assert!(
        service
            .store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .record(&old_fingerprint)
            .is_none()
    );
    assert_eq!(
        service
            .explain(&Name::from_ascii("new.test").unwrap())
            .explanation
            .decision,
        crate::policy::Decision::Blocked
    );
}

#[tokio::test]
async fn automatic_refresh_charges_only_eligible_sources_for_real_failures() {
    let mut manual = source("manual");
    manual.auto_update = false;
    let (_dir, service) = fixture(vec![source("automatic"), manual]).await;
    for (code, expected_failures) in [
        ("subscription_cancelled", 0),
        ("revision_conflict", 0),
        ("subscription_download", 1),
    ] {
        let begin = service.begin(refresh(&service)).await.unwrap();
        service.fail(&begin.operation_id, &Failure::new(code).into());
        let sources = service.snapshot().sources;
        assert_eq!(sources[0].failures, expected_failures);
        assert_eq!(sources[1].failures, 0);
        assert_eq!(sources[1].error, None);
    }
}

#[tokio::test]
async fn bootstrap_reports_the_missing_source_that_failed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let existing = source("existing");
    let mut store = Store::open(&path, Settings::default().max_disk_bytes).unwrap();
    install(&mut store, &existing, b".existing.test\n");
    drop(store);
    let service = Service::open(
        path,
        Policy::default(),
        Settings {
            sources: vec![existing, source("missing")],
            ..Default::default()
        },
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    respond(&service, &[b"bad name with spaces\n"]);
    assert!(service.bootstrap_new().await.is_err());
    let snapshot = service.snapshot();
    assert_eq!(snapshot.sources[0].failures, 0);
    assert_eq!(
        snapshot.recent_operation.unwrap().source_id.as_deref(),
        Some("missing")
    );
}

#[tokio::test]
async fn frozen_start_is_read_only_and_bootstrap_downloads_all_new_sources_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frozen");
    let frozen = Service::open(
        path.clone(),
        Policy::default(),
        Settings::default(),
        1,
        vec![],
        true,
    )
    .await
    .unwrap();
    assert!(frozen.ready());
    let error = frozen.begin(refresh(&frozen)).await.err().unwrap();
    assert_eq!(error_code(&error), "update_in_progress");
    assert!(!path.exists());
    let mut a = source("one");
    a.auto_update = false;
    let mut b = source("two");
    b.auto_update = false;
    let service = Service::open(
        dir.path().join("bootstrap"),
        Policy::default(),
        Settings {
            sources: vec![a, b],
            ..Default::default()
        },
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    assert!(!service.ready());
    respond(&service, &[b".one.test\n", b".two.test\n"]);
    service.bootstrap_new().await.unwrap();
    assert!(service.ready());
    assert_eq!(service.snapshot().sources.len(), 2);
}

#[tokio::test]
async fn frozen_start_opens_the_store_once_thawed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let one = source("one");
    let mut store = Store::open(&path, 256 * 1024 * 1024).unwrap();
    install(&mut store, &one, b".one.test\n");
    drop(store);
    let settings = Settings {
        sources: vec![one],
        ..Default::default()
    };
    let service = Service::open(
        path.clone(),
        Policy::default(),
        settings.clone(),
        1,
        vec![],
        true,
    )
    .await
    .unwrap();
    assert!(service.ready());
    assert!(service.next_due(u64::MAX).is_none());
    assert_eq!(
        service.snapshot().unavailable_reason.as_deref(),
        Some("update_in_progress")
    );
    let error = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await
        .err()
        .unwrap();
    assert_eq!(error_code(&error), "subscription_storage_unavailable");
    drop(Store::open(&path, 256 * 1024 * 1024).unwrap());

    service.thaw().await;
    assert!(
        Store::open(&path, 256 * 1024 * 1024).is_err(),
        "the Store holds its lock"
    );
    assert_eq!(service.snapshot().unavailable_reason, None);
    assert!(
        service.next_due(u64::MAX).is_some(),
        "the refresh schedule is seeded"
    );
    let candidate = service
        .prepare_config(settings, Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    respond(&service, &[b".one.test\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    service.thaw().await;
    assert!(service.ready());
}

#[tokio::test]
async fn metadata_schedule_changes_and_aggregate_failure_have_bounded_next_due() {
    let mut initial = source("one");
    initial.update_interval_hours = 168;
    let (_dir, service) = fixture(vec![initial]).await;
    let old_due = service.snapshot().sources[0].next_update.unwrap();
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources[0].update_interval_hours = 1;
    let generation = service.snapshot().generation;
    let candidate = service
        .prepare_config(settings, Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    assert!(service.snapshot().sources[0].next_update.unwrap() < old_due);
    assert_eq!(service.snapshot().generation, generation);
    let begin = service.begin(refresh(&service)).await.unwrap();
    drop(begin.work);
    service.fail(
        &begin.operation_id,
        &Failure::new("subscription_memory_limit").into(),
    );
    assert!(service.next_due(unix_secs() + 1).is_none());
    assert!(service.snapshot().sources[0].next_update.unwrap() >= unix_secs() + 299);
    assert_eq!(
        service.snapshot().sources[0].error.as_ref().unwrap().code,
        "subscription_memory_limit"
    );
}

#[tokio::test]
async fn remote_retry_after_is_bounded_and_stays_bounded_after_reopen() {
    for retry_after in ["18446744073709551615", "Fri, 31 Dec 9999 23:59:59 GMT"] {
        let mut one = source("one");
        one.update_interval_hours = 1;
        let (dir, service) = fixture(vec![one]).await;
        let settings = service.state.lock().unwrap().settings.clone();
        let original = service.snapshot();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(retry_after).unwrap(),
        );
        *service.responses.lock().unwrap() = Some(
            vec![vec![WireResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                headers,
                body: Box::pin(futures_util::stream::empty()),
                _connection: None,
            }]]
            .into(),
        );
        let before = unix_secs();
        let begin = service.begin(refresh(&service)).await.unwrap();
        let candidate = service.execute(begin.work.unwrap()).await.unwrap();
        service.commit(candidate).await.unwrap();
        let state = service.snapshot();
        let due = state.sources[0].next_update.unwrap();
        assert!((before + 3600..=unix_secs() + 3600).contains(&due));
        assert_eq!(state.sources[0].failures, 1);
        assert_eq!(state.content_revision, original.content_revision);
        assert_eq!(state.generation, original.generation);
        assert!(service.next_due(due - 1).is_none());
        assert!(service.next_due(due).is_some());
        service.close();
        drop(service);
        let service = Service::open(
            dir.path().join("sources"),
            Policy::default(),
            settings,
            1,
            vec![],
            false,
        )
        .await
        .unwrap();
        assert_eq!(service.snapshot().sources[0].next_update, Some(due));
        assert_eq!(service.snapshot().sources[0].failures, 1);
        assert!(service.next_due(due).is_some());
    }
}

#[tokio::test]
async fn failed_schedule_is_recapped_when_the_interval_changes_or_the_store_reopens() {
    let mut one = source("one");
    one.update_interval_hours = 168;
    let (dir, service) = fixture(vec![one]).await;
    let attempted = unix_secs();
    {
        let mut locked = service.store.lock().unwrap();
        let store = locked.as_mut().unwrap();
        let record = store.records()[0].clone();
        store
            .update_status(
                &record.fingerprint,
                StoredStatus {
                    failures: 1,
                    last_attempt: Some(attempted),
                    next_update: Some(u64::MAX),
                    last_error: Some("subscription_download".into()),
                    error_line: None,
                },
            )
            .unwrap();
        service.observe_store(store);
    }
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources[0].update_interval_hours = 1;
    let candidate = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    let due = attempted + 3600;
    assert_eq!(service.snapshot().sources[0].next_update, Some(due));
    service.close();
    drop(service);
    let service = Service::open(
        dir.path().join("sources"),
        Policy::default(),
        settings,
        2,
        vec![],
        false,
    )
    .await
    .unwrap();
    assert_eq!(service.snapshot().sources[0].next_update, Some(due));
    assert!(service.next_due(due).is_some());
}

#[tokio::test]
async fn read_only_configuration_candidate_does_not_create_derived_or_mutate_catalog() {
    let (dir, service) = fixture(vec![source("one")]).await;
    let before = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    let before_indexes = fs::read_dir(dir.path().join("sources/indexes"))
        .unwrap()
        .count();
    let local: Policy = toml::from_str("enabled=true\nblock_exact=['new.local']").unwrap();
    let settings = service.state.lock().unwrap().settings.clone();
    let candidate = service.prepare_config(settings, local, 1).await.unwrap();
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        before
    );
    assert_eq!(
        fs::read_dir(dir.path().join("sources/indexes"))
            .unwrap()
            .count(),
        before_indexes
    );
    drop(candidate);
}

#[tokio::test]
async fn descriptor_reserve_is_charged_before_aggregate_allocation() {
    let (_dir, service) = fixture(vec![source("one")]).await;
    let mut settings = service.state.lock().unwrap().settings.clone();
    let local: Policy = toml::from_str("enabled=true\nblock_exact=['another.local']").unwrap();
    settings.max_memory_bytes = service.live_bytes(&local) + material::COORDINATOR_BYTES - 1;
    let candidate = service.prepare_config(settings, local, 1).await;
    let error = match candidate {
        Ok(_) => panic!("reserve cannot be bypassed"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), "subscription_memory_limit");
}

#[tokio::test]
async fn unchanged_active_material_still_needs_the_coordinator_reservation() {
    let (_dir, service) = fixture(vec![source("one")]).await;
    let (mut settings, local) = {
        let state = service.state.lock().unwrap();
        (state.settings.clone(), state.local.clone())
    };
    settings.max_memory_bytes = service.live_bytes(&local) + material::COORDINATOR_BYTES - 1;
    let result = service.prepare_config(settings, local, 1).await;
    assert!(matches!(result, Err(error) if error_code(&error) == "subscription_memory_limit"));
    assert_eq!(service.config_revision(), 1);
}

#[tokio::test]
async fn local_edits_use_small_budgets_until_subscriptions_are_effective() {
    let (_dir, service) = fixture(vec![]).await;
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.max_memory_bytes = 1024 * 1024;
    let mut disabled = source("disabled");
    disabled.enabled = false;
    settings.sources.push(disabled);
    let local = || {
        crate::policy::LocalPolicySource::Inline(
            toml::from_str("enabled=true\nblock_exact=['small.local']").unwrap(),
        )
    };
    let candidate = service
        .prepare_config_from_source(settings.clone(), local(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    assert!(
        service
            .handle
            .snapshot()
            .policy
            .blocks(&Name::from_ascii("small.local").unwrap())
    );
    settings.sources[0].enabled = true;
    let result = service
        .prepare_config_from_source(settings, local(), 2)
        .await;
    assert!(matches!(result, Err(error) if error_code(&error) == "subscription_memory_limit"));
}

#[tokio::test]
async fn local_source_waits_for_worker_before_opening_file() {
    let (dir, service) = fixture(vec![]).await;
    let path = dir.path().join("missing-local.toml");
    let settings = service.state.lock().unwrap().settings.clone();
    let generation = service.snapshot().generation;
    let lease = service.acquire().unwrap();
    let candidate = service
        .prepare_config_from_source(
            settings.clone(),
            crate::policy::LocalPolicySource::File(path.clone()),
            1,
        )
        .await;
    let error = match candidate {
        Ok(_) => panic!("local source cannot bypass the worker slot"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), "busy");
    assert!(!path.exists());
    drop(lease);

    let candidate = service
        .prepare_config_from_source(settings, crate::policy::LocalPolicySource::File(path), 1)
        .await;
    let error = match candidate {
        Ok(_) => panic!("missing local source must fail after acquiring the worker"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("open filter file"));
    assert_eq!(service.snapshot().generation, generation);
    assert_eq!(service.config_revision(), 1);
    assert!(service.acquire().is_ok());
}

#[tokio::test]
async fn a_check_waits_for_the_worker_instead_of_answering_busy() {
    let (dir, service) = fixture(vec![]).await;
    let path = dir.path().join("local.toml");
    std::fs::write(&path, "enabled = true\nblock_exact = ['ads.test']\n").unwrap();
    let settings = service.state.lock().unwrap().settings.clone();
    let source = || crate::policy::LocalPolicySource::File(path.clone());
    let wait = std::time::Duration::from_secs(5);
    let lease = service.acquire().unwrap();
    let check = tokio::spawn({
        let (service, settings, source) = (service.clone(), settings.clone(), source());
        async move {
            service
                .check_config_from_source(settings, source, 1, wait)
                .await
                .map(drop)
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!check.is_finished(), "the check waits for the worker");
    drop(lease);
    check.await.unwrap().unwrap();

    let lease = service.acquire().unwrap();
    let error = service
        .check_config_from_source(settings, source(), 1, std::time::Duration::from_millis(50))
        .await
        .map(drop)
        .unwrap_err();
    assert_eq!(error_code(&error), "busy");
    drop(lease);
    assert!(service.acquire().is_ok());
}

#[tokio::test]
async fn local_source_budget_counts_old_policy_and_preserves_generation() {
    let dir = tempfile::tempdir().unwrap();
    let rules = (0..256)
        .map(|i| format!("'old-{i}.local'"))
        .collect::<Vec<_>>()
        .join(",");
    let local: Policy = toml::from_str(&format!("enabled=true\nblock_exact=[{rules}]")).unwrap();
    let service = Service::open(
        dir.path().join("sources"),
        local,
        Settings::default(),
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    let generation = service.handle.snapshot();
    let source = crate::policy::LocalPolicySource::Inline(
        toml::from_str("enabled=true\nblock_exact=['new.local']").unwrap(),
    );
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.max_memory_bytes = generation.policy.owned_bytes();
    source
        .compile(
            crate::policy::canonical::Limits {
                max_rules: settings.max_rules,
                max_memory_bytes: settings.max_memory_bytes,
                retained_bytes: 0,
            },
            || Ok(()),
        )
        .unwrap();
    let candidate = service
        .prepare_config_from_source(settings, source, 1)
        .await;
    let error = match candidate {
        Ok(_) => panic!("live old local policy must remain charged"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), "subscription_memory_limit");
    let current = service.handle.snapshot();
    assert_eq!(current.number, generation.number);
    assert!(current.policy.same_allocation(&generation.policy));
    assert_eq!(service.config_revision(), 1);
    assert!(service.ready());
    assert!(service.acquire().is_ok());
}

#[tokio::test]
async fn source_block_method_publishes_from_stored_content_without_downloading() {
    use crate::policy::{BlockMode, BlockTrace};
    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query},
        rr::{Name, RecordType},
    };
    let (dir, service) = fixture(vec![source("one")]).await;
    let catalog = fs::read(dir.path().join("sources/catalog.json")).unwrap();
    let answer = |service: &Service| {
        let mut query = Message::new(1, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(
            Name::from_ascii("x.one.test").unwrap(),
            RecordType::A,
        ));
        service
            .handle
            .snapshot()
            .policy
            .block_query(&query)
            .unwrap()
            .1
    };
    let trace = |response: &str| BlockTrace {
        rule: "one.test".into(),
        source_id: Some("one".into()),
        response: response.into(),
    };
    assert_eq!(answer(&service), trace("nodata"));
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources[0].block_response = Some(BlockMode::Refused);
    let generation = service.snapshot().generation;
    let candidate = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(candidate, 2);
    assert_eq!(answer(&service), trace("refused"));
    assert!(service.snapshot().generation > generation);
    settings.sources[0].block_response = None;
    let local: Policy = toml::from_str("block_response = 'refused'").unwrap();
    let generation = service.snapshot().generation;
    let candidate = service.prepare_config(settings, local, 2).await.unwrap();
    service.publish_config(candidate, 3);
    assert_eq!(answer(&service), trace("refused"));
    assert_eq!(service.snapshot().generation, generation);
    assert_eq!(
        fs::read(dir.path().join("sources/catalog.json")).unwrap(),
        catalog
    );
}

#[tokio::test]
async fn startup_uses_verified_memory_policy_when_index_does_not_fit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let mut store = Store::open(&path, 32 * 1024 * 1024).unwrap();
    let mut active = source("one");
    active.auto_update = false;
    install(&mut store, &active, b".one.test\n");
    store
        .set_references(vec![active.identity().unwrap().fingerprint], vec![])
        .unwrap();
    let padding = crate::private_files::create_private(&path.join("unmanaged")).unwrap();
    padding
        .set_len(256 * 1024 - store.disk_bytes().unwrap() - 1024)
        .unwrap();
    drop(store);
    let settings = Settings {
        sources: vec![active],
        max_disk_bytes: 256 * 1024,
        ..Default::default()
    };
    let service = Service::open(
        path.clone(),
        Policy::default(),
        settings.clone(),
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    assert!(service.ready());
    assert!(
        service
            .handle
            .snapshot()
            .policy
            .blocks(&Name::from_ascii("one.test").unwrap())
    );
    assert!(service.state.lock().unwrap().index_pin.is_none());
    assert_eq!(fs::read_dir(path.join("indexes")).unwrap().count(), 0);
    drop(service);

    let object = fs::read_dir(path.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(object, b"corrupt").unwrap();
    let service = Service::open(path, Policy::default(), settings, 1, vec![], false)
        .await
        .unwrap();
    assert!(!service.ready());
    assert_eq!(
        service.snapshot().unavailable_reason.as_deref(),
        Some("subscription_material_missing")
    );
}

#[tokio::test]
async fn refresh_survives_index_enospc_and_drops_the_obsolete_pin() {
    let (dir, service) = fixture(vec![source("one")]).await;
    let indexes = dir.path().join("sources/indexes");
    assert!(service.state.lock().unwrap().index_pin.is_some());
    service
        .store
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .simulate_index_disk_full();
    respond(&service, &[b".changed.test\n"]);
    let begin = service.begin(refresh(&service)).await.unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    let policy = service.handle.snapshot();
    assert!(
        policy
            .policy
            .blocks(&Name::from_ascii("changed.test").unwrap())
    );
    assert!(!policy.policy.blocks(&Name::from_ascii("one.test").unwrap()));
    assert!(service.state.lock().unwrap().index_pin.is_none());
    service
        .store
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .collect(unix_secs(), false)
        .unwrap();
    assert_eq!(fs::read_dir(indexes).unwrap().count(), 0);
    assert_eq!(
        fs::read_dir(dir.path().join("sources/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn private_index_permission_errors_stay_hard() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, service) = fixture(vec![source("one")]).await;
    assert!(service.ready());
    let path = dir.path().join("sources");
    let settings = service.state.lock().unwrap().settings.clone();
    drop(service);
    let index = fs::read_dir(path.join("indexes"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::set_permissions(index, fs::Permissions::from_mode(0o644)).unwrap();
    let service = Service::open(path, Policy::default(), settings, 1, vec![], false)
        .await
        .unwrap();
    assert!(!service.ready());
    assert_eq!(
        service.snapshot().unavailable_reason.as_deref(),
        Some("subscription_storage_unavailable")
    );
}

#[tokio::test]
async fn deadline_during_download_backs_off_but_shutdown_does_not() {
    for closing in [false, true] {
        let (_dir, service) = fixture(vec![source("one")]).await;
        let (started, wait) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let mut reply = response(b".slow.test\n");
        reply.body = Box::pin(futures_util::stream::poll_fn(move |_| {
            if let Some(started) = started.take() {
                let _ = started.send(());
            }
            std::task::Poll::Pending
        }));
        *service.responses.lock().unwrap() = Some(std::collections::VecDeque::from([vec![reply]]));
        let begin = service.begin(refresh(&service)).await.unwrap();
        let before = service.snapshot().sources.remove(0);
        let work = begin.work.unwrap();
        let owner = service.clone();
        let task = tokio::spawn(async move { owner.execute(work).await });
        tokio::time::timeout(Duration::from_secs(2), wait)
            .await
            .unwrap()
            .unwrap();
        if closing {
            service.request_close();
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(301)).await;
        }
        let error = match task.await.unwrap() {
            Ok(_) => panic!("timed out or closed work is never committed"),
            Err(error) => error,
        };
        if !closing {
            tokio::time::resume();
        }
        service.fail(&begin.operation_id, &error);
        let mut snapshot = service.snapshot();
        let source = snapshot.sources.remove(0);
        if closing {
            assert_eq!(error_code(&error), "subscription_cancelled");
            assert_eq!(
                (source.failures, source.next_update),
                (before.failures, before.next_update)
            );
            assert_eq!(snapshot.recent_operation.unwrap().status, "cancelled");
        } else {
            assert_eq!(error_code(&error), "subscription_download");
            assert_eq!(source.failures, before.failures + 1);
            assert!(source.next_update.unwrap() > unix_secs());
            assert!(source.next_update.unwrap() > source.last_attempt.unwrap());
            assert_eq!(snapshot.recent_operation.unwrap().status, "failed");
        }
    }
}

#[tokio::test]
async fn prepare_repairs_missing_and_corrupt_objects_but_rejects_unsafe_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let mut store = Store::open(&path, 32 * 1024 * 1024).unwrap();
    let draft = source("one");
    let fp = draft.identity().unwrap().fingerprint;
    let content = b".one.test\n";
    install(&mut store, &draft, content);
    let hash = store.record(&fp).unwrap().sha256.clone();
    let object = path.join("objects").join(format!("{hash}.txt"));
    drop(store);
    let service = Service::open(
        path,
        Policy::default(),
        Settings::default(),
        1,
        vec![],
        false,
    )
    .await
    .unwrap();
    let request = || WorkRequest::Prepare {
        config_revision: 1,
        source: DraftSource {
            id: draft.id.clone(),
            url: draft.url.clone(),
            format: draft.format,
        },
    };
    for damage in [
        Some(b".bad.test\n".as_slice()),
        Some(b"short".as_slice()),
        None,
    ] {
        if let Some(damage) = damage {
            fs::write(&object, damage).unwrap();
        } else {
            fs::remove_file(&object).unwrap();
        }
        respond(&service, &[content]);
        let begin = service.begin(request()).await.unwrap();
        let candidate = service.execute(begin.work.unwrap()).await.unwrap();
        service.commit(candidate).await.unwrap();
        assert_eq!(
            service.snapshot().recent_operation.unwrap().status,
            "succeeded"
        );
        assert_eq!(fs::read(&object).unwrap(), content);
    }
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644)).unwrap();
    let result = service.begin(request()).await;
    assert!(
        matches!(result, Err(error) if error_code(&error) == "subscription_storage_unavailable")
    );

    let moved = dir.path().join("target.txt");
    fs::rename(&object, &moved).unwrap();
    symlink(&moved, &object).unwrap();
    let result = service.begin(request()).await;
    assert!(
        matches!(result, Err(error) if error_code(&error) == "subscription_storage_unavailable")
    );
    assert_eq!(fs::read(&moved).unwrap(), content);
}

#[tokio::test]
async fn config_quota_boundary_includes_new_catalog_references() {
    let (dir, service) = fixture(vec![source("one")]).await;
    let mut extra = source("two");
    extra.enabled = false;
    let quota_without_growth = {
        let mut locked = service.store.lock().unwrap();
        let store = locked.as_mut().unwrap();
        install(store, &extra, b".two.test\n");
        store.disk_bytes().unwrap() + 256 * 1024
    };
    let catalog = dir.path().join("sources/catalog.json");
    let old_bytes = fs::read(&catalog).unwrap();
    let mut target: serde_json::Value = serde_json::from_slice(&old_bytes).unwrap();
    target["previous"] = target["current"].clone();
    target["current"]
        .as_array_mut()
        .unwrap()
        .push(extra.identity().unwrap().fingerprint.into());
    let growth = (serde_json::to_vec(&target).unwrap().len() - old_bytes.len()) as u64;
    assert!(growth > 0);
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.sources.push(extra.clone());
    settings.max_disk_bytes = quota_without_growth;
    let result = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await;
    assert!(matches!(result, Err(error) if error_code(&error) == "subscription_disk_limit"));
    settings.max_disk_bytes = quota_without_growth + growth;
    let candidate = service
        .prepare_config(settings, Policy::default(), 1)
        .await
        .unwrap();
    assert_eq!(fs::read(&catalog).unwrap(), old_bytes);
    service.publish_config(candidate, 2);
    let orphan = vec![b'x'; 256 * 1024 + 4096];
    let orphan_path = dir
        .path()
        .join("sources/objects")
        .join(format!("{:x}.txt", Sha256::digest(&orphan)));
    crate::private_files::create_private(&orphan_path)
        .unwrap()
        .write_all(&orphan)
        .unwrap();
    let begin = service
        .begin(WorkRequest::Prepare {
            config_revision: 2,
            source: DraftSource {
                id: extra.id,
                url: extra.url,
                format: extra.format,
            },
        })
        .await
        .unwrap();
    let candidate = service.execute(begin.work.unwrap()).await.unwrap();
    service.commit(candidate).await.unwrap();
    assert!(!orphan_path.exists());
    assert_eq!(
        service.snapshot().recent_operation.unwrap().status,
        "succeeded"
    );
    let synced: serde_json::Value = serde_json::from_slice(&fs::read(catalog).unwrap()).unwrap();
    assert_eq!(synced["current"], target["current"]);
    assert_eq!(synced["previous"], target["previous"]);
}

#[tokio::test]
async fn local_material_replacement_releases_the_old_index_pin() {
    let (dir, service) = fixture(vec![source("one")]).await;
    let indexes = dir.path().join("sources/indexes");
    assert_eq!(fs::read_dir(&indexes).unwrap().count(), 1);
    let mut settings = service.state.lock().unwrap().settings.clone();
    let reused = service
        .prepare_config(settings.clone(), Policy::default(), 1)
        .await
        .unwrap();
    service.publish_config(reused, 2);
    assert!(service.state.lock().unwrap().index_pin.is_some());
    settings.enabled = false;
    let local = toml::from_str("enabled=true\nblock_exact=['local.test']").unwrap();
    let candidate = service.prepare_config(settings, local, 2).await.unwrap();
    service.publish_config(candidate, 3);
    assert!(service.state.lock().unwrap().index_pin.is_none());
    service
        .store
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .collect(unix_secs(), false)
        .unwrap();
    assert_eq!(fs::read_dir(indexes).unwrap().count(), 0);
}
