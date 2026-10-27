//! Credentials share Store's commit point, independently of DNS configuration.
//!
//! - A credential change is its own Store transaction, never a DNS configuration
//!   apply. It preserves the current/previous TOML and the configuration revision.
//! - The hash and attempt budget run outside Manager locks; serialized admission
//!   rechecks the verified username/hash and the caller's session.
//! - After Store's rename commit, saved credentials are published and all sessions
//!   revoked without awaiting, in Manager then Active lock order.
//! - Session issuance rechecks its credential snapshot. Protected manual mutations,
//!   including special updater paths, recheck session/realm at admission; schedulers
//!   need no session and already accepted background work is not revoked.
//! - A success or unknown outcome requires explicit login: never replay, clear the
//!   Cookie, issue a replacement session, or broadcast sensitive inputs.
use super::{ApiError, Manager, Shared, Snapshot, Stored, decode, error, internal, invalid, store};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{future::Future, net::SocketAddr, sync::Arc};
use tokio::sync::OwnedMutexGuard;

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotation {
    current_password: String,
    username: String,
    new_password: String,
}

#[derive(Clone)]
pub(super) struct Credentials {
    pub username: String,
    pub password_hash: String,
}

impl From<&Stored> for Credentials {
    fn from(saved: &Stored) -> Self {
        Self {
            username: saved.username.clone(),
            password_hash: saved.password_hash.clone(),
        }
    }
}

impl Credentials {
    pub fn matches(&self, current: Option<&Stored>) -> bool {
        current.is_some_and(|current| {
            current.username == self.username && current.password_hash == self.password_hash
        })
    }
}

pub(super) async fn rotate(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    peer: SocketAddr,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let input: Rotation = decode(body)?;
    store::validate_username(&input.username).map_err(invalid)?;
    store::validate_password(&input.new_password).map_err(invalid)?;
    let new_password = input.new_password;
    let username = input.username;
    change(
        shared,
        transport,
        peer,
        headers,
        input.current_password,
        move || store::hash_password(&new_password).map_err(invalid),
        |shared, mut manager, hash| async move {
            // Clone at admission: unrelated configuration work may have completed
            // while hashing, and its revision/previous document must survive.
            let mut next = manager.saved.as_ref().ok_or_else(internal)?.clone();
            next.username = username;
            next.password_hash = hash;
            tokio::task::spawn_blocking(move || {
                manager.store.save(&next).map_err(|_| internal())?;
                // Store rename is the commit point. No await separates publication
                // from revocation; retain the Manager -> Active lock order.
                manager.saved = Some(next);
                shared.active.lock().unwrap().sessions.clear();
                Ok(json!({"reauthentication_required":true}))
            })
            .await
            .map_err(|_| internal())?
        },
    )
    .await
}

/// The flow every credential change shares. The current password and the CPU
/// work of `prepare` run outside Manager on the existing hash permit. The
/// mutation permit then admits `commit`, which outlives the HTTP request and
/// runs only while the caller's session and the verified credentials hold.
pub(super) async fn change<R, Commit>(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    peer: SocketAddr,
    headers: &HeaderMap,
    current_password: String,
    prepare: impl FnOnce() -> Result<R, ApiError> + Send + 'static,
    commit: impl FnOnce(Arc<Shared>, OwnedMutexGuard<Manager>, R) -> Commit + Send + 'static,
) -> Result<Value, ApiError>
where
    R: Send + 'static,
    Commit: Future<Output = Result<Value, ApiError>> + Send,
{
    if current_password.len() > 256 {
        return Err(invalid(anyhow::anyhow!(
            "Current password must contain at most 256 bytes"
        )));
    }
    shared.auth.attempt(peer.ip())?;
    let verified = shared
        .manager
        .lock()
        .await
        .saved
        .as_ref()
        .map(Credentials::from)
        .ok_or_else(internal)?;
    let password_hash = verified.password_hash.clone();
    let prepared = shared
        .auth
        .password_work(move || {
            if !store::verify_password(&password_hash, &current_password) {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "CURRENT_PASSWORD_INCORRECT",
                    "Current password is incorrect",
                ));
            }
            prepare()
        })
        .await??;
    let permit = shared
        .protected_mutation_permit(headers, &transport)
        .await?;
    let headers = headers.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let manager = shared.manager.clone().lock_owned().await;
        shared.authorized(&headers, &transport)?;
        if !verified.matches(manager.saved.as_ref()) {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Sign in required",
            ));
        }
        commit(shared, manager, prepared).await
    })
    .await
    .map_err(|_| internal())?
}
