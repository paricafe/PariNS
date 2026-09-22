//! Authenticated management plane. DNS forwarding remains in Server/Resolver.
mod https;
mod runtime;
mod store;

use anyhow::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
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
    collections::VecDeque,
    future::Future,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use store::{Store, Stored};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};

const BODY_LIMIT: usize = 300 * 1024;
struct Session {
    token: String,
    created: Instant,
}
struct Shared {
    manager: Arc<tokio::sync::Mutex<Manager>>,
    sessions: Mutex<Vec<Session>>,
    logins: Mutex<VecDeque<Instant>>,
    mutation: Arc<Semaphore>,
    address: SocketAddr,
}

fn host_allowed(address: SocketAddr, host: &str) -> bool {
    if host == format!("localhost:{}", address.port())
        || address.port() == 443 && host == "localhost"
    {
        return true;
    }
    // Literal IPs support both directly assigned addresses and public-IP NAT.
    // Arbitrary domain names remain rejected to prevent DNS rebinding. The Host
    // header is not an identity: setup tokens and bearer auth are still required.
    let target = host.parse::<SocketAddr>().ok().or_else(|| {
        if address.port() != 443 {
            return None;
        }
        let ip = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(host);
        ip.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 443))
    });
    target.is_some_and(|target| {
        target.port() == address.port()
            && !target.ip().is_unspecified()
            && !target.ip().is_multicast()
            && (!address.ip().is_loopback() || target.ip().is_loopback())
    })
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
fn invalid(err: impl std::fmt::Display) -> ApiError {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "INVALID_CONFIG",
        format!("{err:#}"),
    )
}
fn internal() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Management operation failed",
    )
}
fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> std::result::Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "BAD_JSON", "Invalid JSON request"))
}

impl Shared {
    fn login_budget(&self) -> std::result::Result<(), ApiError> {
        let now = Instant::now();
        let mut attempts = self.logins.lock().unwrap();
        while attempts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= 5 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "LOGIN_LIMIT",
                "Try again in one minute",
            ));
        }
        attempts.push_back(now);
        Ok(())
    }

    fn session(&self) -> String {
        let token = store::secret();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|s| s.created.elapsed() < Duration::from_secs(8 * 3600));
        if sessions.len() == 16 {
            sessions.remove(0);
        }
        sessions.push(Session {
            token: token.clone(),
            created: Instant::now(),
        });
        token
    }

    fn authorized(&self, headers: &HeaderMap) -> std::result::Result<String, ApiError> {
        let supplied = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        let sessions = self.sessions.lock().unwrap();
        if sessions
            .iter()
            .any(|s| s.token == supplied && s.created.elapsed() < Duration::from_secs(8 * 3600))
        {
            Ok(supplied.to_owned())
        } else {
            Err(error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Sign in required",
            ))
        }
    }
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
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: u64,
}

async fn api(
    shared: Arc<Shared>,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> std::result::Result<Value, ApiError> {
    match (method, path) {
        ("GET", "/api/session") => {
            return Ok(json!({"setup_required":shared.manager.lock().await.saved.is_none()}));
        }
        ("GET", "/api/template") => {
            return Ok(json!({"toml":include_str!("../../parins.example.toml")}));
        }
        ("POST", "/api/login") => {
            shared.login_budget()?;
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
            let valid = tokio::task::spawn_blocking(move || {
                let valid_password =
                    store::verify_password(&saved.password_hash, &credentials.password);
                valid_password && saved.username == credentials.username
            })
            .await
            .map_err(|_| internal())?;
            if !valid {
                return Err(error(
                    StatusCode::UNAUTHORIZED,
                    "LOGIN_FAILED",
                    "Invalid credentials",
                ));
            }
            return Ok(json!({"token":shared.session()}));
        }
        ("POST", "/api/setup") => {
            shared.login_budget()?;
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
            return tokio::spawn(async move {
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
                    return Err(invalid(
                        "Username must be 1..64 ASCII letters, digits, hyphen or underscore",
                    ));
                }
                let hash =
                    tokio::task::spawn_blocking(move || store::hash_password(&setup.password))
                        .await
                        .map_err(|_| internal())?
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
                Ok(json!({"token":control.session()}))
            })
            .await
            .map_err(|_| internal())?;
        }
        _ => {}
    }
    let token = shared.authorized(headers)?;
    match (method, path) {
        ("POST", "/api/logout") => {
            shared.sessions.lock().unwrap().retain(|s| s.token != token);
            Ok(json!({"ok":true}))
        }
        ("GET", "/api/status") => Ok(shared.manager.lock().await.status()),
        ("GET", "/api/config") => {
            let manager = shared.manager.lock().await;
            let saved = manager.saved.as_ref().ok_or_else(internal)?;
            Ok(
                json!({"toml":saved.toml,"revision":saved.revision,"has_backup":saved.previous.is_some()}),
            )
        }
        ("POST", "/api/config/validate") => {
            let document: Document = decode(body)?;
            shared
                .manager
                .lock()
                .await
                .validate(document.toml)
                .await
                .map_err(invalid)?;
            Ok(json!({"valid":true,"restart_required":true}))
        }
        ("PUT", "/api/config") | ("POST", "/api/config/rollback") => {
            let (toml, revision) = if path.ends_with("rollback") {
                let r: Revision = decode(body)?;
                (None, r.revision)
            } else {
                let r: Change = decode(body)?;
                (Some(r.toml), r.revision)
            };
            let permit =
                shared.mutation.clone().try_acquire_owned().map_err(|_| {
                    error(StatusCode::CONFLICT, "BUSY", "Another change is running")
                })?;
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
                let next = Stored {
                    toml,
                    previous: Some(saved.toml.clone()),
                    revision: saved.revision.checked_add(1).ok_or_else(internal)?,
                    ..saved.clone()
                };
                let revision = next.revision;
                manager.apply(next).await.map_err(invalid)?;
                Ok(json!({"revision":revision,"restart_required":true}))
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

async fn handle(State(shared): State<Arc<Shared>>, request: Request<Body>) -> Response {
    let mut response = handle_inner(shared, request)
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
    request: Request<Body>,
) -> std::result::Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let host = parts
        .headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_allowed(shared.address, host)
        || parts
            .headers
            .get("origin")
            .is_some_and(|h| h.to_str().ok() != Some(format!("https://{host}").as_str()))
        || parts
            .headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
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
                include_str!("../../web/index.html"),
            )),
            "/app.css" => Some(("text/css; charset=utf-8", include_str!("../../web/app.css"))),
            "/app.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../../web/app.js"),
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
    if parts.method != "GET"
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
    Ok(
        axum::Json(api(shared, parts.method.as_str(), path, &parts.headers, &bytes).await?)
            .into_response(),
    )
}

/// Bind the management listener before starting any saved DNS configuration.
pub async fn serve(
    directory: &Path,
    address: SocketAddr,
    tls_files: Option<crate::tls::TlsFiles>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let store = Store::open(directory)?;
    let acceptor =
        tokio_rustls::TlsAcceptor::from(https::config(&store, tls_files.as_ref(), address)?);
    if tls_files.is_none() {
        eprintln!(
            "PariNS self-signed management certificate: {} (verify its fingerprint before trusting)",
            store.dir.join("https-cert.pem").display()
        );
    }
    if store.read()?.is_none() {
        store.setup_token()?;
        eprintln!(
            "PariNS setup token file: {}",
            store.dir.join("setup-token").display()
        );
    }
    let manager = Arc::new(tokio::sync::Mutex::new(Manager::open(store).await?));
    let shared = Arc::new(Shared {
        manager: manager.clone(),
        sessions: Mutex::new(Vec::new()),
        logins: Mutex::new(VecDeque::new()),
        mutation: Arc::new(Semaphore::new(1)),
        address,
    });
    let router = Router::new().fallback(handle).with_state(shared.clone());
    eprintln!("PariNS management: https://{address}");
    if !address.ip().is_loopback() {
        eprintln!(
            "Management HTTPS is network-accessible. Use the server IP, not the wildcard address, in your browser; allow the port in your firewall/security group for intended clients."
        );
    }
    let permits = Arc::new(Semaphore::new(32));
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = listener.accept() => {
                let (stream,_) = match incoming {
                    Ok(incoming) => incoming,
                    Err(error) => break Err(error.into()),
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let service = TowerToHyperService::new(router.clone());
                let acceptor = acceptor.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(stream)) = timeout(Duration::from_secs(5), acceptor.accept(stream)).await else { return; };
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false).max_headers(32).timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5));
                    let _ = timeout(Duration::from_secs(90),builder.serve_connection(TokioIo::new(stream),service)).await;
                });
            }
        }
    };
    drop(listener);
    // A submitted mutation owns its transaction independently of its HTTP task.
    tasks.shutdown().await;
    // Every accepted transaction owns this permit before detaching. Wait for it
    // before stopping DNS, including transactions not yet holding manager.lock.
    let _transaction = shared.mutation.acquire().await?;
    manager.lock().await.stop().await;
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_allowlist_includes_bound_ip_and_https_default_port() {
        let address = "127.0.0.2:443".parse().unwrap();
        for host in ["127.0.0.2", "127.0.0.2:443", "localhost", "[::1]:443"] {
            assert!(super::host_allowed(address, host));
        }
        let address = "[::1]:3000".parse().unwrap();
        assert!(super::host_allowed(address, "[::1]:3000"));
        assert!(!super::host_allowed(address, "localhost"));
        assert!(!super::host_allowed(address, "attacker.test:3000"));
        assert!(!super::host_allowed(address, "203.0.113.10:3000"));
    }

    #[test]
    fn public_listener_accepts_literal_ipv4_ipv6_but_not_domains_or_other_ports() {
        for address in ["0.0.0.0:3000", "[::]:3000", "10.0.0.1:3000"] {
            let address = address.parse().unwrap();
            for host in ["203.0.113.10:3000", "[2001:db8::10]:3000", "127.0.0.1:3000"] {
                assert!(super::host_allowed(address, host), "{address} {host}");
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
                assert!(!super::host_allowed(address, host), "{address} {host}");
            }
        }
        let address = "0.0.0.0:443".parse().unwrap();
        assert!(super::host_allowed(address, "203.0.113.10"));
        assert!(super::host_allowed(address, "[2001:db8::10]"));
    }
}
