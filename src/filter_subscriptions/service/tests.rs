use super::super::{
    download::{REPRESENTATION, Validators},
    settings::SourceSettings,
    store::PreparedMetadata,
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
    }
}
fn install(store: &mut Store, settings: &SourceSettings, text: &[u8]) {
    let mut stage = store.begin_staging(text.len() as u64, now()).unwrap();
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
            now() - 120,
        )
        .unwrap();
}
async fn fixture(sources: Vec<SourceSettings>) -> (tempfile::TempDir, Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources");
    let mut store = Store::open(&path, 256 * 1024 * 1024, now()).unwrap();
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
    service.set_revision(2);
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
async fn frozen_start_is_read_only_and_bootstrap_downloads_all_new_sources_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frozen");
    let frozen = Service::open_frozen(
        path.clone(),
        Policy::default(),
        Settings::default(),
        1,
        vec![],
    )
    .await
    .unwrap();
    assert!(frozen.ready());
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
    assert!(service.next_due(now() + 1).is_none());
    assert!(service.snapshot().sources[0].next_update.unwrap() >= now() + 299);
    assert_eq!(
        service.snapshot().sources[0].error.as_ref().unwrap().code,
        "subscription_memory_limit"
    );
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
    )
    .await
    .unwrap();
    let generation = service.handle.snapshot();
    let source = crate::policy::LocalPolicySource::Inline(
        toml::from_str("enabled=true\nblock_exact=['new.local']").unwrap(),
    );
    let mut settings = service.state.lock().unwrap().settings.clone();
    settings.max_memory_bytes = material::COORDINATOR_BYTES + generation.policy.owned_bytes();
    // The new source fits by itself; only coexistence with the live old policy
    // makes this budget insufficient.
    source
        .compile(
            crate::policy::canonical::Limits {
                max_rules: settings.max_rules,
                max_memory_bytes: settings.max_memory_bytes,
                retained_bytes: material::COORDINATOR_BYTES,
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
