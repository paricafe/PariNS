//! Management owns admission, mutation ordering and the lifetime of rule work.
//! Downloads and compilation never hold the Manager or mutation lock.
use super::{ApiError, Shared, decode, error};
use crate::filter_subscriptions::service::{DraftSource, WorkRequest};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    config_revision: u64,
    source: DraftSource,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    config_revision: u64,
    source_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    name: String,
}

pub(super) async fn api(shared: Arc<Shared>, path: &str, body: &[u8]) -> Result<Value, ApiError> {
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
                WorkRequest::Prepare {
                    config_revision: input.config_revision,
                    source: input.source,
                },
            )
            .await
        }
        "/api/filter/subscriptions/refresh" => {
            let input: Refresh = decode(body)?;
            start(
                shared,
                WorkRequest::Refresh {
                    config_revision: input.config_revision,
                    source_id: input.source_id,
                    automatic: false,
                },
            )
            .await
        }
        _ => unreachable!("filtered route"),
    }
}

async fn start(shared: Arc<Shared>, request: WorkRequest) -> Result<Value, ApiError> {
    let permit = shared.mutation_permit().await?;
    // Admission may fsync catalog maintenance. Its permit and registration must
    // survive the initiating HTTP connection, just like configuration apply.
    tokio::spawn(async move {
        let _permit = permit;
        let filters = shared.manager.lock().await.filters.clone();
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
                    let manager = control.manager.lock().await;
                    anyhow::ensure!(
                        !manager.updates.frozen.load(Ordering::Acquire),
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
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                manager.filters.next_due(now)
            } else {
                None
            }
        };
        if let Some(request) = request {
            let _ = start(shared.clone(), request).await;
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
    async fn read_only_routes_require_session_and_mutations_obey_revision_and_freeze() {
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
        let input = br#"{"config_revision":99,"source":{"id":"example","url":"https://example.org/list","format":"domain_list"}}"#;
        let conflict = api(
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
        assert_eq!(conflict.1, "revision_conflict");
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
                body: input,
            },
            &mut cookie,
        )
        .await
        .unwrap_err();
        assert_eq!(frozen.1, "update_in_progress");
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
                WorkRequest::Refresh {
                    config_revision: 99,
                    source_id: None,
                    automatic: false,
                },
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
        shared.manager.lock().await.terminal_shutdown().await;
    }
}
