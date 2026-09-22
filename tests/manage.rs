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
        }
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        request.into_bytes()
    }

    async fn raw(&self, bytes: &[u8]) -> Response {
        timeout(DEADLINE, async {
            let mut stream = self.connect().await;
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
        self.connector
            .connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(self.address).await.unwrap(),
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
        self.setup_with(&token, toml).await.expect(200)["token"]
            .as_str()
            .unwrap()
            .to_owned()
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
    "listen = \"127.0.0.1:0\"\nupstream = \"127.0.0.1:9\"\nquery_timeout_ms = 200\ntcp_io_timeout_ms = 500\nshutdown_grace_ms = 200\nmax_inflight = 16\nmax_tcp_connections = 8\n[filter]\nenabled = true\nblock_exact = [\"example.test\"]\n".to_owned()
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
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
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
    .replacen(
        "\r\n\r\n",
        &format!("\r\nOrigin: https://{public_host}\r\nX-PariNS-Setup: wrong-token\r\n\r\n"),
        1,
    );
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
        .await
        .expect(200);
    let new_token = login["token"].as_str().unwrap();
    assert_ne!(new_token, token);
    assert_eq!(server.config(new_token).await["revision"], 1);
    assert_dns(server.dns(new_token).await).await;
    server
        .request("POST", "/api/logout", Some(new_token), None)
        .await
        .expect(200);
    server
        .request("GET", "/api/config", Some(new_token), None)
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
async fn setup_and_login_share_five_attempts_then_reject_before_authentication() {
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
async fn request_body_and_configuration_sizes_are_bounded_before_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &configuration()).await;
    let headers = format!(
        "POST /api/config/validate HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        "POST /api/logout HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        server.address
    );
    server.raw(no_json.as_bytes()).await.expect(415);
    assert_eq!(server.config(&token).await["revision"], 1);
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
