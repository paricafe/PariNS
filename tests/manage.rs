use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{manage, protocol};
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_rustls::{TlsConnector, client::TlsStream};

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
            .find_map(|line| line.strip_prefix("set-cookie: __host-parins_session="))
            .expect("HTTPS response sets session Cookie")
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
    connector: TlsConnector,
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
        let certificate = directory.join("https-cert.pem");
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            manage::serve(&directory, listen, None, async {
                let _ = stopped.await;
            })
            .await
        });
        timeout(DEADLINE, async {
            loop {
                if certificate.is_file() && TcpStream::connect(address).await.is_ok() {
                    break;
                }
                assert!(!task.is_finished(), "management exited before binding");
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut roots = RootCertStore::empty();
        for certificate in CertificateDer::pem_file_iter(certificate).unwrap() {
            roots.add(certificate.unwrap()).unwrap();
        }
        let server = Self {
            address,
            connector: connector(roots),
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
            request.push_str(&format!("Origin: https://{}\r\n", self.address));
        }
        if let Some(token) = token {
            let (cookie, binding) = token.split_once('|').expect("Cookie and binding fixture");
            request.push_str(&format!(
                "Cookie: __Host-parins_session={cookie}\r\nX-PariNS-Session: {binding}\r\n"
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
            let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
            let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
            Response {
                status,
                headers: headers.to_ascii_lowercase(),
                body: body.to_owned(),
            }
        })
        .await
        .expect("HTTP request deadline")
    }

    async fn connect(&self) -> TlsStream<TcpStream> {
        self.connect_at(self.address).await
    }

    async fn connect_at(&self, address: SocketAddr) -> TlsStream<TcpStream> {
        self.connector
            .connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(address).await.unwrap(),
            )
            .await
            .expect("HTTPS handshake with trusted generated certificate")
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

fn connector(roots: RootCertStore) -> TlsConnector {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsConnector::from(Arc::new(config))
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
        "Origin: https://attacker.example\r\n",
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
        "GET /api/session HTTP/1.1\r\nHost: localhost:{}\r\nOrigin: https://localhost:{}\r\nConnection: close\r\n\r\n",
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
            .contains("httponly; secure; samesite=strict; max-age=28800")
    );
    assert!(
        issued
            .headers
            .contains("set-cookie: __host-parins_session=")
    );
    assert!(!issued.headers.contains("domain="));
    let auth = issued.auth();
    let (cookie, binding) = auth.split_once('|').unwrap();
    assert_eq!(view["session"]["binding"], binding);
    assert!(view["session"]["expires_in_seconds"].as_u64().unwrap() <= 28800);

    let resume = format!(
        "GET /api/session HTTP/1.1\r\nHost: {}\r\nCookie: __Host-parins_session={cookie}\r\nConnection: close\r\n\r\n",
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
    assert!(logout.headers.contains(
        "set-cookie: __host-parins_session=; path=/; httponly; secure; samesite=strict; max-age=0"
    ));
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
    assert!(repeated.headers.contains("max-age=0"));
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
    let origin = format!("Origin: https://{}\r\n", server.address);
    for wire in [
        valid.replace(&origin, ""),
        valid.replace(&origin, "Origin: null\r\n"),
        valid.replace(&origin, "Origin: https://attacker.example\r\n"),
        valid.replace(
            &origin,
            &format!("{origin}Origin: https://{}\r\n", server.address),
        ),
        valid.replace(
            &origin,
            &format!("Origin: https://127.0.0.1:{}1\r\n", server.address.port()),
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
async fn successful_login_replaces_the_presented_cookie_but_failed_login_preserves_it() {
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
        .expect(401);
    server.config(&current).await;

    // Cookie is shared between tabs, while each tab still owns its binding.
    let (new_cookie, _) = current.split_once('|').unwrap();
    let (old_cookie, _) = old.split_once('|').unwrap();
    let old_tab_with_new_cookie =
        String::from_utf8(server.wire("GET", "/api/config", Some(&old), None))
            .unwrap()
            .replace(
                &format!("Cookie: __Host-parins_session={old_cookie}"),
                &format!("Cookie: __Host-parins_session={new_cookie}"),
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
        (public_host.clone(), format!("https://{public_host}"), 200),
        (
            public_host.clone(),
            format!("https://203.0.113.11:{}", server.address.port()),
            403,
        ),
        (public_host.clone(), format!("http://{public_host}"), 403),
        (
            format!("attacker.example:{}", server.address.port()),
            format!("https://attacker.example:{}", server.address.port()),
            403,
        ),
        (
            "203.0.113.10:0".into(),
            "https://203.0.113.10:0".into(),
            403,
        ),
    ] {
        let request = format!(
            "GET /api/session HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n"
        );
        server.raw(request.as_bytes()).await.expect(status);
    }
    let private = format!(
        "GET /api/config HTTP/1.1\r\nHost: {public_host}\r\nOrigin: https://{public_host}\r\nConnection: close\r\n\r\n"
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
        &format!("Origin: https://{}", server.address),
        &format!("Origin: https://{public_host}"),
    )
    .replacen("\r\n\r\n", "\r\nX-PariNS-Setup: wrong-token\r\n\r\n", 1);
    server.raw(setup.as_bytes()).await.expect(403);
    assert!(!directory.join("state.json").exists());
    server.finish().await;
}

#[tokio::test]
async fn ipv6_wildcard_serves_https_with_bracketed_public_ip_origin() {
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
        "GET /api/session HTTP/1.1\r\nHost: {host}\r\nOrigin: https://{host}\r\nConnection: close\r\n\r\n"
    );
    server.raw(request.as_bytes()).await.expect(200);
    server.finish().await;
}

#[tokio::test]
async fn management_requires_tls_and_generated_certificate_is_not_implicitly_trusted() {
    let temporary = tempfile::tempdir().unwrap();
    let server = Management::start(&temporary.path().join("state")).await;
    let untrusted = connector(RootCertStore::empty());
    let failure = timeout(
        DEADLINE,
        untrusted.connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(server.address).await.unwrap(),
        ),
    )
    .await
    .unwrap()
    .expect_err("self-signed identity must require explicit trust");
    assert!(
        failure.to_string().contains("UnknownIssuer"),
        "unexpected TLS failure: {failure}"
    );
    let mut plain = TcpStream::connect(server.address).await.unwrap();
    plain
        .write_all(&server.wire("GET", "/api/session", None, None))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    let _closed = timeout(DEADLINE, plain.read_to_end(&mut bytes))
        .await
        .unwrap();
    assert!(
        !bytes.starts_with(b"HTTP/"),
        "plaintext must not reach the HTTP router"
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("setup_required"));
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
    let identity = std::fs::read(directory.join("https-identity.pem")).unwrap();
    let certificate = std::fs::read(directory.join("https-cert.pem")).unwrap();
    let token = server.setup(&directory, &configuration()).await;
    let exposed = server.config(&token).await;
    assert!(exposed.get("password_hash").is_none());
    let disk = std::fs::read_to_string(directory.join("state.json")).unwrap();
    assert!(!disk.contains(PASSWORD));
    assert!(!disk.contains(&token));
    assert!(disk.contains("$argon2id$"));
    server.finish().await;
    let server = Management::start(&directory).await;
    assert_eq!(
        std::fs::read(directory.join("https-identity.pem")).unwrap(),
        identity
    );
    assert_eq!(
        std::fs::read(directory.join("https-cert.pem")).unwrap(),
        certificate
    );
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
        "POST /api/config/validate HTTP/1.1\r\nHost: {}\r\nOrigin: https://{}\r\nCookie: __Host-parins_session={cookie}\r\nX-PariNS-Session: {binding}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        "POST /api/logout HTTP/1.1\r\nHost: {}\r\nOrigin: https://{}\r\nCookie: __Host-parins_session={cookie}\r\nX-PariNS-Session: {binding}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
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
