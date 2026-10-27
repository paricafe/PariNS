//! Management owns admission, mutation ordering and the lifetime of rule work.
//! Downloads and compilation never hold the Manager or mutation lock.
use super::{ApiError, Shared, Snapshot, decode, error};
use crate::filter_subscriptions::service::{DraftSource, WorkRequest};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

// Manual work acts on the saved configuration current at admission; the
// service still rejects a result whose configuration changed meanwhile.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    source: DraftSource,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    source_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    name: String,
}

pub(super) async fn api(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    headers: &HeaderMap,
    path: &str,
    body: &[u8],
) -> Result<Value, ApiError> {
    match path {
        "/api/filter/check" => {
            let input: Check = decode(body)?;
            let name = hickory_proto::rr::Name::from_ascii(&input.name).map_err(|_| {
                error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    "Invalid DNS name",
                )
            })?;
            Ok(json!(shared.manager.lock().await.filters.explain(&name)))
        }
        "/api/filter/subscriptions/prepare" => {
            let input: Prepare = decode(body)?;
            start(
                shared,
                move |config_revision| WorkRequest::Prepare {
                    config_revision,
                    source: input.source,
                },
                Some((headers, &transport)),
            )
            .await
        }
        "/api/filter/subscriptions/refresh" => {
            let input: Refresh = decode(body)?;
            start(
                shared,
                move |config_revision| WorkRequest::Refresh {
                    config_revision,
                    source_id: input.source_id,
                    automatic: false,
                },
                Some((headers, &transport)),
            )
            .await
        }
        _ => unreachable!("filtered route"),
    }
}

/// `request` receives the saved configuration revision read under the
/// mutation permit, which excludes configuration changes until admission.
async fn start(
    shared: Arc<Shared>,
    request: impl FnOnce(u64) -> WorkRequest + Send + 'static,
    authorization: Option<(&HeaderMap, &Snapshot)>,
) -> Result<Value, ApiError> {
    let permit = match authorization {
        Some((headers, transport)) => shared.protected_mutation_permit(headers, transport).await?,
        None => shared.mutation_permit().await?,
    };
    // Admission may fsync catalog maintenance. Its permit and registration must
    // survive the initiating HTTP connection, just like configuration apply.
    tokio::spawn(async move {
        let _permit = permit;
        let filters = shared.manager.lock().await.filters.clone();
        let request = request(filters.config_revision());
        let mut tasks = shared.filter_tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        let begin = filters.begin(request).await.map_err(api_error)?;
        let id = begin.operation_id.clone();
        if let Some(work) = begin.work {
            let control = shared.clone();
            tasks.spawn(async move {
                let result = async {
                    let candidate = filters.execute(work).await?;
                    let _permit = control.mutation.acquire().await?;
                    // A software update commits its freeze under this permit.
                    // Manager only reaches the flag; the commit (fsync and
                    // rename) runs without its lock.
                    let frozen = control
                        .manager
                        .lock()
                        .await
                        .updates
                        .frozen
                        .load(Ordering::Acquire);
                    anyhow::ensure!(
                        !frozen,
                        crate::filter_subscriptions::service::Failure {
                            code: "subscription_cancelled".into(),
                            line: None
                        }
                    );
                    filters.commit(candidate).await
                }
                .await;
                if let Err(error) = result {
                    filters.fail(&begin.operation_id, &error);
                }
            });
        }
        Ok(json!({"operation_id":id}))
    })
    .await
    .map_err(|_| super::internal())?
}

pub(super) fn api_error(err: anyhow::Error) -> ApiError {
    let code = crate::filter_subscriptions::service::error_code(&err);
    let status = match code {
        "busy" | "revision_conflict" | "subscription_active_source" => StatusCode::CONFLICT,
        "subscription_rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        "subscription_storage_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    error(status, code, code)
}

pub(super) async fn run(shared: Arc<Shared>, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        let request = {
            let manager = shared.manager.lock().await;
            if manager.saved.is_some() && !manager.updates.frozen.load(Ordering::Acquire) {
                manager.filters.next_due(crate::clock::unix_secs())
            } else {
                None
            }
        };
        if let Some(request) = request {
            // A configuration saved since `next_due` makes this a revision
            // conflict; the next tick schedules against the new one.
            let _ = start(shared.clone(), move |_| request, None).await;
        }
        tokio::select! {
            _ = stop.changed() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::{Active, ApiRequest, Snapshot, Store, api};
    use axum::http::{HeaderMap, HeaderValue, header};
    use std::sync::Mutex;
    use tokio::{sync::Semaphore, task::JoinSet};

    #[tokio::test]
    async fn read_only_routes_require_session_and_mutations_obey_freeze() {
        let temp = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(Mutex::new(Active {
            snapshot: Arc::new(Snapshot::initial()),
            sessions: vec![],
        }));
        let manager = Arc::new(tokio::sync::Mutex::new(
            crate::manage::runtime::Manager::open(
                Store::open(&temp.path().join("state")).unwrap(),
                address,
                active.clone(),
            )
            .await
            .unwrap(),
        ));
        let shared = Arc::new(Shared {
            manager,
            active: active.clone(),
            auth: crate::manage::auth_budget::Budget::new(),
            mutation: Arc::new(Semaphore::new(1)),
            address,
            filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
            live: Default::default(),
        });
        let transport = active.lock().unwrap().snapshot.clone();
        let mut headers = HeaderMap::new();
        let mut cookie = None;
        let denied = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "GET",
                path: "/api/filter/subscriptions",
                headers: &headers,
                body: &[],
            },
            &mut cookie,
        )
        .await
        .unwrap_err();
        assert_eq!(denied.1, "UNAUTHORIZED");
        let session = shared.session(&transport).unwrap();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("parins_session_http={}", session.token)).unwrap(),
        );
        headers.insert(
            "x-parins-session",
            HeaderValue::from_str(&session.binding).unwrap(),
        );
        let state = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "GET",
                path: "/api/filter/subscriptions",
                headers: &headers,
                body: &[],
            },
            &mut cookie,
        )
        .await
        .unwrap();
        assert_eq!(state["config_revision"], 0);
        assert_eq!(state["sources"], json!([]));
        let checked = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "POST",
                path: "/api/filter/check",
                headers: &headers,
                body: br#"{"name":"example.org"}"#,
            },
            &mut cookie,
        )
        .await
        .unwrap();
        assert_eq!(checked["decision"], "unmatched");
        // Manual work acts on the saved configuration; a client revision is rejected.
        let input = br#"{"config_revision":0,"source":{"id":"example","url":"https://example.org/list","format":"domain_list"}}"#;
        let obsolete = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "POST",
                path: "/api/filter/subscriptions/prepare",
                headers: &headers,
                body: input,
            },
            &mut cookie,
        )
        .await
        .unwrap_err();
        assert_eq!(obsolete.1, "BAD_JSON");
        let catalog = temp.path().join("state/filter-subscriptions/catalog.json");
        let before = std::fs::read(&catalog).unwrap();
        shared
            .manager
            .lock()
            .await
            .updates
            .frozen
            .store(true, Ordering::Release);
        let frozen = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "POST",
                path: "/api/filter/subscriptions/prepare",
                headers: &headers,
                body: br#"{"source":{"id":"example","url":"https://example.org/list","format":"domain_list"}}"#,
            },
            &mut cookie,
        )
        .await
        .unwrap_err();
        assert_eq!(frozen.1, "update_in_progress");
        assert_eq!(std::fs::read(&catalog).unwrap(), before);
        assert!(
            shared
                .manager
                .lock()
                .await
                .filters
                .snapshot()
                .operation
                .is_none()
        );
        assert!(shared.filter_tasks.lock().await.is_empty());
        assert!(cookie.is_none());
        shared
            .manager
            .lock()
            .await
            .updates
            .frozen
            .store(false, Ordering::Release);
        // Hold the registration owner, disconnect the caller after admission,
        // and prove the detached transaction still owns the mutation permit.
        let registration = shared.filter_tasks.lock().await;
        let caller_shared = shared.clone();
        let caller = tokio::spawn(async move {
            start(
                caller_shared,
                |_| WorkRequest::Refresh {
                    config_revision: 99,
                    source_id: None,
                    automatic: false,
                },
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while shared.mutation.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        let _ = caller.await;
        assert_eq!(shared.mutation.available_permits(), 0);
        drop(registration);
        tokio::time::timeout(Duration::from_secs(1), async {
            while shared.mutation.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(shared.filter_tasks.lock().await.is_empty());
        // A manual refresh needs no client revision: it uses the saved one.
        let accepted = api(
            shared.clone(),
            transport.clone(),
            ApiRequest {
                peer: address,
                method: "POST",
                path: "/api/filter/subscriptions/refresh",
                headers: &headers,
                body: br#"{"source_id":null}"#,
            },
            &mut cookie,
        )
        .await
        .unwrap();
        while shared.filter_tasks.lock().await.join_next().await.is_some() {}
        let recent = shared
            .manager
            .lock()
            .await
            .filters
            .snapshot()
            .recent_operation
            .unwrap();
        assert_eq!(accepted["operation_id"], recent.id);
        assert_eq!(recent.status, "succeeded");
        // Revocation blocks manual admission but leaves the process scheduler
        // able to own and finish work without any administrator session.
        active.lock().unwrap().sessions.clear();
        let manual = start(
            shared.clone(),
            |config_revision| WorkRequest::Refresh {
                config_revision,
                source_id: None,
                automatic: false,
            },
            Some((&headers, &transport)),
        )
        .await
        .unwrap_err();
        assert_eq!(manual.1, "UNAUTHORIZED");
        start(
            shared.clone(),
            |config_revision| WorkRequest::Refresh {
                config_revision,
                source_id: None,
                automatic: true,
            },
            None,
        )
        .await
        .unwrap();
        while shared.filter_tasks.lock().await.join_next().await.is_some() {}
        assert_eq!(
            shared
                .manager
                .lock()
                .await
                .filters
                .snapshot()
                .recent_operation
                .unwrap()
                .status,
            "succeeded"
        );
        shared.manager.lock().await.terminal_shutdown().await;
    }
}
