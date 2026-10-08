//! Credentials share Store's commit point, independently of DNS configuration.
use super::{ApiError, Shared, Snapshot, Stored, decode, error, internal, invalid, store};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};

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
    if input.current_password.len() > 256 {
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
    // CPU work owns the existing hash permit, and never holds Manager.
    let replacement = shared
        .auth
        .password_work(move || {
            if !store::verify_password(&password_hash, &input.current_password) {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "CURRENT_PASSWORD_INCORRECT",
                    "Current password is incorrect",
                ));
            }
            let hash = store::hash_password(&input.new_password).map_err(invalid)?;
            Ok((input.username, hash))
        })
        .await??;
    let permit = shared
        .protected_mutation_permit(headers, &transport)
        .await?;
    let headers = headers.clone();
    // Once admitted, persistence and revocation outlive the HTTP request.
    tokio::spawn(async move {
        let _permit = permit;
        let mut manager = shared.manager.clone().lock_owned().await;
        shared.authorized(&headers, &transport)?;
        if !verified.matches(manager.saved.as_ref()) {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Sign in required",
            ));
        }
        // Clone at admission: unrelated configuration work may have completed
        // while hashing, and its revision/previous document must survive.
        let mut next = manager.saved.as_ref().ok_or_else(internal)?.clone();
        next.username = replacement.0;
        next.password_hash = replacement.1;
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
    })
    .await
    .map_err(|_| internal())?
}
