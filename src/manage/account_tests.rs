use super::*;
use crate::manage::{Active, ApiRequest, Manager, Store, api, auth_budget};
use axum::http::{HeaderValue, header};
use std::{sync::Mutex, task::Poll};
use tokio::{sync::Semaphore, task::JoinSet};

const PASSWORD: &str = "local-original-password";
const REPLACEMENT: &str = "local-replacement-password";

async fn fixture() -> (tempfile::TempDir, Arc<Shared>, HeaderMap) {
    let temp = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:3000".parse().unwrap();
    let store = Store::open(&temp.path().join("state")).unwrap();
    store.save(&Stored {
        username: "admin".into(),
        password_hash: store::hash_password(PASSWORD).unwrap(),
        toml: "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=100\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into(),
        previous: Some("listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=101\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into()),
        revision: 2,
    }).unwrap();
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: Vec::new(),
    }));
    let manager = Arc::new(tokio::sync::Mutex::new(
        Manager::open(store, address, active.clone()).await.unwrap(),
    ));
    let shared = Arc::new(Shared {
        filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
        manager,
        active,
        auth: auth_budget::Budget::new(),
        mutation: Arc::new(Semaphore::new(1)),
        address,
    });
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let session = shared.session(&transport).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3000"));
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("parins_session_http={}", session.token)).unwrap(),
    );
    headers.insert(
        "x-parins-session",
        HeaderValue::from_str(&session.binding).unwrap(),
    );
    (temp, shared, headers)
}

async fn request(
    shared: &Arc<Shared>,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let mut cookie = None;
    let bytes = serde_json::to_vec(&body).unwrap();
    api(
        shared.clone(),
        transport,
        ApiRequest {
            peer: shared.address,
            method,
            path,
            headers,
            body: &bytes,
        },
        &mut cookie,
    )
    .await
}

fn rotation() -> Value {
    json!({"current_password":PASSWORD,"username":"replacement-admin","new_password":REPLACEMENT})
}

#[tokio::test]
async fn credential_rotation_rejects_invalid_input_and_preserves_failed_save_and_sessions() {
    let (_temp, shared, headers) = fixture().await;
    let state_path = shared.manager.lock().await.store.dir.join("state.json");
    let before = std::fs::read(&state_path).unwrap();
    for input in [
        json!({"current_password":PASSWORD,"username":"invalid name","new_password":REPLACEMENT}),
        json!({"current_password":PASSWORD,"username":"admin","new_password":"short"}),
        json!({"current_password":PASSWORD,"username":"admin","new_password":"a".repeat(257)}),
        json!({"current_password":"a".repeat(257),"username":"admin","new_password":REPLACEMENT}),
    ] {
        assert_eq!(
            request(&shared, &headers, "POST", "/api/account/credentials", input)
                .await
                .unwrap_err()
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let mut extra = rotation();
    extra["confirm_password"] = json!(REPLACEMENT);
    assert_eq!(
        request(&shared, &headers, "POST", "/api/account/credentials", extra)
            .await
            .unwrap_err()
            .1,
        "BAD_JSON"
    );
    let mut wrong = rotation();
    wrong["current_password"] = json!("wrong");
    let error = request(&shared, &headers, "POST", "/api/account/credentials", wrong)
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::FORBIDDEN);
    assert_eq!(error.1, "CURRENT_PASSWORD_INCORRECT");
    // A directory at Store's destination reliably fails before rename, also
    // when tests run with elevated privileges; retain the original private file.
    let original = state_path.with_extension("original");
    std::fs::rename(&state_path, &original).unwrap();
    std::fs::create_dir(&state_path).unwrap();
    assert_eq!(
        request(
            &shared,
            &headers,
            "POST",
            "/api/account/credentials",
            rotation()
        )
        .await
        .unwrap_err()
        .1,
        "INTERNAL"
    );
    assert_eq!(shared.active.lock().unwrap().sessions.len(), 1);
    assert_eq!(
        shared.manager.lock().await.saved.as_ref().unwrap().username,
        "admin"
    );
    std::fs::remove_dir(&state_path).unwrap();
    std::fs::rename(original, &state_path).unwrap();
    assert_eq!(std::fs::read(&state_path).unwrap(), before);
    assert_eq!(
        request(&shared, &headers, "GET", "/api/account", json!(null))
            .await
            .unwrap(),
        json!({"username":"admin"})
    );
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_preserves_runtime_cache_and_configuration_envelope() {
    use crate::{ecs::Scope, protocol};
    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::{Name, RData, Record, RecordType, rdata::A},
    };
    let (_temp, shared, headers) = fixture().await;
    let manager = shared.manager.lock().await;
    let before = manager.saved.clone().unwrap();
    let resolver = manager.resolver().unwrap().clone();
    let cache = resolver.cache();
    let services = manager.services.clone();
    let filters = manager.filters.clone();
    let updates = manager.updates.clone();
    let generation = manager.status()["generation"].clone();
    let transport = shared.active.lock().unwrap().snapshot.clone();
    shared.session(&transport).unwrap();
    let mut query = Message::new(1, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("retained.test").unwrap(),
        RecordType::A,
    ));
    let mut answer = protocol::error_response(&query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        Name::from_ascii("retained.test").unwrap(),
        300,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    cache.insert(&query, &answer, Scope::NoEcs, std::time::Instant::now());
    assert_eq!(cache.snapshot()["entries"], 1);
    let cache_epoch = cache.epoch();
    let filter_state = serde_json::to_value(filters.snapshot()).unwrap();
    let update_state = updates.view();
    let storage_status = serde_json::to_value(services.status()).unwrap();
    drop(manager);
    assert_eq!(
        request(
            &shared,
            &headers,
            "POST",
            "/api/account/credentials",
            rotation()
        )
        .await
        .unwrap(),
        json!({"reauthentication_required":true})
    );
    assert!(shared.active.lock().unwrap().sessions.is_empty());
    let manager = shared.manager.lock().await;
    let next = manager.saved.as_ref().unwrap();
    assert_eq!(next.username, "replacement-admin");
    assert!(store::verify_password(&next.password_hash, REPLACEMENT));
    assert_ne!(next.password_hash, before.password_hash);
    assert_eq!(
        (next.revision, &next.toml, &next.previous),
        (before.revision, &before.toml, &before.previous)
    );
    let persisted = manager.store.read().unwrap().unwrap();
    assert_eq!(persisted.password_hash, next.password_hash);
    assert_eq!(manager.status()["generation"], generation);
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&cache, &manager.resolver().unwrap().cache()));
    assert!(Arc::ptr_eq(&services, &manager.services));
    assert!(Arc::ptr_eq(&filters, &manager.filters));
    assert!(Arc::ptr_eq(&updates, &manager.updates));
    assert!(Arc::ptr_eq(
        &transport,
        &shared.active.lock().unwrap().snapshot
    ));
    assert_eq!(cache.epoch(), cache_epoch);
    assert_eq!(cache.snapshot()["entries"], 1);
    assert_eq!(
        serde_json::to_value(filters.snapshot()).unwrap(),
        filter_state
    );
    assert_eq!(updates.view(), update_state);
    let after = serde_json::to_value(services.status()).unwrap();
    for key in [
        "configured_revision",
        "applied_revision",
        "log_epoch",
        "totals_epoch",
        "history_epoch",
    ] {
        assert!(
            storage_status.get(key).is_some(),
            "unknown storage field {key}"
        );
        assert_eq!(after[key], storage_status[key]);
    }
    drop(manager);
    assert_eq!(
        request(&shared, &headers, "GET", "/api/account", json!(null))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_rejects_late_old_password_login_without_issuing_session() {
    let (_temp, shared, headers) = fixture().await;
    let (verified, resume) = shared.auth.pause_next_password_completion();
    let control = shared.clone();
    let login = tokio::spawn(async move {
        request(
            &control,
            &HeaderMap::new(),
            "POST",
            "/api/login",
            json!({"username":"admin","password":PASSWORD}),
        )
        .await
    });
    verified.await.unwrap();
    request(
        &shared,
        &headers,
        "POST",
        "/api/account/credentials",
        rotation(),
    )
    .await
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(login.await.unwrap().unwrap_err().1, "LOGIN_FAILED");
    assert!(shared.active.lock().unwrap().sessions.is_empty());
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_preserves_configuration_completed_during_password_work() {
    let (_temp, shared, headers) = fixture().await;
    let (verified, resume) = shared.auth.pause_next_password_completion();
    let control = shared.clone();
    let caller_headers = headers.clone();
    let rotation = tokio::spawn(async move {
        request(
            &control,
            &caller_headers,
            "POST",
            "/api/account/credentials",
            rotation(),
        )
        .await
    });
    verified.await.unwrap();
    let source = shared
        .manager
        .lock()
        .await
        .saved
        .as_ref()
        .unwrap()
        .toml
        .clone();
    let next_source = format!("{source}\n[cache]\nmax_ttl_secs=120\n");
    request(
        &shared,
        &headers,
        "PUT",
        "/api/config",
        json!({"revision":2,"toml":next_source}),
    )
    .await
    .unwrap();
    resume.send(()).unwrap();
    rotation.await.unwrap().unwrap();
    let mut manager = shared.manager.lock().await;
    let saved = manager.saved.as_ref().unwrap();
    assert_eq!(saved.revision, 3);
    assert_eq!(saved.toml, next_source);
    assert_eq!(saved.previous.as_deref(), Some(source.as_str()));
    manager.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_rechecks_session_after_hash_and_obeys_freeze() {
    let (_temp, shared, headers) = fixture().await;
    let (verified, resume) = shared.auth.pause_next_password_completion();
    let control = shared.clone();
    let caller_headers = headers.clone();
    let caller = tokio::spawn(async move {
        request(
            &control,
            &caller_headers,
            "POST",
            "/api/account/credentials",
            rotation(),
        )
        .await
    });
    verified.await.unwrap();
    request(
        &shared,
        &headers,
        "POST",
        "/api/account/credentials",
        rotation(),
    )
    .await
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(
        caller.await.unwrap().unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let session = shared.session(&transport).unwrap();
    let mut headers = headers;
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("parins_session_http={}", session.token)).unwrap(),
    );
    headers.insert(
        "x-parins-session",
        HeaderValue::from_str(&session.binding).unwrap(),
    );
    shared
        .manager
        .lock()
        .await
        .updates
        .frozen
        .store(true, std::sync::atomic::Ordering::Release);
    let input = json!({"current_password":REPLACEMENT,"username":"admin","new_password":PASSWORD});
    assert_eq!(
        request(&shared, &headers, "POST", "/api/account/credentials", input)
            .await
            .unwrap_err()
            .1,
        "update_in_progress"
    );
    assert_eq!(shared.active.lock().unwrap().sessions.len(), 1);
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_protected_admission_rechecks_session_and_transport_after_wait() {
    for change_realm in [false, true] {
        let (_temp, shared, headers) = fixture().await;
        let transport = shared.active.lock().unwrap().snapshot.clone();
        shared.authorized(&headers, &transport).unwrap();
        let guard = shared.manager.lock().await;
        let admission = shared.protected_mutation_permit(&headers, &transport);
        tokio::pin!(admission);
        assert!(matches!(
            futures_util::poll!(admission.as_mut()),
            Poll::Pending
        ));
        if change_realm {
            let mut next = Snapshot::initial();
            next.realm = transport.realm + 1;
            shared.active.lock().unwrap().snapshot = Arc::new(next);
        } else {
            shared.active.lock().unwrap().sessions.clear();
        }
        drop(guard);
        assert_eq!(
            admission.await.unwrap_err().1,
            if change_realm {
                "TRANSPORT_CHANGED"
            } else {
                "UNAUTHORIZED"
            }
        );
        assert_eq!(shared.mutation.available_permits(), 1);
        shared.manager.lock().await.terminal_shutdown().await;
    }
}

#[tokio::test]
async fn credential_rotation_rejects_old_session_paused_before_mutation_admission() {
    let (_temp, shared, headers) = fixture().await;
    let (checked, wait) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    let control = shared.clone();
    let caller_headers = headers.clone();
    let caller = tokio::spawn(async move {
        let transport = control.active.lock().unwrap().snapshot.clone();
        control.authorized(&caller_headers, &transport).unwrap();
        checked.send(()).unwrap();
        resumed.await.unwrap();
        control
            .protected_mutation_permit(&caller_headers, &transport)
            .await
    });
    wait.await.unwrap();
    request(
        &shared,
        &headers,
        "POST",
        "/api/account/credentials",
        rotation(),
    )
    .await
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(
        caller.await.unwrap().unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_same_values_still_replace_hash_and_revoke_sessions() {
    let (_temp, shared, headers) = fixture().await;
    let old_hash = shared
        .manager
        .lock()
        .await
        .saved
        .as_ref()
        .unwrap()
        .password_hash
        .clone();
    request(
        &shared,
        &headers,
        "POST",
        "/api/account/credentials",
        json!({
            "current_password":PASSWORD,"username":"admin","new_password":PASSWORD
        }),
    )
    .await
    .unwrap();
    let mut manager = shared.manager.lock().await;
    let saved = manager.saved.as_ref().unwrap();
    assert_ne!(saved.password_hash, old_hash);
    assert!(store::verify_password(&saved.password_hash, PASSWORD));
    assert!(shared.active.lock().unwrap().sessions.is_empty());
    manager.terminal_shutdown().await;
}

#[tokio::test]
async fn credential_rotation_keeps_admitted_filter_work_and_manual_update_request() {
    use crate::filter_subscriptions::service::WorkRequest;
    let (_temp, shared, headers) = fixture().await;
    let filters = shared.manager.lock().await.filters.clone();
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let work = {
        let _permit = shared
            .protected_mutation_permit(&headers, &transport)
            .await
            .unwrap();
        filters
            .begin(WorkRequest::Refresh {
                config_revision: 2,
                source_id: None,
                automatic: true,
            })
            .await
            .unwrap()
            .work
            .unwrap()
    };
    let candidate = filters.execute(work).await.unwrap();
    request(&shared, &headers, "POST", "/api/updates/check", json!({}))
        .await
        .unwrap();
    let before = shared.manager.lock().await.updates.view();
    request(
        &shared,
        &headers,
        "POST",
        "/api/account/credentials",
        rotation(),
    )
    .await
    .unwrap();
    {
        let _permit = shared.mutation.acquire().await.unwrap();
        let manager = shared.manager.lock().await;
        assert!(
            !manager
                .updates
                .frozen
                .load(std::sync::atomic::Ordering::Acquire)
        );
        filters.commit(candidate).await.unwrap();
        assert_eq!(manager.updates.view(), before);
    }
    assert!(filters.snapshot().operation.is_none());
    assert_eq!(filters.snapshot().config_revision, 2);
    shared.manager.lock().await.terminal_shutdown().await;
}
