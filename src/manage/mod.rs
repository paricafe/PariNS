//! Authenticated management plane. DNS forwarding remains in Server/Resolver.
mod auth_budget;
mod cache;
mod certificates;
mod runtime;
mod settings;
pub(crate) mod store;
mod transport;

use anyhow::Result;
use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use runtime::Manager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use store::{Store, Stored};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};
use transport::{Scheme, Snapshot};

const BODY_LIMIT: usize = 300 * 1024;
const SESSION_LIFETIME: Duration = Duration::from_secs(8 * 3600);
const HTTPS_COOKIE: &str = "__Host-parins_session";
const HTTP_COOKIE: &str = "parins_session_http";
#[derive(Clone)]
struct Session {
    token: String,
    binding: String,
    created: Instant,
}
struct Active {
    snapshot: Arc<Snapshot>,
    sessions: Vec<Session>,
}
struct Shared {
    manager: Arc<tokio::sync::Mutex<Manager>>,
    active: Arc<Mutex<Active>>,
    auth: auth_budget::Budget,
    mutation: Arc<Semaphore>,
    address: SocketAddr,
}

fn host_allowed(address: SocketAddr, host: &str, transport: &Snapshot) -> bool {
    let Some((name, port)) = origin_identity(host, transport.scheme.default_port()) else {
        return false;
    };
    if port != address.port() {
        return false;
    }
    if let Some(public) = &transport.public_host
        && *public == name
    {
        return true;
    }
    if transport.scheme == Scheme::Https {
        return false;
    }
    if name == "localhost" {
        return true;
    }
    // Literal IPs support both directly assigned addresses and public-IP NAT.
    // Arbitrary domain names remain rejected to prevent DNS rebinding. The Host
    // header is not an identity: setup tokens and Cookie auth are still required.
    name.parse::<IpAddr>().is_ok_and(|ip| {
        !ip.is_unspecified()
            && !ip.is_multicast()
            && (!address.ip().is_loopback() || ip.is_loopback())
    })
}

// Origin and Host must name the same actual transport origin, including effective port.
// Keeping localhost distinct from an IP literal also rejects same-site requests
// sent from another management port or host.
fn origin_identity(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if let Ok(address) = authority.parse::<SocketAddr>() {
        return Some((address.ip().to_string(), address.port()));
    }
    if let Some(literal) = authority
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
    {
        let ip = literal.parse::<std::net::Ipv6Addr>().ok()?;
        return Some((ip.to_string(), default_port));
    }
    if let Ok(ip) = authority.parse::<std::net::Ipv4Addr>() {
        return Some((ip.to_string(), default_port));
    }
    let (name, port) = match authority.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => (name, port.parse().ok()?),
        None => (authority, default_port),
        _ => return None,
    };
    let name = crate::config::public_host(name).ok()?;
    if name.parse::<IpAddr>().is_ok() {
        return None;
    }
    Some((name, port))
}

fn same_origin(headers: &HeaderMap, host: &str, unsafe_method: bool, scheme: Scheme) -> bool {
    let origins = headers.get_all(header::ORIGIN);
    let mut origins = origins.iter();
    let Some(origin) = origins.next() else {
        return !unsafe_method;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(source) = origin.to_str().ok().and_then(|s| {
        s.strip_prefix(if scheme == Scheme::Https {
            "https://"
        } else {
            "http://"
        })
    }) else {
        return false;
    };
    origin_identity(source, scheme.default_port())
        .is_some_and(|source| Some(source) == origin_identity(host, scheme.default_port()))
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            axum::Json(json!({"error":{"code":self.1,"message":self.2}})),
        )
            .into_response()
    }
}
fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError(status, code, message.into())
}
fn invalid(err: anyhow::Error) -> ApiError {
    let code = if err.is::<crate::tls::ManagementNameMismatch>() {
        "CERTIFICATE_NAME_MISMATCH"
    } else if err.is::<transport::CertificateInvalid>() {
        "CERTIFICATE_INVALID"
    } else if err
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
    {
        "CONFIG_CONFLICT"
    } else {
        "INVALID_CONFIG"
    };
    error(StatusCode::UNPROCESSABLE_ENTITY, code, format!("{err:#}"))
}
fn internal() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Management operation failed",
    )
}
fn storage_error(err: crate::storage::Error) -> ApiError {
    use crate::storage::Error;
    let (status, code) = match &err {
        Error::Conflict { .. } => (StatusCode::CONFLICT, "EPOCH"),
        Error::StorageUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "STORAGE_UNAVAILABLE"),
        Error::Busy => (StatusCode::SERVICE_UNAVAILABLE, "STORAGE_BUSY"),
        Error::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
    };
    error(status, code, err.to_string())
}

fn history_options(query: &str) -> std::result::Result<crate::storage::HistoryOptions, ApiError> {
    let mut input = serde_json::Map::new();
    if !query.is_empty() {
        for field in query.split('&') {
            let (name, value) = field.split_once('=').ok_or_else(|| {
                error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    "Invalid history query",
                )
            })?;
            if input.contains_key(name) {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    "Duplicate history option",
                ));
            }
            let value = match name {
                "range" => json!(value),
                "from_ms" | "to_ms" | "max_points" => {
                    json!(value.parse::<u64>().map_err(|_| error(
                        StatusCode::BAD_REQUEST,
                        "INVALID_REQUEST",
                        "Invalid history number"
                    ))?)
                }
                _ => {
                    return Err(error(
                        StatusCode::BAD_REQUEST,
                        "INVALID_REQUEST",
                        "Unknown history option",
                    ));
                }
            };
            input.insert(name.to_owned(), value);
        }
    }
    serde_json::from_value(Value::Object(input)).map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Invalid history options",
        )
    })
}
fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> std::result::Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "BAD_JSON", "Invalid JSON request"))
}

impl Shared {
    fn ensure_current(&self, transport: &Snapshot) -> std::result::Result<(), ApiError> {
        if self.active.lock().unwrap().snapshot.realm != transport.realm {
            return Err(error(
                StatusCode::CONFLICT,
                "TRANSPORT_CHANGED",
                "Management address changed; reconnect",
            ));
        }
        Ok(())
    }

    fn session(&self, transport: &Snapshot) -> std::result::Result<Session, ApiError> {
        let mut active = self.active.lock().unwrap();
        if active.snapshot.realm != transport.realm {
            return Err(error(
                StatusCode::CONFLICT,
                "TRANSPORT_CHANGED",
                "Management address changed; reconnect",
            ));
        }
        active
            .sessions
            .retain(|s| s.created.elapsed() < SESSION_LIFETIME);
        if active.sessions.len() >= 16 {
            active.sessions.remove(0);
        }
        let session = Session {
            token: store::secret(),
            binding: store::secret(),
            created: Instant::now(),
        };
        active.sessions.push(session.clone());
        Ok(session)
    }

    fn find_session(
        &self,
        headers: &HeaderMap,
        transport: &Snapshot,
    ) -> std::result::Result<Option<Session>, ApiError> {
        let mut active = self.active.lock().unwrap();
        if active.snapshot.realm != transport.realm {
            return Err(error(
                StatusCode::CONFLICT,
                "TRANSPORT_CHANGED",
                "Management address changed; reconnect",
            ));
        }
        let Some(supplied) = session_cookie(headers, transport.scheme) else {
            return Ok(None);
        };
        active
            .sessions
            .retain(|s| s.created.elapsed() < SESSION_LIFETIME);
        Ok(active
            .sessions
            .iter()
            .find(|s| s.token == supplied)
            .cloned())
    }

    fn authorized(
        &self,
        headers: &HeaderMap,
        transport: &Snapshot,
    ) -> std::result::Result<(), ApiError> {
        let session = self
            .find_session(headers, transport)?
            .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Sign in required"))?;
        verify_binding(headers, &session)
    }

    fn revoke_session(
        &self,
        headers: &HeaderMap,
        transport: &Snapshot,
    ) -> std::result::Result<(), ApiError> {
        let mut active = self.active.lock().unwrap();
        if active.snapshot.realm != transport.realm {
            return Err(error(
                StatusCode::CONFLICT,
                "TRANSPORT_CHANGED",
                "Management address changed; reconnect",
            ));
        }
        active
            .sessions
            .retain(|session| session.created.elapsed() < SESSION_LIFETIME);
        let Some(token) = session_cookie(headers, transport.scheme) else {
            return Ok(());
        };
        if let Some(session) = active
            .sessions
            .iter()
            .find(|session| session.token == token)
        {
            verify_binding(headers, session)?;
            active.sessions.retain(|session| session.token != token);
        }
        Ok(())
    }
}

fn verify_binding(headers: &HeaderMap, session: &Session) -> std::result::Result<(), ApiError> {
    if headers.get_all("x-parins-session").iter().count() != 1
        || headers
            .get("x-parins-session")
            .and_then(|h| h.to_str().ok())
            != Some(session.binding.as_str())
    {
        return Err(error(
            StatusCode::CONFLICT,
            "SESSION_CHANGED",
            "Session changed; check your current sign-in",
        ));
    }
    Ok(())
}

fn session_cookie(headers: &HeaderMap, scheme: Scheme) -> Option<&str> {
    let expected = if scheme == Scheme::Https {
        HTTPS_COOKIE
    } else {
        HTTP_COOKIE
    };
    let mut found = None;
    for header in headers.get_all(header::COOKIE).iter() {
        for part in header.to_str().ok()?.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name == expected {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}

fn session_view(setup_required: bool, session: Option<&Session>) -> Value {
    match session {
        Some(session) => json!({
            "setup_required":setup_required,
            "authenticated":true,
            "session":{
                "binding":session.binding,
                "expires_in_seconds":SESSION_LIFETIME.saturating_sub(session.created.elapsed()).as_secs(),
            }
        }),
        None => json!({"setup_required":setup_required,"authenticated":false,"session":null}),
    }
}

fn with_transport(mut view: Value, transport: &Snapshot) -> Value {
    view["transport"] = transport.view();
    view
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    username: String,
    password: String,
    toml: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    toml: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    toml: String,
    revision: u64,
    #[serde(default)]
    allow_http_downgrade: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: u64,
    #[serde(default)]
    allow_http_downgrade: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateImport {
    revision: u64,
    certificate_pem: String,
    private_key_pem: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogClear {
    revision: u64,
    log_epoch: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TotalsReset {
    revision: u64,
    totals_epoch: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryClear {
    revision: u64,
    history_epoch: u64,
}

struct ApiRequest<'a> {
    peer: SocketAddr,
    method: &'a str,
    path: &'a str,
    headers: &'a HeaderMap,
    body: &'a [u8],
}

async fn api(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    input: ApiRequest<'_>,
    cookie: &mut Option<String>,
) -> std::result::Result<Value, ApiError> {
    let ApiRequest {
        peer,
        method,
        path,
        headers,
        body,
    } = input;
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    shared.ensure_current(&transport)?;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match (method, path) {
        ("GET", "/api/session") => {
            let setup_required = shared.manager.lock().await.saved.is_none();
            let session = shared.find_session(headers, &transport)?;
            return Ok(with_transport(
                session_view(setup_required, session.as_ref()),
                &transport,
            ));
        }
        ("GET", "/api/template") => {
            return Ok(json!({"toml":include_str!("../../parins.example.toml")}));
        }
        ("POST", "/api/setup/preview") => {
            let document: Document = decode(body)?;
            let manager = shared.manager.lock().await;
            if manager.saved.is_some() {
                return Err(error(
                    StatusCode::CONFLICT,
                    "ALREADY_SETUP",
                    "Setup is already complete",
                ));
            }
            let supplied = headers
                .get("x-parins-setup")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let expected = manager.store.setup_token().map_err(|_| internal())?;
            if supplied != expected {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "SETUP_TOKEN",
                    "Invalid setup token",
                ));
            }
            let change = manager
                .transport_change(&document.toml, host)
                .map_err(invalid)?;
            manager.validate(document.toml).await.map_err(invalid)?;
            return Ok(json!({"valid":true,"transport_change":change}));
        }
        ("POST", "/api/login") => {
            shared.auth.attempt(peer.ip())?;
            let credentials: Credentials = decode(body)?;
            if credentials.username.len() > 64 || credentials.password.len() > 256 {
                return Err(error(
                    StatusCode::UNAUTHORIZED,
                    "LOGIN_FAILED",
                    "Invalid credentials",
                ));
            }
            let saved = shared.manager.lock().await.saved.clone().ok_or_else(|| {
                error(
                    StatusCode::CONFLICT,
                    "SETUP_REQUIRED",
                    "Complete setup first",
                )
            })?;
            let valid = shared
                .auth
                .password_work(move || {
                    let valid_password =
                        store::verify_password(&saved.password_hash, &credentials.password);
                    valid_password && saved.username == credentials.username
                })
                .await?;
            if !valid {
                return Err(error(
                    StatusCode::UNAUTHORIZED,
                    "LOGIN_FAILED",
                    "Invalid credentials",
                ));
            }
            let session = shared.session(&transport)?;
            *cookie = Some(session.token.clone());
            return Ok(with_transport(
                session_view(false, Some(&session)),
                &transport,
            ));
        }
        ("POST", "/api/setup") => {
            shared.auth.attempt(peer.ip())?;
            let setup: Setup = decode(body)?;
            let supplied = headers
                .get("x-parins-setup")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            let control = shared.clone();
            let host = host.to_owned();
            let result = tokio::spawn(async move {
                let _permit = permit;
                let mut manager = control.manager.lock().await;
                if manager.saved.is_some() {
                    return Err(error(
                        StatusCode::CONFLICT,
                        "ALREADY_SETUP",
                        "Setup is already complete",
                    ));
                }
                let expected = manager.store.setup_token().map_err(|_| internal())?;
                if supplied != expected {
                    return Err(error(
                        StatusCode::FORBIDDEN,
                        "SETUP_TOKEN",
                        "Invalid setup token",
                    ));
                }
                if setup.username.is_empty()
                    || setup.username.len() > 64
                    || !setup
                        .username
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Err(invalid(anyhow::anyhow!(
                        "Username must be 1..64 ASCII letters, digits, hyphen or underscore",
                    )));
                }
                let hash = control
                    .auth
                    .password_work(move || store::hash_password(&setup.password))
                    .await?
                    .map_err(invalid)?;
                let change = manager
                    .transport_change(&setup.toml, &host)
                    .map_err(invalid)?;
                manager
                    .apply(Stored {
                        username: setup.username,
                        password_hash: hash,
                        toml: setup.toml,
                        previous: None,
                        revision: 1,
                    })
                    .await
                    .map_err(invalid)?;
                Ok::<_, ApiError>(change)
            })
            .await
            .map_err(|_| internal())?;
            let change = result?;
            let next = shared.active.lock().unwrap().snapshot.clone();
            if next.realm != transport.realm {
                return Ok(json!({"setup_required":false,"authenticated":false,
                    "session":null,"transport":next.view(),"transport_change":change}));
            }
            let session = shared.session(&transport)?;
            *cookie = Some(session.token.clone());
            let mut view = with_transport(session_view(false, Some(&session)), &transport);
            view["transport_change"] = change;
            return Ok(view);
        }
        ("POST", "/api/logout") => {
            shared.revoke_session(headers, &transport)?;
            let setup_required = shared.manager.lock().await.saved.is_none();
            return Ok(with_transport(
                session_view(setup_required, None),
                &transport,
            ));
        }
        _ => {}
    }
    shared.authorized(headers, &transport)?;
    match (method, path) {
        ("GET", "/api/status") => {
            let manager = shared.manager.lock().await;
            let mut status = manager.status();
            status["transport"] = manager.transport();
            Ok(status)
        }
        ("GET", "/api/stats") => {
            let input = history_options(query)?;
            let services = shared.manager.lock().await.services.clone();
            Ok(json!(
                services.statistics(input).await.map_err(storage_error)?
            ))
        }
        ("POST", "/api/query-log/list") => {
            let input: crate::query_log::ListOptions = decode(body)?;
            input.validate().map_err(invalid)?;
            let manager = shared.manager.lock().await;
            let revision = manager.saved.as_ref().map_or(0, |s| s.revision);
            let services = manager.services.clone();
            drop(manager);
            Ok(
                json!({"revision":revision,"page":services.list_logs(input).await.map_err(storage_error)?}),
            )
        }
        ("POST", "/api/query-log/clear" | "/api/stats/reset" | "/api/stats/history/clear") => {
            let (revision, epoch, action) = match path {
                "/api/query-log/clear" => {
                    let v: LogClear = decode(body)?;
                    (v.revision, v.log_epoch, 0)
                }
                "/api/stats/reset" => {
                    let v: TotalsReset = decode(body)?;
                    (v.revision, v.totals_epoch, 1)
                }
                _ => {
                    let v: HistoryClear = decode(body)?;
                    (v.revision, v.history_epoch, 2)
                }
            };
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            tokio::spawn(async move {
                let _permit = permit;
                let manager = shared.manager.lock().await;
                if manager.saved.as_ref().map(|s| s.revision) != Some(revision) {
                    return Err(error(
                        StatusCode::CONFLICT,
                        "REVISION",
                        "Configuration changed; refresh before clearing",
                    ));
                }
                let services = manager.services.clone();
                drop(manager);
                let result = match action {
                    0 => services.clear_logs(epoch).await,
                    1 => services.reset_totals(epoch).await,
                    _ => services.clear_history(epoch).await,
                }
                .map_err(storage_error)?;
                Ok(json!({"removed":result.removed,"epoch":result.epoch,"revision":revision}))
            })
            .await
            .map_err(|_| internal())?
        }
        ("POST", "/api/certificates/reload") => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Reload {
                revision: u64,
            }
            let input: Reload = decode(body)?;
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            tokio::spawn(async move {
                let _permit = permit;
                let mut manager = shared.manager.lock().await;
                if manager.saved.as_ref().map_or(0, |s| s.revision) != input.revision {
                    return Err(error(
                        StatusCode::CONFLICT,
                        "REVISION",
                        "Configuration changed; refresh before reloading",
                    ));
                }
                manager.reload_certificates("api").await.map_err(invalid)
            })
            .await
            .map_err(|_| internal())?
        }
        ("POST", "/api/certificates/import") => {
            let input: CertificateImport = decode(body)?;
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            tokio::spawn(async move {
                let _permit = permit;
                let manager = shared.manager.lock().await;
                if manager.saved.as_ref().map(|s| s.revision) != Some(input.revision) {
                    return Err(error(
                        StatusCode::CONFLICT,
                        "REVISION",
                        "Configuration changed; reload before importing",
                    ));
                }
                let store = manager.store.clone();
                let result = tokio::task::spawn_blocking(move || {
                    certificates::import(&store.dir, &input.certificate_pem, &input.private_key_pem)
                })
                .await
                .map_err(|_| internal())?
                .map_err(invalid)?;
                Ok(json!({"identity": result, "revision": input.revision}))
            })
            .await
            .map_err(|_| internal())?
        }
        ("POST", "/api/cache/inspect") => {
            let input: cache::Inspect = decode(body)?;
            let (query, subnet) = input.prepare().map_err(invalid)?;
            let manager = shared.manager.lock().await;
            let resolver = manager
                .resolver()
                .ok_or_else(|| error(StatusCode::CONFLICT, "DNS_STOPPED", "DNS is not running"))?;
            let cache = resolver.cache();
            let now = Instant::now();
            let name = query.queries[0].name().to_ascii();
            Ok(json!({
                "revision": manager.saved.as_ref().map_or(0, |s| s.revision),
                "epoch": cache.epoch(),
                "inspection": cache.inspect(&name, Some(query.queries[0].query_type()), now),
                "explanation": cache.explain(&query, subnet, now),
            }))
        }
        ("POST", "/api/cache/invalidate") => {
            let input: cache::Invalidate = decode(body)?;
            let (name, kind, scope) = input.selection().map_err(invalid)?;
            let _permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            let manager = shared.manager.lock().await;
            if manager.saved.as_ref().map(|s| s.revision) != Some(input.revision) {
                return Err(error(
                    StatusCode::CONFLICT,
                    "REVISION",
                    "Configuration changed; refresh before invalidating",
                ));
            }
            let resolver = manager
                .resolver()
                .ok_or_else(|| error(StatusCode::CONFLICT, "DNS_STOPPED", "DNS is not running"))?;
            let cache = resolver.cache();
            if cache.epoch() != input.epoch {
                return Err(error(
                    StatusCode::CONFLICT,
                    "CACHE_EPOCH",
                    "Cache changed; refresh before invalidating",
                ));
            }
            let removed = cache.invalidate(name.as_deref(), kind, scope);
            Ok(json!({"removed":removed,"epoch":cache.epoch(),"revision":input.revision}))
        }
        ("GET", "/api/config") => {
            let manager = shared.manager.lock().await;
            let saved = manager.saved.as_ref().ok_or_else(internal)?;
            Ok(
                json!({"toml":saved.toml,"revision":saved.revision,"has_backup":saved.previous.is_some()}),
            )
        }
        ("POST", "/api/config/parse") => {
            let document: Document = decode(body)?;
            tokio::task::spawn_blocking(move || settings::parse(&document.toml))
                .await
                .map_err(|_| internal())?
                .map_err(invalid)
        }
        ("POST", "/api/config/preview") => {
            let request: settings::Preview = decode(body)?;
            let mut preview = tokio::task::spawn_blocking(move || settings::preview(request))
                .await
                .map_err(|_| internal())?
                .map_err(invalid)?;
            let manager = shared.manager.lock().await;
            preview["transport_change"] = manager
                .transport_change(preview["toml"].as_str().expect("preview TOML"), host)
                .map_err(invalid)?;
            Ok(preview)
        }
        ("POST", "/api/config/validate") => {
            let document: Document = decode(body)?;
            let manager = shared.manager.lock().await;
            let restart_required = manager.hot_update(&document.toml).is_none();
            let change = manager
                .transport_change(&document.toml, host)
                .map_err(invalid)?;
            manager.validate(document.toml).await.map_err(invalid)?;
            Ok(json!({"valid":true,"restart_required":restart_required,"transport_change":change}))
        }
        ("POST", "/api/config/rollback/preview") => {
            let input: Revision = decode(body)?;
            let manager = shared.manager.lock().await;
            let saved = manager.saved.as_ref().ok_or_else(internal)?;
            if saved.revision != input.revision {
                return Err(error(
                    StatusCode::CONFLICT,
                    "REVISION",
                    "Configuration changed; reload before restoring",
                ));
            }
            let previous = saved.previous.as_ref().ok_or_else(|| {
                error(
                    StatusCode::CONFLICT,
                    "NO_BACKUP",
                    "No previous configuration",
                )
            })?;
            let change = manager.transport_change(previous, host).map_err(invalid)?;
            Ok(json!({"revision":input.revision,"transport_change":change}))
        }
        ("PUT", "/api/config") | ("POST", "/api/config/rollback") => {
            let (toml, revision, allow_http_downgrade) = if path.ends_with("rollback") {
                let r: Revision = decode(body)?;
                (None, r.revision, r.allow_http_downgrade)
            } else {
                let r: Change = decode(body)?;
                (Some(r.toml), r.revision, r.allow_http_downgrade)
            };
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
            let host = host.to_owned();
            tokio::spawn(async move {
                let _permit = permit;
                let mut manager = shared.manager.lock().await;
                let saved = manager.saved.as_ref().ok_or_else(internal)?;
                if revision != saved.revision {
                    return Err(error(
                        StatusCode::CONFLICT,
                        "REVISION",
                        "Configuration changed; reload before saving",
                    ));
                }
                let toml = toml.or_else(|| saved.previous.clone()).ok_or_else(|| {
                    error(
                        StatusCode::CONFLICT,
                        "NO_BACKUP",
                        "No previous configuration",
                    )
                })?;
                let change = manager.transport_change(&toml, &host).map_err(invalid)?;
                if change["requires_http_confirmation"] == true && !allow_http_downgrade {
                    return Err(error(StatusCode::CONFLICT, "HTTP_DOWNGRADE_CONFIRMATION_REQUIRED", "Confirm switching management to HTTP"));
                }
                let next = Stored {
                    toml,
                    previous: Some(saved.toml.clone()),
                    revision: saved.revision.checked_add(1).ok_or_else(internal)?,
                    ..saved.clone()
                };
                let revision = next.revision;
                let restarted = manager.apply(next).await.map_err(invalid)?;
                Ok(json!({"revision":revision,"restart_required":restarted,"transport_change":change}))
            })
            .await
            .map_err(|_| internal())?
        }
        _ => Err(error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "Unknown management endpoint",
        )),
    }
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    Extension(peer): Extension<SocketAddr>,
    Extension(transport): Extension<Arc<Snapshot>>,
    request: Request<Body>,
) -> Response {
    let mut response = handle_inner(shared, transport, peer, request)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            value.parse().unwrap(),
        );
    }
    response
}

async fn handle_inner(
    shared: Arc<Shared>,
    transport: Arc<Snapshot>,
    peer: SocketAddr,
    request: Request<Body>,
) -> std::result::Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let hosts = parts.headers.get_all(header::HOST);
    let mut hosts = hosts.iter();
    let host = hosts.next().and_then(|h| h.to_str().ok()).unwrap_or("");
    let unsafe_method = parts.method != "GET" && parts.method != "HEAD";
    shared.ensure_current(&transport)?;
    if !host_allowed(shared.address, host, &transport)
        || hosts.next().is_some()
        || !same_origin(&parts.headers, host, unsafe_method, transport.scheme)
        || parts.headers.get_all("sec-fetch-site").iter().count() > 1
        || parts.headers.get("sec-fetch-site").is_some_and(|site| {
            if unsafe_method {
                site != "same-origin"
            } else {
                site == "cross-site"
            }
        })
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "ORIGIN",
            "Unexpected Host or Origin",
        ));
    }
    let path = parts.uri.path();
    if parts.method == "GET" {
        let asset = match path {
            "/" => Some((
                "text/html; charset=utf-8",
                include_str!("../../web/dist/index.html"),
            )),
            "/assets/app.css" => Some((
                "text/css; charset=utf-8",
                include_str!("../../web/dist/assets/app.css"),
            )),
            "/assets/app.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../../web/dist/assets/app.js"),
            )),
            "/theme-init.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../../web/dist/theme-init.js"),
            )),
            _ => None,
        };
        if let Some((kind, text)) = asset {
            return Ok(([("content-type", kind)], text).into_response());
        }
    }
    if !path.starts_with("/api/") {
        return Err(error(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found"));
    }
    if parts.headers.contains_key(header::AUTHORIZATION) {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Sign in required",
        ));
    }
    if unsafe_method
        && !parts
            .headers
            .get("content-type")
            .is_some_and(|v| v.to_str().unwrap_or("").split(';').next() == Some("application/json"))
    {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "JSON_REQUIRED",
            "Use application/json",
        ));
    }
    let bytes = to_bytes(body, BODY_LIMIT).await.map_err(|_| {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BODY_LIMIT",
            "Request body is too large or incomplete",
        )
    })?;
    let mut cookie = None;
    let mut response = axum::Json(
        api(
            shared,
            transport.clone(),
            ApiRequest {
                peer,
                method: parts.method.as_str(),
                path: parts
                    .uri
                    .path_and_query()
                    .map_or(path, |value| value.as_str()),
                headers: &parts.headers,
                body: &bytes,
            },
            &mut cookie,
        )
        .await?,
    )
    .into_response();
    let cookie = match cookie {
        Some(token) => {
            let (name, attributes) = if transport.scheme == Scheme::Https {
                (HTTPS_COOKIE, "Path=/; HttpOnly; Secure; SameSite=Strict")
            } else {
                (HTTP_COOKIE, "Path=/; HttpOnly; SameSite=Strict")
            };
            format!(
                "{name}={token}; {attributes}; Max-Age={}",
                SESSION_LIFETIME.as_secs()
            )
        }
        None => return Ok(response),
    };
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    Ok(response)
}

/// Bind the management listener before starting any saved DNS configuration.
pub async fn serve(
    directory: &Path,
    address: SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let store = Store::open(directory)?;
    if store.read()?.is_none() {
        store.setup_token()?;
        eprintln!(
            "PariNS setup token file: {}",
            store.dir.join("setup-token").display()
        );
    }
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: Vec::new(),
    }));
    let manager = Arc::new(tokio::sync::Mutex::new(
        Manager::open(store, address, active.clone()).await?,
    ));
    let shared = Arc::new(Shared {
        manager: manager.clone(),
        active: active.clone(),
        auth: auth_budget::Budget::new(),
        mutation: Arc::new(Semaphore::new(1)),
        address,
    });
    let router = Router::new().fallback(handle).with_state(shared.clone());
    let initial = active.lock().unwrap().snapshot.clone();
    if let Some(origin) = &initial.origin {
        eprintln!("PariNS management: {origin}/");
    } else if address.ip().is_unspecified() {
        eprintln!(
            "PariNS management: HTTP listening on {address}; use a concrete server IP with this port"
        );
    } else {
        eprintln!("PariNS management: http://{address}/");
    }
    if !address.ip().is_loopback() {
        if initial.scheme == Scheme::Http {
            eprintln!(
                "Management HTTP is network-accessible. Enter credentials locally or over an SSH tunnel; use a concrete IP or configured public_host, not the wildcard address."
            );
        } else {
            eprintln!("Management HTTPS is network-accessible. Restrict access to administrators.");
        }
    }
    let permits = Arc::new(Semaphore::new(32));
    let mut tasks = JoinSet::new();
    let mut reload_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut reload_pending = false;
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            _ = async { #[cfg(unix)] { hangup.recv().await; } #[cfg(not(unix))] { std::future::pending::<()>().await; } } => {
                if reload_task.is_some() { reload_pending = true; }
                else { reload_task = Some(start_signal_reload(shared.clone())); }
            },
            _ = async { if let Some(task) = &mut reload_task { let _ = task.await; } else { std::future::pending::<()>().await; } }, if reload_task.is_some() => {
                reload_task = None;
                if reload_pending { reload_pending = false; reload_task = Some(start_signal_reload(shared.clone())); }
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = listener.accept() => {
                let (stream,peer) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => break Err(error.into()),
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let snapshot = active.lock().unwrap().snapshot.clone();
                let service = TowerToHyperService::new(router.clone().layer(Extension(peer)).layer(Extension(snapshot.clone())));
                tasks.spawn(async move {
                    let _permit = permit;
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false).max_headers(32).timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5));
                    if let Some(tls) = &snapshot.tls {
                        let acceptor = tokio_rustls::TlsAcceptor::from(tls.clone());
                        let Ok(Ok(stream)) = timeout(Duration::from_secs(5), acceptor.accept(stream)).await else { return; };
                        let _ = timeout(Duration::from_secs(90),builder.serve_connection(TokioIo::new(stream),service)).await;
                    } else {
                        let _ = timeout(Duration::from_secs(90),builder.serve_connection(TokioIo::new(stream),service)).await;
                    }
                });
            }
        }
    };
    drop(listener);
    // A submitted mutation owns its transaction independently of its HTTP task.
    tasks.shutdown().await;
    // Every accepted transaction owns this permit before detaching. Wait for it
    // before stopping DNS, including transactions not yet holding manager.lock.
    // A blocked filesystem must not keep the process alive indefinitely. On
    // timeout, leave no clean snapshot and let the process runtime terminate.
    let _transaction = timeout(Duration::from_secs(65), async {
        if let Some(task) = reload_task {
            let _ = task.await;
        }
        shared.mutation.acquire().await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "management transaction did not drain; shutting down without a clean cache snapshot"
        )
    })??;
    manager.lock().await.terminal_shutdown().await;
    result
}

fn start_signal_reload(shared: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(_permit) = shared.mutation.acquire().await else {
            return;
        };
        match shared
            .manager
            .lock()
            .await
            .reload_certificates("signal")
            .await
        {
            Ok(result) => eprintln!("PariNS certificate reload: {}", result["outcome"]),
            Err(_) => eprintln!(
                "PariNS certificate reload failed; previous identities retained (see authenticated status)"
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn session_capacity_and_late_logout_do_not_revoke_other_logins() {
        use axum::http::{HeaderMap, HeaderValue, header};
        use std::sync::{Arc, Mutex};
        let temp = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(Mutex::new(super::Active {
            snapshot: Arc::new(super::Snapshot::initial()),
            sessions: Vec::new(),
        }));
        let manager = Arc::new(tokio::sync::Mutex::new(
            super::Manager::open(
                super::Store::open(&temp.path().join("state")).unwrap(),
                address,
                active.clone(),
            )
            .await
            .unwrap(),
        ));
        let shared = super::Shared {
            manager,
            active: active.clone(),
            auth: super::auth_budget::Budget::new(),
            mutation: Arc::new(tokio::sync::Semaphore::new(1)),
            address,
        };
        let current = active.lock().unwrap().snapshot.clone();
        let first = shared.session(&current).unwrap();
        let second = shared.session(&current).unwrap();
        let mut first_headers = HeaderMap::new();
        first_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("parins_session_http={}", first.token)).unwrap(),
        );
        first_headers.insert(
            "x-parins-session",
            HeaderValue::from_str(&first.binding).unwrap(),
        );
        shared.revoke_session(&first_headers, &current).unwrap();
        let mut second_headers = HeaderMap::new();
        second_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("parins_session_http={}", second.token)).unwrap(),
        );
        assert!(
            shared
                .find_session(&second_headers, &current)
                .unwrap()
                .is_some()
        );
        for _ in 0..16 {
            shared.session(&current).unwrap();
        }
        assert_eq!(active.lock().unwrap().sessions.len(), 16);
        assert!(
            shared
                .find_session(&second_headers, &current)
                .unwrap()
                .is_none()
        );
        let newest = active.lock().unwrap().sessions.last().unwrap().clone();
        let mut newest_headers = HeaderMap::new();
        newest_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("parins_session_http={}", newest.token)).unwrap(),
        );
        shared.revoke_session(&first_headers, &current).unwrap();
        assert!(
            shared
                .find_session(&newest_headers, &current)
                .unwrap()
                .is_some()
        );
        newest_headers.insert("x-parins-session", HeaderValue::from_static("old-binding"));
        assert_eq!(
            shared
                .revoke_session(&newest_headers, &current)
                .unwrap_err()
                .1,
            "SESSION_CHANGED"
        );
        assert!(
            shared
                .find_session(&newest_headers, &current)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn expired_cookie_logout_is_idempotent_without_binding() {
        use axum::http::{HeaderMap, HeaderValue, header};
        use std::{
            sync::{Arc, Mutex},
            time::{Duration, Instant},
        };
        let temp = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(Mutex::new(super::Active {
            snapshot: Arc::new(super::Snapshot::initial()),
            sessions: vec![super::Session {
                token: "expired".into(),
                binding: "old-binding".into(),
                created: Instant::now() - Duration::from_secs(8 * 3600 + 1),
            }],
        }));
        let manager = Arc::new(tokio::sync::Mutex::new(
            super::Manager::open(
                super::Store::open(&temp.path().join("state")).unwrap(),
                address,
                active.clone(),
            )
            .await
            .unwrap(),
        ));
        let shared = super::Shared {
            manager,
            active: active.clone(),
            auth: super::auth_budget::Budget::new(),
            mutation: Arc::new(tokio::sync::Semaphore::new(1)),
            address,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("parins_session_http=expired"),
        );
        let current = active.lock().unwrap().snapshot.clone();
        shared.revoke_session(&headers, &current).unwrap();
        assert!(active.lock().unwrap().sessions.is_empty());
    }

    #[tokio::test]
    async fn session_waiting_on_manager_rejects_replaced_realm() {
        use axum::http::HeaderMap;
        use std::{
            sync::{Arc, Mutex},
            task::Poll,
        };
        let temp = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(Mutex::new(super::Active {
            snapshot: Arc::new(super::Snapshot::initial()),
            sessions: Vec::new(),
        }));
        let manager = Arc::new(tokio::sync::Mutex::new(
            super::Manager::open(
                super::Store::open(&temp.path().join("state")).unwrap(),
                address,
                active.clone(),
            )
            .await
            .unwrap(),
        ));
        let shared = Arc::new(super::Shared {
            manager: manager.clone(),
            active: active.clone(),
            auth: super::auth_budget::Budget::new(),
            mutation: Arc::new(tokio::sync::Semaphore::new(1)),
            address,
        });
        let old = active.lock().unwrap().snapshot.clone();
        let headers = HeaderMap::new();
        let mut cookie = None;
        let guard = manager.lock().await;
        let future = super::api(
            shared.clone(),
            old.clone(),
            super::ApiRequest {
                peer: address,
                method: "GET",
                path: "/api/session",
                headers: &headers,
                body: &[],
            },
            &mut cookie,
        );
        tokio::pin!(future);
        assert!(matches!(
            futures_util::poll!(future.as_mut()),
            Poll::Pending
        ));
        let mut replacement = super::Snapshot::initial();
        replacement.realm = 1;
        active.lock().unwrap().snapshot = Arc::new(replacement);
        drop(guard);
        let error = future.await.unwrap_err();
        assert_eq!(error.1, "TRANSPORT_CHANGED");
        assert!(matches!(
            shared.session(&old),
            Err(super::ApiError(_, "TRANSPORT_CHANGED", _))
        ));
    }

    #[test]
    fn origin_comparison_normalizes_default_https_port_and_ipv6() {
        use axum::http::{HeaderMap, HeaderValue, header};
        for (host, origin) in [
            ("localhost", "https://localhost:443"),
            ("localhost:443", "https://localhost"),
            ("127.0.0.1", "https://127.0.0.1:443"),
            ("[::1]", "https://[::1]:443"),
            ("[2001:db8::1]:3000", "https://[2001:0db8::1]:3000"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            assert!(
                super::same_origin(&headers, host, true, super::Scheme::Https),
                "{host} {origin}"
            );
        }
        let mut headers = HeaderMap::new();
        for origin in [
            "null",
            "http://localhost:443",
            "https://localhost:444",
            "https://localhost.evil:443",
            "https://[::2]:443",
            "https://::1",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            assert!(
                !super::same_origin(&headers, "localhost", true, super::Scheme::Https),
                "{origin}"
            );
        }
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://::1"));
        assert!(!super::same_origin(
            &headers,
            "[::1]",
            true,
            super::Scheme::Https
        ));
        headers.remove(header::ORIGIN);
        assert!(!super::same_origin(
            &headers,
            "localhost",
            true,
            super::Scheme::Https
        ));
    }

    #[test]
    fn host_allowlist_includes_bound_ip_and_https_default_port() {
        let transport = super::Snapshot::initial();
        let address = "127.0.0.2:80".parse().unwrap();
        for host in ["127.0.0.2", "127.0.0.2:80", "localhost", "[::1]:80"] {
            assert!(super::host_allowed(address, host, &transport));
        }
        let address = "[::1]:3000".parse().unwrap();
        assert!(super::host_allowed(address, "[::1]:3000", &transport));
        assert!(!super::host_allowed(address, "localhost", &transport));
        assert!(!super::host_allowed(
            address,
            "attacker.test:3000",
            &transport
        ));
        assert!(!super::host_allowed(
            address,
            "203.0.113.10:3000",
            &transport
        ));
    }

    #[test]
    fn public_listener_accepts_literal_ipv4_ipv6_but_not_domains_or_other_ports() {
        let transport = super::Snapshot::initial();
        for address in ["0.0.0.0:3000", "[::]:3000", "10.0.0.1:3000"] {
            let address = address.parse().unwrap();
            for host in ["203.0.113.10:3000", "[2001:db8::10]:3000", "127.0.0.1:3000"] {
                assert!(
                    super::host_allowed(address, host, &transport),
                    "{address} {host}"
                );
            }
            for host in [
                "attacker.test:3000",
                "203.0.113.10",
                "203.0.113.10:3001",
                "user@203.0.113.10:3000",
                "0.0.0.0:3000",
                "[::]:3000",
                "224.0.0.1:3000",
            ] {
                assert!(
                    !super::host_allowed(address, host, &transport),
                    "{address} {host}"
                );
            }
        }
        let address = "0.0.0.0:80".parse().unwrap();
        assert!(super::host_allowed(address, "203.0.113.10", &transport));
        assert!(super::host_allowed(address, "[2001:db8::10]", &transport));
    }
}
