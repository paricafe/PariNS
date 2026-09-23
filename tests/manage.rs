use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{manage, protocol};
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, ServerName},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_rustls::TlsConnector;

const PASSWORD: &str = "local-integration-password";
const DEADLINE: Duration = Duration::from_secs(10);

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON response")
    }

    fn expect(&self, status: u16) -> Value {
        assert_eq!(self.status, status, "{}", self.body);
        self.json()
    }

    fn auth(&self) -> String {
        let cookie = self
            .headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("set-cookie: parins_session_http=")
                    .or_else(|| line.strip_prefix("set-cookie: __host-parins_session="))
            })
            .expect("response sets session Cookie")
            .split(';')
            .next()
            .unwrap();
        let binding = self.json()["session"]["binding"]
            .as_str()
            .unwrap()
            .to_owned();
        format!("{cookie}|{binding}")
    }
}

struct Management {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Management {
    async fn start(directory: &Path) -> Self {
        // The public entry point binds its own listener, so reserve an ephemeral
        // address briefly before handing it to serve. Every test is isolated.
        let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::start_reserved(directory, reservation).await
    }

    async fn start_reserved(directory: &Path, reservation: TcpListener) -> Self {
        let listen = reservation.local_addr().unwrap();
        let address = if listen.ip().is_unspecified() {
            SocketAddr::new(
                if listen.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                },
                listen.port(),
            )
        } else {
            listen
        };
        drop(reservation);
        let directory = directory.to_owned();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            manage::serve(&directory, listen, async {
                let _ = stopped.await;
            })
            .await
        });
        timeout(DEADLINE, async {
            loop {
                if TcpStream::connect(address).await.is_ok() {
                    break;
                }
                assert!(!task.is_finished(), "management exited before binding");
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let server = Self {
            address,
            stop: Some(stop),
            task: Some(task),
        };
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200);
        server
    }

    fn wire(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> Vec<u8> {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.address,
            body.len()
        );
        if method != "GET" {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Origin: http://{}\r\n", self.address));
        }
        if let Some(token) = token {
            let (cookie, binding) = token.split_once('|').expect("Cookie and binding fixture");
            request.push_str(&format!(
                "Cookie: parins_session_http={cookie}\r\nX-PariNS-Session: {binding}\r\n"
            ));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        request.into_bytes()
    }

    async fn raw(&self, bytes: &[u8]) -> Response {
        self.raw_at(self.address, bytes).await
    }

    async fn raw_at(&self, address: SocketAddr, bytes: &[u8]) -> Response {
        timeout(DEADLINE, async {
            let mut stream = self.connect_at(address).await;
            stream.write_all(bytes).await.unwrap();
            stream.flush().await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            parse_response(&response)
        })
        .await
        .expect("HTTP request deadline")
    }

    async fn connect(&self) -> TcpStream {
        self.connect_at(self.address).await
    }

    async fn connect_at(&self, address: SocketAddr) -> TcpStream {
        TcpStream::connect(address).await.unwrap()
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        self.raw(&self.wire(method, path, token, body)).await
    }

    async fn setup(&self, directory: &Path, toml: &str) -> String {
        let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
        let response = self.setup_with(&token, toml).await;
        response.expect(200);
        response.auth()
    }

    async fn setup_with(&self, token: &str, toml: &str) -> Response {
        let wire = String::from_utf8(self.wire(
            "POST",
            "/api/setup",
            None,
            Some(json!({"username":"admin","password":PASSWORD,"toml":toml})),
        ))
        .unwrap()
        .replacen(
            "\r\n\r\n",
            &format!("\r\nX-PariNS-Setup: {token}\r\n\r\n"),
            1,
        );
        self.raw(wire.as_bytes()).await
    }

    async fn config(&self, token: &str) -> Value {
        self.request("GET", "/api/config", Some(token), None)
            .await
            .expect(200)
    }

    async fn dns(&self, token: &str) -> SocketAddr {
        let status = self
            .request("GET", "/api/status", Some(token), None)
            .await
            .expect(200);
        assert_eq!(status["running"], true, "{status}");
        status["listen"].as_str().unwrap().parse().unwrap()
    }

    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(DEADLINE, self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let _rebound = TcpListener::bind(self.address).await.unwrap();
    }
}

fn parse_response(response: &str) -> Response {
    let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Response {
        status,
        headers: headers.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

fn connector(roots: RootCertStore, alpn: &[u8]) -> TlsConnector {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    TlsConnector::from(Arc::new(config))
}

async fn trusted_tls(
    address: SocketAddr,
    certificate: &rcgen::CertifiedKey<rcgen::KeyPair>,
    name: &str,
    alpn: &[u8],
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(certificate.cert.der().to_vec()))
        .unwrap();
    connector(roots, alpn)
        .connect(
            ServerName::try_from(name.to_owned()).unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap()
}

async fn secure_request(
    address: SocketAddr,
    certificate: &rcgen::CertifiedKey<rcgen::KeyPair>,
    name: &str,
    wire: &str,
) -> Response {
    let mut stream = trusted_tls(address, certificate, name, b"http/1.1").await;
    assert_eq!(
        stream.get_ref().1.alpn_protocol(),
        Some(b"http/1.1".as_slice())
    );
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    parse_response(&response)
}

fn https_wire(
    address: SocketAddr,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> String {
    let body = body.map(|value| value.to_string()).unwrap_or_default();
    let authority = format!("{host}:{}", address.port());
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if method != "GET" {
        wire.push_str(&format!(
            "Origin: https://{authority}\r\nContent-Type: application/json\r\n"
        ));
    }
    if let Some(token) = token {
        let (cookie, binding) = token.split_once('|').unwrap();
        wire.push_str(&format!(
            "Cookie: __Host-parins_session={cookie}\r\nX-PariNS-Session: {binding}\r\n"
        ));
    }
    wire.push_str("\r\n");
    wire.push_str(&body);
    wire
}

impl Drop for Management {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn configuration() -> String {
    "listen = \"127.0.0.1:0\"\nquery_timeout_ms = 200\ntcp_io_timeout_ms = 500\nshutdown_grace_ms = 200\nmax_inflight = 16\nmax_tcp_connections = 8\n[upstreams]\nservers = [\"127.0.0.1:9\"]\n[filter]\nenabled = true\nblock_exact = [\"example.test\"]\n".to_owned()
}

fn query(name: &str) -> Message {
    let mut message = Message::new(731, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    message
}

async fn assert_dns(address: SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let request = query("example.test.");
    socket
        .send_to(&request.to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut bytes = [0; 4096];
    let length = timeout(DEADLINE, socket.recv(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response = protocol::decode(&bytes[..length]).unwrap();
    assert_eq!(response.id, request.id);
    assert_eq!(response.queries, request.queries);
    assert_eq!(response.response_code, ResponseCode::NoError);
    assert!(response.answers.is_empty());
}

#[tokio::test]
async fn bootstrap_requires_token_and_private_api_rejects_cross_origin_and_wrong_host() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    assert_eq!(
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200)["setup_required"],
        true
    );
    for (path, content_type) in [
        ("/", "text/html"),
        ("/assets/app.js", "text/javascript"),
        ("/assets/app.css", "text/css"),
        ("/theme-init.js", "text/javascript"),
    ] {
        let response = server.request("GET", path, None, None).await;
        assert_eq!(response.status, 200);
        assert!(response.headers.contains(content_type));
        assert!(response.headers.contains("cache-control: no-store"));
        assert!(response.headers.contains("script-src 'self'"));
        assert!(response.headers.contains("x-frame-options: deny"));
    }
    server
        .request("GET", "/api/status", None, None)
        .await
        .expect(401);
    for extra in [
        "Origin: http://attacker.example\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
    ] {
        let request = format!(
            "GET /api/session HTTP/1.1\r\nHost: {}\r\n{extra}Connection: close\r\n\r\n",
            server.address
        );
        server.raw(request.as_bytes()).await.expect(403);
    }
    server
        .raw(b"GET /api/session HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n")
        .await
        .expect(403);
    let same_origin = format!(
        "GET /api/session HTTP/1.1\r\nHost: localhost:{}\r\nOrigin: http://localhost:{}\r\nConnection: close\r\n\r\n",
        server.address.port(),
        server.address.port()
    );
    server.raw(same_origin.as_bytes()).await.expect(200);
    server
        .setup_with("wrong-token", &configuration())
        .await
        .expect(403);
    assert!(!directory.join("state.json").exists());
    let token = server.setup(&directory, &configuration()).await;
    assert_eq!(
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200)["setup_required"],
        false
    );
    assert_dns(server.dns(&token).await).await;
    server
        .setup_with("wrong-token", &configuration())
        .await
        .expect(409);
    server.finish().await;
}

#[tokio::test]
async fn cookie_session_restores_without_a_token_and_binding_prevents_stale_requests() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let setup_token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    let issued = server.setup_with(&setup_token, &configuration()).await;
    let view = issued.expect(200);
    assert_eq!(view["authenticated"], true);
    assert!(view.get("token").is_none());
    assert!(
        issued
            .headers
            .contains("httponly; samesite=strict; max-age=28800")
    );
    assert!(issued.headers.contains("set-cookie: parins_session_http="));
    assert!(!issued.headers.contains("; secure;"));
    assert!(!issued.headers.contains("domain="));
    let auth = issued.auth();
    let (cookie, binding) = auth.split_once('|').unwrap();
    assert_eq!(view["session"]["binding"], binding);
    assert!(view["session"]["expires_in_seconds"].as_u64().unwrap() <= 28800);

    let resume = format!(
        "GET /api/session HTTP/1.1\r\nHost: {}\r\nCookie: parins_session_http={cookie}\r\nConnection: close\r\n\r\n",
        server.address
    );
    let resumed = server.raw(resume.as_bytes()).await;
    assert_eq!(resumed.expect(200)["session"]["binding"], binding);
    assert!(!resumed.headers.contains("set-cookie:"));
    server.config(&auth).await;

    let valid = String::from_utf8(server.wire("GET", "/api/config", Some(&auth), None)).unwrap();
    let changed = valid.replace(
        &format!("X-PariNS-Session: {binding}\r\n"),
        "X-PariNS-Session: stale-binding\r\n",
    );
    let conflict = server.raw(changed.as_bytes()).await;
    assert_eq!(conflict.expect(409)["error"]["code"], "SESSION_CHANGED");
    assert!(!conflict.headers.contains("set-cookie:"));
    let missing = valid.replace(&format!("X-PariNS-Session: {binding}\r\n"), "");
    server.raw(missing.as_bytes()).await.expect(409);

    let bearer = valid.replacen(
        "\r\n\r\n",
        &format!("\r\nAuthorization: Bearer {cookie}\r\n\r\n"),
        1,
    );
    server.raw(bearer.as_bytes()).await.expect(401);
    let changed_logout = String::from_utf8(server.wire("POST", "/api/logout", Some(&auth), None))
        .unwrap()
        .replace(
            &format!("X-PariNS-Session: {binding}\r\n"),
            "X-PariNS-Session: stale-binding\r\n",
        );
    let rejected = server.raw(changed_logout.as_bytes()).await;
    rejected.expect(409);
    assert!(!rejected.headers.contains("set-cookie:"));
    server.config(&auth).await;

    let logout = server
        .request("POST", "/api/logout", Some(&auth), None)
        .await;
    let logged_out = logout.expect(200);
    assert_eq!(logged_out["authenticated"], false);
    assert_eq!(logged_out["session"], Value::Null);
    assert!(!logout.headers.contains("set-cookie:"));
    server
        .request("GET", "/api/config", Some(&auth), None)
        .await
        .expect(401);
    let stale = server.raw(resume.as_bytes()).await;
    assert_eq!(stale.expect(200)["authenticated"], false);
    assert!(!stale.headers.contains("set-cookie:"));
    let repeated = server
        .request("POST", "/api/logout", Some(&auth), None)
        .await;
    assert_eq!(repeated.expect(200)["authenticated"], false);
    assert!(!repeated.headers.contains("set-cookie:"));
    server.finish().await;
}

#[tokio::test]
async fn unsafe_api_requires_exact_origin_and_same_origin_fetch_metadata() {
    let temporary = tempfile::tempdir().unwrap();
    let server = Management::start(&temporary.path().join("state")).await;
    let valid =
        String::from_utf8(server.wire("POST", "/api/config/parse", None, Some(json!({})))).unwrap();
    // A correct origin reaches authentication; malformed or cross-origin
    // requests are rejected before any private API branch executes.
    server.raw(valid.as_bytes()).await.expect(401);
    let origin = format!("Origin: http://{}\r\n", server.address);
    for wire in [
        valid.replace(&origin, ""),
        valid.replace(&origin, "Origin: null\r\n"),
        valid.replace(&origin, "Origin: http://attacker.example\r\n"),
        valid.replace(
            &origin,
            &format!("{origin}Origin: http://{}\r\n", server.address),
        ),
        valid.replace(
            &origin,
            &format!("Origin: http://127.0.0.1:{}1\r\n", server.address.port()),
        ),
        valid.replacen("\r\n\r\n", "\r\nSec-Fetch-Site: same-site\r\n\r\n", 1),
        valid.replacen("\r\n\r\n", "\r\nSec-Fetch-Site: cross-site\r\n\r\n", 1),
    ] {
        server.raw(wire.as_bytes()).await.expect(403);
    }
    let same_origin = valid.replacen("\r\n\r\n", "\r\nSec-Fetch-Site: same-origin\r\n\r\n", 1);
    server.raw(same_origin.as_bytes()).await.expect(401);
    server.finish().await;
}

#[tokio::test]
async fn successful_login_creates_an_independent_session_and_failed_login_preserves_it() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let old = server.setup(&directory, &configuration()).await;
    let failed = server
        .request(
            "POST",
            "/api/login",
            Some(&old),
            Some(json!({"username":"admin","password":"incorrect-password"})),
        )
        .await;
    failed.expect(401);
    assert!(!failed.headers.contains("set-cookie:"));
    server.config(&old).await;

    let login = server
        .request(
            "POST",
            "/api/login",
            Some(&old),
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await;
    let view = login.expect(200);
    assert!(view.get("token").is_none());
    let current = login.auth();
    assert_ne!(old, current);
    server
        .request("GET", "/api/config", Some(&old), None)
        .await
        .expect(200);
    server.config(&current).await;

    // Cookie is shared between tabs, while each tab still owns its binding.
    let (new_cookie, _) = current.split_once('|').unwrap();
    let (old_cookie, _) = old.split_once('|').unwrap();
    let old_tab_with_new_cookie =
        String::from_utf8(server.wire("GET", "/api/config", Some(&old), None))
            .unwrap()
            .replace(
                &format!("Cookie: parins_session_http={old_cookie}"),
                &format!("Cookie: parins_session_http={new_cookie}"),
            );
    assert_eq!(
        server
            .raw(old_tab_with_new_cookie.as_bytes())
            .await
            .expect(409)["error"]["code"],
        "SESSION_CHANGED"
    );
    server.finish().await;
}

#[tokio::test]
async fn configuration_forms_are_authenticated_read_only_and_apply_with_revision() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let source = configuration();
    server
        .request("GET", "/api/stats", None, None)
        .await
        .expect(401);
    for (path, body) in [
        ("/api/config/parse", json!({"toml": source})),
        (
            "/api/config/preview",
            json!({"toml": source, "changes": {}}),
        ),
    ] {
        server
            .request("POST", path, None, Some(body))
            .await
            .expect(401);
    }
    let token = server.setup(&directory, &source).await;
    let stats = server
        .request("GET", "/api/stats", Some(&token), None)
        .await
        .expect(200);
    assert_eq!(stats["interval_seconds"], 60);
    assert_eq!(stats["retention_seconds"], 86400);
    assert!(stats["samples"].is_array());
    let status = server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(200);
    assert!(status["generation"].as_u64().unwrap() > 0);
    assert!(status["uptime_seconds"].is_u64());
    let before = std::fs::read(directory.join("state.json")).unwrap();
    let saved = server.config(&token).await;
    let parsed = server
        .request(
            "POST",
            "/api/config/parse",
            Some(&token),
            Some(json!({"toml": source})),
        )
        .await
        .expect(200);
    assert_eq!(parsed["toml"], source);
    assert_eq!(
        parsed["settings"]["filter"]["block_exact"],
        json!(["example.test"])
    );
    assert_eq!(parsed["settings"]["cache"]["enabled"], true);
    let preview = server
        .request(
            "POST",
            "/api/config/preview",
            Some(&token),
            Some(json!({"toml": source, "changes": {"cache": {"enabled": false}}})),
        )
        .await
        .expect(200);
    assert_eq!(preview["settings"]["cache"]["enabled"], false);
    assert_eq!(
        preview["settings"]["filter"]["block_exact"],
        json!(["example.test"])
    );
    assert_eq!(std::fs::read(directory.join("state.json")).unwrap(), before);
    assert_eq!(server.config(&token).await, saved);
    for (body, status) in [
        (json!({"toml": source, "changes": []}), 400),
        (json!({"toml": source, "changes": {}, "revision": 1}), 400),
        (json!({"toml": source, "changes": {"typo": true}}), 422),
    ] {
        server
            .request("POST", "/api/config/preview", Some(&token), Some(body))
            .await
            .expect(status);
    }
    let no_file = server
        .request(
            "POST",
            "/api/config/preview",
            Some(&token),
            Some(json!({"toml": source, "changes": {"filter_file": "missing-rules.toml"}})),
        )
        .await
        .expect(200);
    server
        .request(
            "POST",
            "/api/config/validate",
            Some(&token),
            Some(json!({"toml": no_file["toml"]})),
        )
        .await
        .expect(422);
    server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml": preview["toml"], "revision": 0})),
        )
        .await
        .expect(409);
    server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml": preview["toml"], "revision": saved["revision"]})),
        )
        .await
        .expect(200);
    let applied = server.config(&token).await;
    assert_eq!(applied["revision"], 2);
    assert_eq!(applied["toml"], preview["toml"]);
    let next_status = server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(200);
    assert_eq!(
        next_status["generation"], status["generation"],
        "cache-only apply keeps listener generation"
    );
    assert_eq!(next_status["listen"], status["listen"]);
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}

#[tokio::test]
async fn cache_inspection_invalidation_and_hot_policy_apply_are_authenticated_and_revisioned() {
    use hickory_proto::rr::{RData, Record, rdata::A};
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let mock = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        loop {
            let (len, peer) = upstream.recv_from(&mut bytes).await.unwrap();
            let q = protocol::decode(&bytes[..len]).unwrap();
            let mut r = protocol::error_response(&q, ResponseCode::NoError);
            r.add_answer(Record::from_rdata(
                q.queries[0].name().clone(),
                60,
                RData::A(A::new(192, 0, 2, 1)),
            ));
            upstream.send_to(&r.to_vec().unwrap(), peer).await.unwrap();
        }
    });
    let server = Management::start(&directory).await;
    for path in ["/api/cache/inspect", "/api/cache/invalidate"] {
        server
            .request("POST", path, None, Some(json!({})))
            .await
            .expect(401);
    }
    let source = configuration().replace("127.0.0.1:9", &upstream_addr.to_string());
    let token = server.setup(&directory, &source).await;
    let dns = server.dns(&token).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for name in ["cache.test.", "other.test."] {
        client
            .send_to(&query(name).to_vec().unwrap(), dns)
            .await
            .unwrap();
        let mut bytes = [0; 4096];
        let len = timeout(DEADLINE, client.recv(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(protocol::decode(&bytes[..len]).unwrap().answers.len(), 1);
    }
    let status = server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(200);
    assert_eq!(status["cache"]["entries"], 2);
    let inspection = server
        .request(
            "POST",
            "/api/cache/inspect",
            Some(&token),
            Some(json!({"name":"CACHE.test"})),
        )
        .await
        .expect(200);
    assert_eq!(inspection["explanation"]["state"], "fresh");
    assert_eq!(
        inspection["inspection"]["variants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let unchanged = server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(200);
    assert_eq!(
        status["cache"], unchanged["cache"],
        "inspection is non-touching"
    );
    for body in [
        json!({"revision":1,"epoch":inspection["epoch"]}),
        json!({"revision":1,"epoch":inspection["epoch"],"all":true,"name":"cache.test"}),
    ] {
        server
            .request("POST", "/api/cache/invalidate", Some(&token), Some(body))
            .await
            .expect(422);
    }
    let selected = json!({"revision":1,"epoch":inspection["epoch"],"name":"cache.test", "qtype":"A", "scope":"no_ecs"});
    let removed = server
        .request(
            "POST",
            "/api/cache/invalidate",
            Some(&token),
            Some(selected.clone()),
        )
        .await
        .expect(200);
    assert_eq!(removed["removed"], 1);
    server
        .request(
            "POST",
            "/api/cache/invalidate",
            Some(&token),
            Some(selected),
        )
        .await
        .expect(409);
    let other = server
        .request(
            "POST",
            "/api/cache/inspect",
            Some(&token),
            Some(json!({"name":"other.test"})),
        )
        .await
        .expect(200);
    assert_eq!(other["explanation"]["state"], "fresh");
    let next = format!(
        "{source}\n[cache]\nmax_ttl_secs = 30\n[[cache.rules]]\nname = 'other.test'\nbypass = true\n"
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/config/validate",
                Some(&token),
                Some(json!({"toml":next}))
            )
            .await
            .expect(200)["restart_required"],
        false
    );
    let applied = server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(200);
    assert_eq!(applied["restart_required"], false);
    let after = server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(200);
    assert_eq!(after["generation"], status["generation"]);
    assert_eq!(after["listen"], status["listen"]);
    assert_eq!(after["cache"]["entries"], 0);
    let bypass = server
        .request(
            "POST",
            "/api/cache/inspect",
            Some(&token),
            Some(json!({"name":"other.test"})),
        )
        .await
        .expect(200);
    assert_eq!(bypass["explanation"]["state"], "bypass");
    server
        .request(
            "POST",
            "/api/cache/invalidate",
            Some(&token),
            Some(json!({"revision":1,"epoch":0,"all":true})),
        )
        .await
        .expect(409);
    let rollback = server
        .request(
            "POST",
            "/api/config/rollback",
            Some(&token),
            Some(json!({"revision":2})),
        )
        .await
        .expect(200);
    assert_eq!(rollback["restart_required"], false);
    assert_dns(dns).await;
    server.finish().await;
    mock.abort();
    let _ = mock.await;
}

#[tokio::test]
async fn ipv4_wildcard_supports_public_ip_origin_without_bypassing_authentication() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let server = Management::start_reserved(&directory, listener).await;
    let public_host = format!("203.0.113.10:{}", server.address.port());
    for (host, origin, status) in [
        (public_host.clone(), format!("http://{public_host}"), 200),
        (
            public_host.clone(),
            format!("http://203.0.113.11:{}", server.address.port()),
            403,
        ),
        (public_host.clone(), format!("https://{public_host}"), 403),
        (
            format!("attacker.example:{}", server.address.port()),
            format!("http://attacker.example:{}", server.address.port()),
            403,
        ),
        ("203.0.113.10:0".into(), "http://203.0.113.10:0".into(), 403),
    ] {
        let request = format!(
            "GET /api/session HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
        );
        server.raw(request.as_bytes()).await.expect(status);
    }
    let private = format!(
        "GET /api/config HTTP/1.1\r\nHost: {public_host}\r\nOrigin: http://{public_host}\r\nConnection: close\r\n\r\n"
    );
    server.raw(private.as_bytes()).await.expect(401);
    let setup = String::from_utf8(server.wire(
        "POST",
        "/api/setup",
        None,
        Some(json!({"username":"admin","password":PASSWORD,"toml":configuration()})),
    ))
    .unwrap()
    .replace(
        &format!("Host: {}", server.address),
        &format!("Host: {public_host}"),
    )
    .replace(
        &format!("Origin: http://{}", server.address),
        &format!("Origin: http://{public_host}"),
    )
    .replacen("\r\n\r\n", "\r\nX-PariNS-Setup: wrong-token\r\n\r\n", 1);
    server.raw(setup.as_bytes()).await.expect(403);
    assert!(!directory.join("state.json").exists());
    server.finish().await;
}

#[tokio::test]
async fn ipv6_wildcard_serves_http_with_bracketed_public_ip_origin() {
    let listener = match TcpListener::bind("[::]:0").await {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            eprintln!("IPv6 unavailable on this test host: {error}");
            return;
        }
        Err(error) => panic!("bind IPv6 management listener: {error}"),
    };
    let temporary = tempfile::tempdir().unwrap();
    let server = Management::start_reserved(&temporary.path().join("state"), listener).await;
    let host = format!("[2001:db8::10]:{}", server.address.port());
    let request = format!(
        "GET /api/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nConnection: close\r\n\r\n"
    );
    server.raw(request.as_bytes()).await.expect(200);
    server.finish().await;
}

#[tokio::test]
async fn fresh_management_serves_http_without_generating_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let response = server.request("GET", "/api/session", None, None).await;
    assert_eq!(response.expect(200)["transport"]["scheme"], "http");
    assert!(!directory.join("https-identity.pem").exists());
    assert!(!directory.join("https-cert.pem").exists());
    server.finish().await;
}

#[tokio::test]
async fn doh_apply_reuses_identity_and_downgrade_requires_confirmation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let original = configuration();
    let old = server.setup(&directory, &original).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("doh-cert.pem");
    let key_path = temporary.path().join("doh-key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let doh_address = reservation.local_addr().unwrap();
    drop(reservation);
    let candidate = format!(
        "{original}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='{doh_address}'\ncert_file={}\nkey_file={}\n",
        json!(cert_path),
        json!(key_path)
    );
    let preview = server
        .request(
            "POST",
            "/api/config/preview",
            Some(&old),
            Some(json!({"toml":candidate,"changes":{}})),
        )
        .await
        .expect(200);
    assert_eq!(preview["transport_change"]["to"], "https");
    let validated = server
        .request(
            "POST",
            "/api/config/validate",
            Some(&old),
            Some(json!({"toml":candidate})),
        )
        .await
        .expect(200);
    assert_eq!(
        validated["transport_change"]["next_origin"],
        format!("https://dns.test:{}", server.address.port())
    );
    let applied = server
        .request(
            "PUT",
            "/api/config",
            Some(&old),
            Some(json!({"toml":candidate,"revision":1})),
        )
        .await
        .expect(200);
    assert_eq!(applied["transport_change"]["reauthenticate"], true);
    assert_eq!(applied["revision"], 2);
    let get_session = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/session",
        None,
        None,
    );
    let session = secure_request(server.address, &identity, "dns.test", &get_session).await;
    assert_eq!(session.expect(200)["authenticated"], false);
    assert_eq!(session.json()["transport"]["certificate_source"], "doh");
    let login_wire = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/login",
        None,
        Some(json!({"username":"admin","password":PASSWORD})),
    );
    let login = secure_request(server.address, &identity, "dns.test", &login_wire).await;
    login.expect(200);
    assert!(login.headers.contains("set-cookie: __host-parins_session="));
    assert!(
        login
            .headers
            .contains("httponly; secure; samesite=strict; max-age=28800")
    );
    let secure_auth = login.auth();
    let status_wire = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/status",
        Some(&secure_auth),
        None,
    );
    let status = secure_request(server.address, &identity, "dns.test", &status_wire).await;
    assert_eq!(status.expect(200)["transport"]["scheme"], "https");

    let management_tls = trusted_tls(server.address, &identity, "dns.test", b"http/1.1").await;
    let doh_tls = trusted_tls(doh_address, &identity, "dns.test", b"h2").await;
    assert_eq!(
        management_tls.get_ref().1.peer_certificates().unwrap()[0].as_ref(),
        doh_tls.get_ref().1.peer_certificates().unwrap()[0].as_ref()
    );
    assert_eq!(doh_tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));

    let rollback_preview = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/config/rollback/preview",
        Some(&secure_auth),
        Some(json!({"revision":2})),
    );
    let preview = secure_request(server.address, &identity, "dns.test", &rollback_preview).await;
    assert_eq!(
        preview.expect(200)["transport_change"]["requires_http_confirmation"],
        true
    );
    let rollback = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/config/rollback",
        Some(&secure_auth),
        Some(json!({"revision":2})),
    );
    let rejected = secure_request(server.address, &identity, "dns.test", &rollback).await;
    assert_eq!(
        rejected.expect(409)["error"]["code"],
        "HTTP_DOWNGRADE_CONFIRMATION_REQUIRED"
    );
    let rollback = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/config/rollback",
        Some(&secure_auth),
        Some(json!({"revision":2,"allow_http_downgrade":true})),
    );
    let result = secure_request(server.address, &identity, "dns.test", &rollback).await;
    assert_eq!(result.expect(200)["transport_change"]["to"], "http");
    let back = server.request("GET", "/api/session", None, None).await;
    assert_eq!(back.expect(200)["transport"]["scheme"], "http");
    assert_eq!(back.json()["authenticated"], false);
    server.finish().await;
}

#[tokio::test]
async fn setup_switches_to_https_without_issuing_an_http_cookie() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("setup-cert.pem");
    let key_path = temporary.path().join("setup-key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    let preview_wire = String::from_utf8(server.wire(
        "POST",
        "/api/setup/preview",
        None,
        Some(json!({"toml":candidate})),
    ))
    .unwrap()
    .replacen(
        "\r\n\r\n",
        &format!("\r\nX-PariNS-Setup: {token}\r\n\r\n"),
        1,
    );
    let preview = server.raw(preview_wire.as_bytes()).await;
    assert_eq!(
        preview.expect(200)["transport_change"]["next_origin"],
        format!("https://dns.test:{}", server.address.port())
    );
    let response = server.setup_with(&token, &candidate).await;
    assert_eq!(response.expect(200)["authenticated"], false);
    assert_eq!(response.json()["transport_change"]["to"], "https");
    assert!(!response.headers.contains("set-cookie:"));
    let wire = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/session",
        None,
        None,
    );
    assert_eq!(
        secure_request(server.address, &identity, "dns.test", &wire)
            .await
            .expect(200)["authenticated"],
        false
    );
    server.finish().await;
}

#[tokio::test]
async fn rejected_management_identity_preserves_http_revision_and_sessions() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let original = configuration();
    let auth = server.setup(&directory, &original).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("cert.pem");
    let key_path = temporary.path().join("key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let candidate = format!(
        "{original}\n[web]\npublic_host='other.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        json!(cert_path),
        json!(key_path)
    );
    let rejected = server
        .request(
            "POST",
            "/api/config/validate",
            Some(&auth),
            Some(json!({"toml":candidate})),
        )
        .await;
    assert_eq!(
        rejected.expect(422)["error"]["code"],
        "CERTIFICATE_NAME_MISMATCH"
    );
    let rejected = server
        .request(
            "PUT",
            "/api/config",
            Some(&auth),
            Some(json!({"toml":candidate,"revision":1})),
        )
        .await;
    assert_eq!(
        rejected.expect(422)["error"]["code"],
        "CERTIFICATE_NAME_MISMATCH"
    );
    assert_eq!(server.config(&auth).await["revision"], 1);
    assert_eq!(
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200)["transport"]["scheme"],
        "http"
    );
    std::fs::write(&cert_path, "invalid PEM").unwrap();
    let invalid = server
        .request(
            "POST",
            "/api/config/validate",
            Some(&auth),
            Some(json!({"toml":candidate.replace("other.test", "dns.test")})),
        )
        .await;
    assert_eq!(invalid.expect(422)["error"]["code"], "CERTIFICATE_INVALID");
    server.config(&auth).await;
    server.finish().await;
}

#[tokio::test]
async fn not_yet_valid_and_expired_management_certificates_are_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let auth = server.setup(&directory, &configuration()).await;
    let cert_path = temporary.path().join("dated-cert.pem");
    let key_path = temporary.path().join("dated-key.pem");
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let now = std::time::SystemTime::now();
    for (not_before, not_after) in [
        (
            now + Duration::from_secs(3600),
            now + Duration::from_secs(7200),
        ),
        (
            now - Duration::from_secs(7200),
            now - Duration::from_secs(3600),
        ),
    ] {
        let mut params = rcgen::CertificateParams::new(vec!["dns.test".into()]).unwrap();
        params.not_before = not_before.into();
        params.not_after = not_after.into();
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        std::fs::write(&cert_path, certificate.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        let rejected = server
            .request(
                "POST",
                "/api/config/validate",
                Some(&auth),
                Some(json!({"toml":candidate})),
            )
            .await;
        assert_eq!(rejected.expect(422)["error"]["code"], "CERTIFICATE_INVALID");
        assert_eq!(server.config(&auth).await["revision"], 1);
    }
    server.finish().await;
}

#[tokio::test]
async fn saved_https_with_missing_identity_fails_startup_closed() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("cert.pem");
    let key_path = temporary.path().join("key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    server.setup_with(&token, &candidate).await.expect(200);
    server.finish().await;
    std::fs::remove_file(&cert_path).unwrap();
    let result = manage::serve(&directory, "127.0.0.1:0".parse().unwrap(), async {}).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("management TLS stage")
    );
    assert!(!directory.join("https-identity.pem").exists());
}

#[tokio::test]
async fn same_origin_certificate_reapply_changes_identity_without_revoking_session() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let first = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("renew-cert.pem");
    let key_path = temporary.path().join("renew-key.pem");
    std::fs::write(&cert_path, first.cert.pem()).unwrap();
    std::fs::write(&key_path, first.signing_key.serialize_pem()).unwrap();
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    server.setup_with(&token, &candidate).await.expect(200);
    let login = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/login",
        None,
        Some(json!({"username":"admin","password":PASSWORD})),
    );
    let login = secure_request(server.address, &first, "dns.test", &login).await;
    login.expect(200);
    let auth = login.auth();

    let second = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    std::fs::write(&cert_path, second.cert.pem()).unwrap();
    std::fs::write(&key_path, second.signing_key.serialize_pem()).unwrap();
    let apply = https_wire(
        server.address,
        "dns.test",
        "PUT",
        "/api/config",
        Some(&auth),
        Some(json!({"toml":candidate,"revision":1})),
    );
    let result = secure_request(server.address, &first, "dns.test", &apply).await;
    assert_eq!(result.expect(200)["transport_change"], Value::Null);
    let status = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/status",
        Some(&auth),
        None,
    );
    let response = secure_request(server.address, &second, "dns.test", &status).await;
    assert_eq!(response.expect(200)["revision"], 2);
    let tls = trusted_tls(server.address, &second, "dns.test", b"http/1.1").await;
    assert_eq!(
        tls.get_ref().1.peer_certificates().unwrap()[0].as_ref(),
        second.cert.der().as_ref()
    );
    server.finish().await;
}

#[tokio::test]
async fn https_rejects_host_origin_and_http_cookie_confusion() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("cert.pem");
    let key_path = temporary.path().join("key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    server.setup_with(&token, &candidate).await.expect(200);
    let login = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/login",
        None,
        Some(json!({"username":"admin","password":PASSWORD})),
    );
    let login = secure_request(server.address, &identity, "dns.test", &login).await;
    login.expect(200);
    let auth = login.auth();
    let good = https_wire(
        server.address,
        "dns.test",
        "POST",
        "/api/config/validate",
        Some(&auth),
        Some(json!({"toml":candidate})),
    );
    secure_request(server.address, &identity, "dns.test", &good)
        .await
        .expect(200);
    let authority = format!("dns.test:{}", server.address.port());
    let wrong_port = if server.address.port() == u16::MAX {
        server.address.port() - 1
    } else {
        server.address.port() + 1
    };
    let origin = format!("Origin: https://{authority}\r\n");
    let invalid = [
        good.replace(&origin, ""),
        good.replace(&origin, "Origin: null\r\n"),
        good.replace(&origin, &format!("Origin: http://{authority}\r\n")),
        good.replace(
            &origin,
            &format!("Origin: https://dns.test:{wrong_port}\r\n"),
        ),
        good.replace(&origin, &format!("{origin}Origin: https://{authority}\r\n")),
        good.replace(
            &format!("Host: {authority}"),
            &format!("Host: attacker.test:{}", server.address.port()),
        ),
        good.replace(
            &format!("Host: {authority}"),
            &format!(
                "Host: attacker.test:{}\r\nX-Forwarded-Host: {authority}",
                server.address.port()
            ),
        ),
        good.replacen("\r\n\r\n", "\r\nSec-Fetch-Site: same-site\r\n\r\n", 1),
    ];
    for request in invalid {
        secure_request(server.address, &identity, "dns.test", &request)
            .await
            .expect(403);
    }
    let config = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/config",
        Some(&auth),
        None,
    );
    let (cookie, _) = auth.split_once('|').unwrap();
    let with_http_cookie = config.replace(
        &format!("Cookie: __Host-parins_session={cookie}"),
        &format!("Cookie: __Host-parins_session={cookie}; parins_session_http=unrelated"),
    );
    secure_request(server.address, &identity, "dns.test", &with_http_cookie)
        .await
        .expect(200);
    let only_http_cookie = config.replace(
        &format!("Cookie: __Host-parins_session={cookie}"),
        "Cookie: parins_session_http=unrelated",
    );
    secure_request(server.address, &identity, "dns.test", &only_http_cookie)
        .await
        .expect(401);
    server.finish().await;
}

#[tokio::test]
async fn doh3_only_serves_management_https_and_verified_h3_with_same_certificate() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert_path = temporary.path().join("h3-cert.pem");
    let key_path = temporary.path().join("h3-key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).unwrap();
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).unwrap();
    let reservation = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let h3_address = reservation.local_addr().unwrap();
    drop(reservation);
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh3]\nlisten='{h3_address}'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(cert_path),
        json!(key_path)
    );
    let token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    let setup = server.setup_with(&token, &candidate).await;
    assert_eq!(setup.expect(200)["transport"]["certificate_source"], "doh3");
    assert!(!setup.headers.contains("set-cookie:"));
    let request = https_wire(
        server.address,
        "dns.test",
        "GET",
        "/api/session",
        None,
        None,
    );
    let response = secure_request(server.address, &identity, "dns.test", &request).await;
    assert_eq!(response.expect(200)["transport"]["scheme"], "https");
    let management_tls = trusted_tls(server.address, &identity, "dns.test", b"http/1.1").await;

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(identity.cert.der().to_vec()))
        .unwrap();
    let mut tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let connection = timeout(DEADLINE, endpoint.connect(h3_address, "dns.test").unwrap())
        .await
        .unwrap()
        .unwrap();
    let peer = connection
        .peer_identity()
        .unwrap()
        .downcast::<Vec<CertificateDer<'static>>>()
        .unwrap();
    assert_eq!(
        peer[0].as_ref(),
        management_tls.get_ref().1.peer_certificates().unwrap()[0].as_ref()
    );
    connection.close(0u32.into(), b"test complete");
    endpoint.wait_idle().await;
    server.finish().await;
}

#[tokio::test]
async fn validation_is_read_only_and_revisioned_apply_and_rollback_keep_dns_live() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let original = configuration();
    let token = server.setup(&directory, &original).await;
    let initial_address = server.dns(&token).await;
    server
        .request(
            "POST",
            "/api/config/validate",
            Some(&token),
            Some(json!({"toml":"not toml"})),
        )
        .await
        .expect(422);
    let next = format!("{original}\n# accepted configuration\n");
    server
        .request(
            "POST",
            "/api/config/validate",
            Some(&token),
            Some(json!({"toml":next})),
        )
        .await
        .expect(200);
    assert_eq!(server.dns(&token).await, initial_address);
    assert_eq!(server.config(&token).await["toml"], original);
    server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":0})),
        )
        .await
        .expect(409);
    let result = server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(200);
    assert_eq!(result["revision"], 2);
    let saved = server.config(&token).await;
    assert_eq!(saved["toml"], next);
    assert_eq!(saved["has_backup"], true);
    assert_dns(server.dns(&token).await).await;
    server
        .request(
            "POST",
            "/api/config/rollback",
            Some(&token),
            Some(json!({"revision":1})),
        )
        .await
        .expect(409);
    assert_eq!(
        server
            .request(
                "POST",
                "/api/config/rollback",
                Some(&token),
                Some(json!({"revision":2}))
            )
            .await
            .expect(200)["revision"],
        3
    );
    assert_eq!(server.config(&token).await["toml"], original);
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}

#[tokio::test]
async fn candidate_port_collision_restores_previous_dns_without_committing() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let original = configuration();
    let token = server.setup(&directory, &original).await;
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next = original.replacen(
        "127.0.0.1:0",
        &occupied.local_addr().unwrap().to_string(),
        1,
    );
    server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(422);
    let saved = server.config(&token).await;
    assert_eq!(saved["revision"], 1);
    assert_eq!(saved["toml"], original);
    assert_eq!(saved["has_backup"], false);
    let disk: Value =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert_eq!(disk["revision"], 1);
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}

#[tokio::test]
async fn restart_restores_configuration_but_not_sessions_and_logout_revokes_token() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &configuration()).await;
    let exposed = server.config(&token).await;
    assert!(exposed.get("password_hash").is_none());
    let disk = std::fs::read_to_string(directory.join("state.json")).unwrap();
    assert!(!disk.contains(PASSWORD));
    assert!(!disk.contains(&token));
    assert!(disk.contains("$argon2id$"));
    server.finish().await;
    let server = Management::start(&directory).await;
    assert!(!directory.join("https-identity.pem").exists());
    assert!(!directory.join("https-cert.pem").exists());
    assert_eq!(
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200)["setup_required"],
        false
    );
    server
        .request("GET", "/api/status", Some(&token), None)
        .await
        .expect(401);
    server
        .request(
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":"incorrect-password"})),
        )
        .await
        .expect(401);
    let login = server
        .request(
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await;
    login.expect(200);
    let new_token = login.auth();
    assert_ne!(new_token, token);
    assert_eq!(server.config(&new_token).await["revision"], 1);
    assert_dns(server.dns(&new_token).await).await;
    server
        .request("POST", "/api/logout", Some(&new_token), None)
        .await
        .expect(200);
    server
        .request("GET", "/api/config", Some(&new_token), None)
        .await
        .expect(401);
    server.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn persistence_failure_restores_previous_dns_and_can_be_retried_after_repair() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let original = configuration();
    let token = server.setup(&directory, &original).await;
    let state_file = directory.join("state.json");
    let before = std::fs::read(&state_file).unwrap();
    // A deliberately exposed state file must be rejected by the real Store;
    // this reaches persistence after the candidate DNS sockets have bound.
    std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let next = format!("{original}\n# persisted after permission repair\n");
    let rejected = server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(422);
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("private permissions")
    );
    assert_eq!(std::fs::read(&state_file).unwrap(), before);
    let saved = server.config(&token).await;
    assert_eq!(saved["revision"], 1);
    assert_eq!(saved["toml"], original);
    assert_eq!(saved["has_backup"], false);
    assert_dns(server.dns(&token).await).await;

    std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let accepted = server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(200);
    assert_eq!(accepted["revision"], 2);
    let disk: Value = serde_json::from_slice(&std::fs::read(&state_file).unwrap()).unwrap();
    assert_eq!(disk["revision"], 2);
    assert_eq!(disk["toml"], next);
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}

#[tokio::test]
async fn same_source_setup_and_login_share_five_attempts_then_reject_before_authentication() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    for _ in 0..5 {
        server
            .setup_with("wrong-token", &configuration())
            .await
            .expect(403);
    }
    let login = server
        .request(
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await
        .expect(429);
    assert_eq!(login["error"]["code"], "LOGIN_LIMIT");
    let correct_token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    assert_eq!(
        server
            .setup_with(&correct_token, &configuration())
            .await
            .expect(429)["error"]["code"],
        "LOGIN_LIMIT"
    );
    assert!(!directory.join("state.json").exists());
    assert_eq!(
        server
            .request("GET", "/api/session", None, None)
            .await
            .expect(200)["setup_required"],
        true
    );
    server.finish().await;
}

#[tokio::test]
async fn exhausted_socket_source_does_not_block_other_source_setup_or_login() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let reservation = TcpListener::bind("[::]:0").await.unwrap();
    let server = Management::start_reserved(&directory, reservation).await;
    let other = SocketAddr::from(([127, 0, 0, 1], server.address.port()));
    for attempt in 0..5 {
        let wire = String::from_utf8(server.wire("POST", "/api/login", None, Some(json!({}))))
            .unwrap().replacen("\r\n\r\n", &format!("\r\nX-Forwarded-For: 192.0.2.{attempt}\r\nForwarded: for=198.51.100.{attempt}\r\n\r\n"), 1);
        server.raw(wire.as_bytes()).await.expect(400);
    }
    // Changing forwarding headers and opening another TCP socket cannot reset
    // the actual IPv6 peer's debt.
    let wire = String::from_utf8(server.wire("POST", "/api/login", None, Some(json!({}))))
        .unwrap()
        .replacen("\r\n\r\n", "\r\nX-Forwarded-For: 203.0.113.1\r\n\r\n", 1);
    server.raw(wire.as_bytes()).await.expect(429);
    let setup_token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    let setup = String::from_utf8(server.wire(
        "POST",
        "/api/setup",
        None,
        Some(json!({"username":"admin","password":PASSWORD,"toml":configuration()})),
    ))
    .unwrap()
    .replacen(
        "\r\n\r\n",
        &format!("\r\nX-PariNS-Setup: {setup_token}\r\n\r\n"),
        1,
    );
    let session = server.raw_at(other, setup.as_bytes()).await.expect(200);
    assert_eq!(session["authenticated"], true);
    let login = server.wire(
        "POST",
        "/api/login",
        None,
        Some(json!({"username":"admin","password":PASSWORD})),
    );
    server.raw_at(other, &login).await.expect(200);
    server.raw(&login).await.expect(429);
    server.finish().await;
}

#[tokio::test]
async fn request_body_and_configuration_sizes_are_bounded_before_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &configuration()).await;
    let (cookie, binding) = token.split_once('|').unwrap();
    let headers = format!(
        "POST /api/config/validate HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: parins_session_http={cookie}\r\nX-PariNS-Session: {binding}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        server.address,
        server.address,
        300 * 1024 + 1
    );
    let mut oversized = headers.into_bytes();
    oversized.resize(oversized.len() + 300 * 1024 + 1, b'x');
    server.raw(&oversized).await.expect(413);
    server
        .request(
            "POST",
            "/api/config/validate",
            Some(&token),
            Some(json!({"toml":"x".repeat(256 * 1024 + 1)})),
        )
        .await
        .expect(422);
    let no_json = format!(
        "POST /api/logout HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: parins_session_http={cookie}\r\nX-PariNS-Session: {binding}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        server.address, server.address,
    );
    server.raw(no_json.as_bytes()).await.expect(415);
    assert_eq!(server.config(&token).await["revision"], 1);
    server.finish().await;
}

#[tokio::test]
async fn query_logs_are_authenticated_bounded_and_revision_checked() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let config = format!(
        "{}\n[query_log]\nenabled=true\nmax_entries=2\nretention_secs=60\n",
        configuration()
    );
    let token = server.setup(&directory, &config).await;
    server
        .request("POST", "/api/query-log/list", None, Some(json!({})))
        .await
        .expect(401);
    let address = server.dns(&token).await;
    for _ in 0..3 {
        assert_dns(address).await;
    }
    let page = server
        .request(
            "POST",
            "/api/query-log/list",
            Some(&token),
            Some(json!({"limit":1,"status":null})),
        )
        .await
        .expect(200);
    assert_eq!(page["revision"], 1);
    assert_eq!(page["page"]["total"], 2);
    assert_eq!(page["page"]["entries"][0]["name"], "example.test.");
    assert_eq!(page["page"]["entries"][0]["status"], "blocked");
    assert_eq!(page["page"]["entries"][0]["transport"], "udp");
    let older = server
        .request(
            "POST",
            "/api/query-log/list",
            Some(&token),
            Some(json!({"before_id":page["page"]["next_cursor"],"search":"EXAMPLE"})),
        )
        .await
        .expect(200);
    assert_eq!(older["page"]["entries"].as_array().unwrap().len(), 1);
    server
        .request(
            "POST",
            "/api/query-log/list",
            Some(&token),
            Some(json!({"limit":101})),
        )
        .await
        .expect(422);
    server
        .request(
            "POST",
            "/api/query-log/list",
            Some(&token),
            Some(json!({"status":"unknown"})),
        )
        .await
        .expect(422);
    server
        .request(
            "POST",
            "/api/query-log/clear",
            None,
            Some(json!({"revision":1})),
        )
        .await
        .expect(401);
    server
        .request(
            "POST",
            "/api/query-log/clear",
            Some(&token),
            Some(json!({"revision":0})),
        )
        .await
        .expect(409);
    let cleared = server
        .request(
            "POST",
            "/api/query-log/clear",
            Some(&token),
            Some(json!({"revision":1})),
        )
        .await
        .expect(200);
    assert_eq!(cleared["removed"], 2);
    let page = server
        .request("POST", "/api/query-log/list", Some(&token), Some(json!({})))
        .await
        .expect(200);
    assert_eq!(page["page"]["total"], 0);
    server.finish().await;
}

#[tokio::test]
async fn pasted_certificate_is_private_and_applies_via_existing_config_transaction() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &configuration()).await;
    let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = identity.signing_key.serialize_pem();
    let body = json!({"revision":1,"certificate_pem":identity.cert.pem(),"private_key_pem":key});
    server
        .request("POST", "/api/certificates/import", None, Some(body.clone()))
        .await
        .expect(401);
    let mut stale = body.clone();
    stale["revision"] = json!(0);
    server
        .request(
            "POST",
            "/api/certificates/import",
            Some(&token),
            Some(stale),
        )
        .await
        .expect(409);
    let imported = server
        .request("POST", "/api/certificates/import", Some(&token), Some(body))
        .await
        .expect(200);
    assert!(!imported.to_string().contains("PRIVATE KEY"));
    assert!(imported["identity"]["key_matches"].as_bool().unwrap());
    let path = imported["identity"]["cert_file"].as_str().unwrap();
    assert!(Path::new(path).starts_with(std::fs::canonicalize(&directory).unwrap()));
    let next = format!(
        "{}\n[dot]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration(),
        json!(path),
        json!(path)
    );
    server
        .request(
            "POST",
            "/api/config/validate",
            Some(&token),
            Some(json!({"toml":next})),
        )
        .await
        .expect(200);
    server
        .request(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        )
        .await
        .expect(200);
    let saved = server.config(&token).await;
    assert_eq!(saved["revision"], 2);
    assert!(!saved.to_string().contains(&key));
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}

#[tokio::test]
async fn admitted_configuration_transaction_survives_http_disconnect() {
    disconnected_transaction(false).await;
}

#[tokio::test]
async fn shutdown_waits_for_admitted_transaction_then_stops_the_new_dns_instance() {
    disconnected_transaction(true).await;
}

async fn disconnected_transaction(stop_management: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let original = configuration()
        .replacen(
            "127.0.0.1:9",
            &upstream.local_addr().unwrap().to_string(),
            1,
        )
        .replace("query_timeout_ms = 200", "query_timeout_ms = 2000")
        .replace("shutdown_grace_ms = 200", "shutdown_grace_ms = 1000");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &original).await;
    let old_dns = server.dns(&token).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&query("pending.test.").to_vec().unwrap(), old_dns)
        .await
        .unwrap();
    let mut received = [0; 4096];
    timeout(DEADLINE, upstream.recv_from(&mut received))
        .await
        .unwrap()
        .unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let candidate_address = reservation.local_addr().unwrap();
    drop(reservation);
    let next = original.replacen("127.0.0.1:0", &candidate_address.to_string(), 1);
    let mut connection = server.connect().await;
    connection
        .write_all(&server.wire(
            "PUT",
            "/api/config",
            Some(&token),
            Some(json!({"toml":next,"revision":1})),
        ))
        .await
        .unwrap();
    connection.flush().await.unwrap();
    // The old listener closes before draining the deliberately pending query.
    // This proves apply was admitted before the HTTP client disappears.
    timeout(DEADLINE, async {
        loop {
            if TcpStream::connect(old_dns).await.is_err() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(connection);
    if stop_management {
        server.finish().await;
        let disk: Value =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        assert_eq!(disk["revision"], 2);
        assert_eq!(disk["toml"], next);
        // Shutdown must also stop the DNS instance created by the accepted
        // transaction, not leave an orphan listener after HTTP tasks disappear.
        let _tcp = TcpListener::bind(candidate_address).await.unwrap();
        let _udp = UdpSocket::bind(candidate_address).await.unwrap();
        return;
    }
    let saved = server.config(&token).await;
    assert_eq!(saved["revision"], 2);
    assert_eq!(saved["toml"], next);
    let disk: Value =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    assert_eq!(disk["revision"], 2);
    assert_dns(server.dns(&token).await).await;
    server.finish().await;
}
