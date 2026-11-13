//! Optional Cloudflare Turnstile for console sign-in.
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
    ApiError, Shared, Snapshot, account, decode, error, internal, invalid,
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
const TOKEN_LIMIT: usize = 2048;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Keys {
    site_key: String,
    secret_key: String,
}

pub(super) struct State {
    pub keys: Option<Keys>,
    pub generation: u64,
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

fn test_secret(key: &str) -> Option<bool> {
    match key {
        "1x0000000000000000000000000000000AA" => Some(true),
        "2x0000000000000000000000000000000AA" | "3x0000000000000000000000000000000AA" => {
            Some(false)
        }
        _ => None,
    }
}

fn test_site_key(key: &str) -> bool {
    key.len() == 24
        && key.is_ascii()
        && matches!(&key[..2], "1x" | "2x" | "3x")
        && key[2..22].bytes().all(|b| b == b'0')
        && key[22..].bytes().all(|b| b.is_ascii_uppercase())
}

pub(super) fn loopback(peer: SocketAddr) -> bool {
    match peer.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

fn expected_hostname(transport: &Snapshot, host: &str) -> String {
    transport.public_host.clone().unwrap_or_else(|| {
        super::origin_identity(host, transport.scheme.default_port())
            .map(|(name, _)| name)
            .unwrap_or_default()
    })
}

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
        let testing = test_secret(&keys.secret_key).is_some()
            && outcome
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
            None => Err(crate::https_reader::ReaderError::new("network_error")),
        };
        #[cfg(not(test))]
        static READER: std::sync::LazyLock<crate::https_reader::HttpsReader> =
            std::sync::LazyLock::new(crate::https_reader::HttpsReader::new);
        #[cfg(not(test))]
        let response = READER
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

pub(super) async fn view(shared: &Shared, peer: SocketAddr) -> Value {
    let manager = shared.manager.lock().await;
    json!({
        "turnstile": manager.turnstile.keys.as_ref().map(|keys| json!({
            "site_key": keys.site_key,
            "secret_set": true,
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
    #[serde(default)]
    secret_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    current_password: String,
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
            if crate::private_files::checked_open(&path, false)?.is_some() {
                std::fs::remove_file(&path).context("remove Turnstile state")?;
                store::sync_committed(&store.dir, "Turnstile key removal");
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
    if let Some(setting) = &input.turnstile
        && !(valid_key(&setting.site_key) && setting.secret_key.as_deref().is_none_or(valid_key))
    {
        return Err(invalid(anyhow::anyhow!(
            "Turnstile keys must be 1 to 256 visible ASCII characters"
        )));
    }
    let setting = input.turnstile;
    account::change(
        shared,
        transport,
        peer,
        headers,
        input.current_password,
        || Ok(()),
        |shared, mut manager, ()| async move {
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
                        .ok_or_else(|| {
                            invalid(anyhow::anyhow!("Enter the Turnstile secret key"))
                        })?;
                    let keys = Keys {
                        site_key,
                        secret_key,
                    };
                    if !valid(&keys) {
                        return Err(test_keys_unpaired());
                    }
                    Some(keys)
                }
            };
            persist(manager.store.clone(), next.clone()).await?;
            manager.turnstile.keys = next;
            manager.turnstile.generation += 1;
            shared.active.lock().unwrap().sessions.clear();
            Ok(json!({"reauthentication_required": true}))
        },
    )
    .await
}
