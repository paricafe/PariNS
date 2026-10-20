//! Optional Cloudflare Turnstile for console sign-in. Off by default.
//!
//! - The keys live only in the private Store file `turnstile.json`, never in the
//!   configuration TOML, which is returned to the browser, exported and rolled
//!   back. The secret key is write-only: never logged or returned; the console
//!   sees only the site key and whether Cloudflare test keys are set.
//! - A sign-in passes Turnstile before the password is checked, charged to the
//!   socket peer's attempt budget. Cloudflare siteverify is asked through the
//!   pinned HTTPS reader, with one deadline and a response size limit, and must
//!   report success for this console's host name and the `login` action.
//!   Failure, or an unreachable Cloudflare, refuses the sign-in.
//! - A loopback socket peer (an SSH tunnel) skips Turnstile, so the administrator
//!   can always sign in from the server. Only the accepted socket counts; there
//!   is no forwarding header to trust. A same-host proxy or tunnel therefore makes
//!   every client loopback; the guide and the console warn about it.
//! - Cloudflare's published test secret keys answer for any token, host and
//!   action, so they are accepted only with a test site key.
//! - Only a signed-out console document that shows the widget relaxes the CSP for
//!   its script and frame.
//! - Setting or clearing the keys is a credential change, as in `account`: the
//!   current password, one Store write, then every session is revoked. The
//!   generation it advances rejects a sign-in that passed the old setting.
use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ApiError, Shared, Snapshot,
    account::Credentials,
    decode, error, internal, invalid,
    store::{self, Store},
};

#[cfg(test)]
#[path = "turnstile_tests.rs"]
mod tests;

pub(super) const FILE: &str = "turnstile.json";
const MAX_FILE: usize = 4 * 1024;
#[cfg_attr(test, allow(dead_code))]
const SITEVERIFY: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
#[cfg(not(test))]
const DEADLINE: Duration = Duration::from_secs(8);
#[cfg(test)]
const DEADLINE: Duration = Duration::from_secs(1);
const RESPONSE_LIMIT: usize = 16 * 1024;
const ACTION: &str = "login";
/// Cloudflare documents tokens of at most 2048 characters.
const TOKEN_LIMIT: usize = 2048;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Keys {
    site_key: String,
    secret_key: String,
}

/// The saved keys and the generation that every change advances.
pub(super) struct State {
    pub keys: Option<Keys>,
    pub generation: u64,
    /// A local stand-in for siteverify, reached over plain HTTP.
    #[cfg(test)]
    pub siteverify: Option<SocketAddr>,
}

impl State {
    pub fn load(dir: &Path) -> Result<Self> {
        Ok(Self {
            keys: read(dir)?,
            generation: 0,
            #[cfg(test)]
            siteverify: None,
        })
    }
}

/// Absent means off. Shared by startup and the read-only managed check.
pub(super) fn read(dir: &Path) -> Result<Option<Keys>> {
    let Some(bytes) = store::read_bounded(&dir.join(FILE), MAX_FILE)? else {
        return Ok(None);
    };
    let keys: Keys = serde_json::from_slice(&bytes).context("invalid Turnstile state")?;
    ensure!(valid(&keys), "invalid Turnstile keys");
    Ok(Some(keys))
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && key.bytes().all(|b| b.is_ascii_graphic())
}

fn valid(keys: &Keys) -> bool {
    valid_key(&keys.site_key)
        && valid_key(&keys.secret_key)
        && test_secret(&keys.secret_key).is_some() == test_site_key(&keys.site_key)
}

/// Cloudflare's published test secret keys: `Some(true)` always passes,
/// `Some(false)` always fails (or reports the token as already spent).
fn test_secret(key: &str) -> Option<bool> {
    match key {
        "1x0000000000000000000000000000000AA" => Some(true),
        "2x0000000000000000000000000000000AA" | "3x0000000000000000000000000000000AA" => {
            Some(false)
        }
        _ => None,
    }
}

/// Cloudflare's published test site keys, such as `1x00000000000000000000AA`
/// or `3x00000000000000000000FF`: `1x`/`2x`/`3x`, 20 zeros, two letters.
fn test_site_key(key: &str) -> bool {
    key.len() == 24
        && key.is_ascii()
        && matches!(&key[..2], "1x" | "2x" | "3x")
        && key[2..22].bytes().all(|b| b == b'0')
        && key[22..].bytes().all(|b| b.is_ascii_uppercase())
}

/// A loopback socket peer, such as an SSH tunnel.
pub(super) fn loopback(peer: SocketAddr) -> bool {
    match peer.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

/// The host name Cloudflare must report: the configured public host over HTTPS,
/// otherwise the console address that `host_allowed` already admitted.
fn expected_hostname(transport: &Snapshot, host: &str) -> String {
    transport.public_host.clone().unwrap_or_else(|| {
        super::origin_identity(host, transport.scheme.default_port())
            .map(|(name, _)| name)
            .unwrap_or_default()
    })
}

/// The keys and generation read together: a sign-in that passes Turnstile is
/// valid only while that generation is current.
pub(super) struct Admission {
    keys: Option<Keys>,
    pub generation: u64,
    #[cfg(test)]
    siteverify: Option<SocketAddr>,
}

pub(super) async fn admission(shared: &Shared) -> Admission {
    let manager = shared.manager.lock().await;
    Admission {
        keys: manager.turnstile.keys.clone(),
        generation: manager.turnstile.generation,
        #[cfg(test)]
        siteverify: manager.turnstile.siteverify,
    }
}

impl Admission {
    /// Callers charge the attempt budget first, so failures count.
    pub async fn check(
        &self,
        transport: &Snapshot,
        peer: SocketAddr,
        host: &str,
        token: Option<&str>,
    ) -> Result<(), ApiError> {
        let Some(keys) = &self.keys else {
            return Ok(());
        };
        if loopback(peer) {
            return Ok(());
        }
        let token = token
            .filter(|token| !token.is_empty() && token.len() <= TOKEN_LIMIT)
            .ok_or_else(|| {
                error(
                    StatusCode::FORBIDDEN,
                    "TURNSTILE_REQUIRED",
                    "Complete the human verification",
                )
            })?;
        let outcome = tokio::time::timeout(DEADLINE, self.siteverify(&keys.secret_key, token))
            .await
            .map_err(|_| unavailable())??;
        let testing = outcome
            .metadata
            .is_some_and(|metadata| metadata.result_with_testing_key);
        let hostname = expected_hostname(transport, host);
        let matches = outcome
            .hostname
            .is_some_and(|name| name.eq_ignore_ascii_case(&hostname))
            && outcome.action.as_deref() == Some(ACTION);
        if outcome.success && (matches || testing) {
            Ok(())
        } else {
            Err(error(
                StatusCode::FORBIDDEN,
                "TURNSTILE_FAILED",
                "Human verification failed; try again",
            ))
        }
    }

    async fn siteverify(&self, secret: &str, token: &str) -> Result<Outcome, ApiError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("secret", secret)
            .append_pair("response", token)
            .finish();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let body = Bytes::from(body);
        #[cfg(test)]
        let response = match self.siteverify {
            Some(address) => {
                crate::https_reader::send_plain(
                    address,
                    Method::POST,
                    Uri::from_static("http://siteverify.test/turnstile/v0/siteverify"),
                    headers,
                    body,
                )
                .await
            }
            // Tests never reach Cloudflare.
            None => Err(crate::https_reader::ReaderError::new("network_error")),
        };
        #[cfg(not(test))]
        let response = crate::https_reader::HttpsReader::new()
            .send(Method::POST, Uri::from_static(SITEVERIFY), headers, body)
            .await;
        let response = response.map_err(|_| unavailable())?;
        if response.status != StatusCode::OK {
            return Err(unavailable());
        }
        let mut body = response.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| unavailable())?;
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| unavailable())
    }
}

#[derive(Deserialize)]
struct Outcome {
    success: bool,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default)]
    result_with_testing_key: bool,
}

fn unavailable() -> ApiError {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "TURNSTILE_UNAVAILABLE",
        "Human verification could not be checked; try again",
    )
}

/// Whether the console document served to this request must allow the widget:
/// only a sign-in page that will show it.
pub(super) async fn document_needs_widget(
    shared: &Shared,
    transport: &Snapshot,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> bool {
    !loopback(peer)
        && shared.manager.lock().await.turnstile.keys.is_some()
        && shared
            .find_session(headers, transport)
            .ok()
            .flatten()
            .is_none()
}

/// What the sign-in page shows. Public before sign-in: the site key is public.
pub(super) async fn options(shared: &Shared, peer: SocketAddr) -> Value {
    let manager = shared.manager.lock().await;
    let keys = manager.turnstile.keys.as_ref();
    let skipped = keys.is_some() && loopback(peer);
    json!({
        "turnstile": keys.filter(|_| !skipped).map(|keys| json!({
            "site_key": keys.site_key,
            "action": ACTION,
        })),
        "turnstile_skipped": skipped,
    })
}

/// The settings view. Never includes the secret key.
pub(super) async fn view(shared: &Shared, peer: SocketAddr) -> Value {
    let manager = shared.manager.lock().await;
    json!({
        "turnstile": manager.turnstile.keys.as_ref().map(|keys| json!({
            "site_key": keys.site_key,
            "secret_set": true,
            // "pass" or "fail": Cloudflare test keys decide every sign-in alike.
            "test_keys": test_secret(&keys.secret_key)
                .map(|passes| if passes { "pass" } else { "fail" }),
        })),
        "loopback": loopback(peer),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    site_key: String,
    /// Omitted keeps the saved secret key; it is never sent back.
    #[serde(default)]
    secret_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    current_password: String,
    /// `null` turns Turnstile off.
    turnstile: Option<Setting>,
}

fn test_keys_unpaired() -> ApiError {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "TURNSTILE_TEST_KEYS",
        "Use Cloudflare test keys only as a pair: a test site key with a test secret key",
    )
}

async fn persist(store: Arc<Store>, keys: Option<Keys>) -> Result<(), ApiError> {
    tokio::task::spawn_blocking(move || match keys {
        Some(keys) => {
            let bytes = serde_json::to_vec(&keys).context("encode Turnstile state")?;
            store.atomic_write(FILE, &bytes)
        }
        None => {
            let path = store.dir.join(FILE);
            if store::checked_open(&path, false)?.is_some() {
                std::fs::remove_file(&path).context("remove Turnstile state")?;
                // Removal is the commit point; as with atomic_write, a failed
                // directory sync leaves only power-loss durability uncertain.
                let _ = std::fs::File::open(&store.dir).and_then(|dir| dir.sync_all());
            }
            Ok(())
        }
    })
    .await
    .map_err(|_| internal())?
    .map_err(|_| internal())
}

pub(super) async fn change(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    peer: SocketAddr,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let input: Change = decode(body)?;
    if input.current_password.len() > 256 {
        return Err(invalid(anyhow::anyhow!(
            "Current password must contain at most 256 bytes"
        )));
    }
    if let Some(setting) = &input.turnstile
        && !(valid_key(&setting.site_key) && setting.secret_key.as_deref().is_none_or(valid_key))
    {
        return Err(invalid(anyhow::anyhow!(
            "Turnstile keys must be 1 to 256 visible ASCII characters"
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
    let current_password = input.current_password;
    // CPU work owns the existing hash permit, and never holds Manager.
    let correct = shared
        .auth
        .password_work(move || store::verify_password(&password_hash, &current_password))
        .await?;
    if !correct {
        return Err(error(
            StatusCode::FORBIDDEN,
            "CURRENT_PASSWORD_INCORRECT",
            "Current password is incorrect",
        ));
    }
    let permit = shared
        .protected_mutation_permit(headers, &transport)
        .await?;
    let headers = headers.clone();
    let setting = input.turnstile;
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
        let next = match setting {
            None => None,
            Some(Setting {
                site_key,
                secret_key,
            }) => {
                let secret_key = secret_key
                    .or_else(|| {
                        manager
                            .turnstile
                            .keys
                            .as_ref()
                            .map(|keys| keys.secret_key.clone())
                    })
                    .ok_or_else(|| invalid(anyhow::anyhow!("Enter the Turnstile secret key")))?;
                let keys = Keys {
                    site_key,
                    secret_key,
                };
                // A test secret passes (or fails) every token whatever the
                // widget, so it is accepted only with a test site key.
                if !valid(&keys) {
                    return Err(test_keys_unpaired());
                }
                Some(keys)
            }
        };
        persist(manager.store.clone(), next.clone()).await?;
        // The Store commit is done. No await separates publication from
        // revocation; retain the Manager -> Active lock order.
        manager.turnstile.keys = next;
        manager.turnstile.generation += 1;
        shared.active.lock().unwrap().sessions.clear();
        Ok(json!({"reauthentication_required": true}))
    })
    .await
    .map_err(|_| internal())?
}
