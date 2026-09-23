//! Integration between listener ownership, one resolver, startup files and reload publication.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{
    config::Config,
    ecs, protocol,
    server::Server,
    tls::{self, ListenerConfig, TlsFiles},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const WAIT: Duration = Duration::from_secs(3);

struct Certificate {
    directory: tempfile::TempDir,
    files: TlsFiles,
    der: rustls::pki_types::CertificateDer<'static>,
}

impl Certificate {
    fn new() -> Self {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            cert_file: directory.path().join("cert.pem"),
            key_file: directory.path().join("key.pem"),
        };
        std::fs::write(&files.cert_file, certificate.cert.pem()).unwrap();
        std::fs::write(&files.key_file, certificate.signing_key.serialize_pem()).unwrap();
        Self {
            directory,
            files,
            der: certificate.cert.der().clone(),
        }
    }
    fn listener(&self) -> ListenerConfig {
        ListenerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            files: self.files.clone(),
        }
    }
    async fn connect(&self, address: SocketAddr) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut roots = RootCertStore::empty();
        roots.add(self.der.clone()).unwrap();
        let mut client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        client.alpn_protocols = vec![b"dot".to_vec()];
        timeout(
            WAIT,
            TlsConnector::from(Arc::new(client)).connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(address).await.unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }
}

fn config(upstream: SocketAddr) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.listen.set_port(0);
    config.upstreams.servers = vec![upstream.to_string()];
    config.query_timeout_ms = 1000;
    config.tcp_io_timeout_ms = 1000;
    config.shutdown_grace_ms = 200;
    config
}

fn query(id: u16) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("runtime.test.").unwrap(),
        RecordType::A,
    ));
    query
}

fn answer(query: &Message) -> Message {
    let mut answer = protocol::error_response(query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    answer
}

async fn udp(address: SocketAddr, id: u16) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(id).to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let length = timeout(WAIT, socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    protocol::decode(&buffer[..length]).unwrap()
}

async fn write(stream: &mut (impl AsyncWrite + Unpin), message: &Message) {
    let bytes = message.to_vec().unwrap();
    stream.write_u16(bytes.len() as u16).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read(stream: &mut (impl AsyncRead + Unpin)) -> Message {
    timeout(WAIT, async {
        let length = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; usize::from(length)];
        stream.read_exact(&mut bytes).await.unwrap();
        protocol::decode(&bytes).unwrap()
    })
    .await
    .unwrap()
}

fn run(server: Server) -> (oneshot::Sender<()>, JoinHandle<anyhow::Result<()>>) {
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    (stop, task)
}

async fn finish(stop: oneshot::Sender<()>, task: JoinHandle<anyhow::Result<()>>) {
    stop.send(()).unwrap();
    timeout(WAIT, task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn source_budget_is_shared_by_udp_tcp_dot_and_charges_cache_hits() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    config.dot = Some(cert.listener());
    config.source_limits.enabled = true;
    config.source_limits.rate_per_sec = 1;
    config.source_limits.burst = 2;
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr().unwrap();
    let dot = server.encrypted_addrs().unwrap()[0].1;
    let metrics = server.metrics().clone();
    let (stop, task) = run(server);
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let request = protocol::decode(&buffer[..length]).unwrap();
        upstream
            .send_to(&answer(&request).to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    let mut tls = cert.connect(dot).await;
    let mut tcp = TcpStream::connect(address).await.unwrap();
    assert_eq!(udp(address, 71).await.answers.len(), 1);
    write(&mut tcp, &query(72)).await;
    assert_eq!(read(&mut tcp).await.answers.len(), 1);
    // Same peer, different connection and protocol: no fresh token bucket.
    write(&mut tls, &query(73)).await;
    let denied = read(&mut tls).await;
    assert_eq!(
        (denied.id, denied.response_code),
        (73, ResponseCode::ServFail)
    );
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(74).to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    assert!(
        timeout(Duration::from_millis(100), socket.recv(&mut buffer))
            .await
            .is_err()
    );
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.counters["source_queries_rejected"], 2);
    assert_eq!(snapshot.counters["upstream_operations"], 1);
    assert_eq!(snapshot.counters["cache_hits"], 1);
    finish(stop, task).await;
    mock.await.unwrap();
}

#[tokio::test]
async fn source_inflight_rejection_does_not_cancel_admitted_udp_query() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    config.source_limits.enabled = true;
    config.source_limits.max_inflight = 1;
    config.max_inflight = 8;
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr().unwrap();
    let metrics = server.metrics().clone();
    let (stop, task) = run(server);
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(81).to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let (length, peer) = timeout(WAIT, upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let request = protocol::decode(&buffer[..length]).unwrap();
    let mut tcp = TcpStream::connect(address).await.unwrap();
    write(&mut tcp, &query(82)).await;
    assert_eq!(read(&mut tcp).await.response_code, ResponseCode::ServFail);
    assert_eq!(metrics.snapshot().upstream_inflight, 1);
    upstream
        .send_to(&answer(&request).to_vec().unwrap(), peer)
        .await
        .unwrap();
    let length = timeout(WAIT, socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(protocol::decode(&buffer[..length]).unwrap().id, 81);
    // The completed query releases source concurrency, and a cache hit is admitted.
    write(&mut tcp, &query(83)).await;
    assert_eq!(read(&mut tcp).await.answers.len(), 1);
    assert_eq!(metrics.snapshot().counters["upstream_operations"], 1);
    finish(stop, task).await;
}

#[tokio::test]
async fn all_listeners_bind_together_and_udp_dot_tcp_share_peer_ecs_cache() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    // Global query capacity can exceed the QUIC per-connection stream cap.
    config.max_inflight = 2048;
    config.ecs.enabled = true;
    config.dot = Some(cert.listener());
    config.doh = Some(cert.listener());
    config.doq = Some(cert.listener());
    config.doh3 = Some(cert.listener());
    config.admin_listen = Some("127.0.0.1:0".parse().unwrap());
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr().unwrap();
    let encrypted = server.encrypted_addrs().unwrap();
    assert_eq!(
        encrypted.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["dot", "doh", "doq", "doh3"]
    );
    let dot = encrypted[0].1;
    let admin = server.admin_addr().unwrap().unwrap();
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let query = protocol::decode(&buffer[..length]).unwrap();
        let mut subnet = ecs::subnet(&query).unwrap();
        assert_eq!(subnet.addr().to_string(), "127.0.0.0");
        assert_eq!(subnet.source_prefix(), 24);
        subnet.set_scope_prefix(24);
        let mut reply = answer(&query);
        ecs::set_subnet(&mut reply, Some(subnet));
        upstream
            .send_to(&reply.to_vec().unwrap(), peer)
            .await
            .unwrap();
        // The mock exits: further answers must come from the shared resolver cache.
    });
    let (stop, task) = run(server);
    let first = udp(address, 11).await;
    assert_eq!((first.id, first.answers.len()), (11, 1));
    let mut client = cert.connect(dot).await;
    write(&mut client, &query(12)).await;
    let second = read(&mut client).await;
    assert_eq!((second.id, second.answers.len()), (12, 1));
    assert!(second.edns.is_none());
    let mut tcp = TcpStream::connect(address).await.unwrap();
    write(&mut tcp, &query(13)).await;
    assert_eq!(read(&mut tcp).await.id, 13);
    let mut health = TcpStream::connect(admin).await.unwrap();
    health
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut result = String::new();
    timeout(WAIT, health.read_to_string(&mut result))
        .await
        .unwrap()
        .unwrap();
    assert!(result.starts_with("HTTP/1.1 200"));
    finish(stop, task).await;
    mock.await.unwrap();
    for (name, address) in encrypted {
        if matches!(name, "dot" | "doh") {
            TcpListener::bind(address).await.unwrap();
        } else {
            UdpSocket::bind(address).await.unwrap();
        }
    }
    TcpListener::bind(admin).await.unwrap();
}

#[tokio::test]
async fn configured_dot_upstream_is_verified_and_never_falls_back_to_plaintext() {
    let cert = Certificate::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let plaintext_trap = UdpSocket::bind(upstream).await.unwrap();
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bootstrap_address = bootstrap.local_addr().unwrap();
    let bootstrap_task = tokio::spawn(async move {
        for _ in 0..4 {
            let mut bytes = [0; 4096];
            let (length, peer) = bootstrap.recv_from(&mut bytes).await.unwrap();
            let query = Message::from_vec(&bytes[..length]).unwrap();
            let mut response = protocol::error_response(&query, ResponseCode::NoError);
            if query.queries[0].query_type() == RecordType::A {
                response.add_answer(Record::from_rdata(
                    query.queries[0].name().clone(),
                    60,
                    RData::A(A::new(127, 0, 0, 1)),
                ));
            }
            bootstrap
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let acceptor = TlsAcceptor::from(tls::server_config(&cert.files, &[b"dot"]).unwrap());
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(stream).await.unwrap();
        let query = read(&mut stream).await;
        write(&mut stream, &answer(&query)).await;
        let (stream, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(stream).await.is_err());
    });
    for (name, expected) in [
        ("localhost", ResponseCode::NoError),
        ("wrong.test", ResponseCode::ServFail),
    ] {
        let mut config = config(upstream);
        config.upstreams.servers = vec![format!("tls://{name}:{}", upstream.port())];
        config.upstreams.bootstrap = vec![bootstrap_address];
        config.upstreams.ca_file = Some(cert.files.cert_file.clone());
        let server = Server::bind(config).await.unwrap();
        let address = server.local_addr().unwrap();
        let (stop, task) = run(server);
        assert_eq!(udp(address, 88).await.response_code, expected);
        finish(stop, task).await;
    }
    timeout(WAIT, mock).await.unwrap().unwrap();
    timeout(WAIT, bootstrap_task).await.unwrap().unwrap();
    assert_eq!(
        plaintext_trap.try_recv(&mut [0; 4096]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn failed_certificate_startup_releases_previously_bound_sockets() {
    let cert = Certificate::new();
    for _ in 0..16 {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = probe.local_addr().unwrap();
        // Hold both reservations until they are chosen so they are distinct.
        let dot_probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dot_address = dot_probe.local_addr().unwrap();
        drop(probe);
        drop(dot_probe);
        let mut config = config("127.0.0.1:9".parse().unwrap());
        config.listen = address;
        let mut dot = cert.listener();
        dot.listen = dot_address;
        config.dot = Some(dot);
        let mut bad = cert.listener();
        bad.files.key_file = cert.directory.path().join("missing-key.pem");
        config.doh = Some(bad);
        let error = match Server::bind(config).await {
            Ok(_) => panic!("missing certificate key unexpectedly accepted"),
            Err(error) => error,
        };
        if error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
        {
            continue;
        }
        assert!(
            format!("{error:#}").contains("load TLS private key"),
            "{error:#}"
        );
        let released = async {
            let _tcp = TcpListener::bind(address).await?;
            let _udp = UdpSocket::bind(address).await?;
            let _dot = TcpListener::bind(dot_address).await?;
            Ok::<(), std::io::Error>(())
        }
        .await;
        match released {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("cannot rebind released sockets: {error}"),
        }
    }
    panic!("could not verify socket release after 16 port reservations");
}

#[test]
fn relative_runtime_paths_and_check_resolve_against_configuration_directory() {
    let cert = Certificate::new();
    let path = cert.directory.path().join("parins.toml");
    std::fs::write(
        cert.directory.path().join("rules.toml"),
        "enabled = true\nblock_exact = ['runtime.test']",
    )
    .unwrap();
    let text = format!(
        "filter_file = 'rules.toml'\n{}\n[dot]\nlisten = '127.0.0.1:0'\ncert_file = 'cert.pem'\nkey_file = 'key.pem'\n",
        include_str!("../parins.example.toml")
    );
    let mut source: toml::Value = toml::from_str(&text).unwrap();
    source["upstreams"]
        .as_table_mut()
        .unwrap()
        .insert("ca_file".into(), toml::Value::String("cert.pem".into()));
    let text = toml::to_string(&source).unwrap();
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.filter_file.unwrap(),
        cert.directory.path().join("rules.toml")
    );
    assert_eq!(config.dot.unwrap().files.cert_file, cert.files.cert_file);
    assert_eq!(config.upstreams.ca_file.unwrap(), cert.files.cert_file);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_parins"))
        .args(["--config", path.to_str().unwrap(), "--check"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn reload_prepares_all_rule_and_certificate_candidates_before_publication() {
    let cert = Certificate::new();
    let replacement = Certificate::new();
    let rules = cert.directory.path().join("rules.toml");
    std::fs::write(&rules, "enabled = true\nblock_exact = ['runtime.test']").unwrap();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    config.dot = Some(cert.listener());
    config.filter_file = Some(rules.clone());
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr().unwrap();
    let dot = server.encrypted_addrs().unwrap()[0].1;
    let reload = server.reload_handle();
    let (stop, task) = run(server);
    assert!(udp(address, 1).await.answers.is_empty());
    std::fs::write(&rules, "enabled = false").unwrap();
    std::fs::copy(&replacement.files.cert_file, &cert.files.cert_file).unwrap();
    // New certificate plus old private key must not publish the unblocking policy.
    assert!(reload.reload().is_err());
    assert!(udp(address, 2).await.answers.is_empty());
    let client = cert.connect(dot).await;
    assert_eq!(client.get_ref().1.peer_certificates().unwrap()[0], cert.der);
    drop(client);
    std::fs::copy(&replacement.files.key_file, &cert.files.key_file).unwrap();
    reload.reload().unwrap();
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let query = protocol::decode(&buffer[..length]).unwrap();
        upstream
            .send_to(&answer(&query).to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    assert_eq!(udp(address, 3).await.answers.len(), 1);
    let client = replacement.connect(dot).await;
    assert_eq!(
        client.get_ref().1.peer_certificates().unwrap()[0],
        replacement.der
    );
    drop(client);
    std::fs::write(&rules, "unknown = true").unwrap();
    assert!(reload.reload().is_err());
    assert_eq!(udp(address, 4).await.answers.len(), 1);
    finish(stop, task).await;
    timeout(WAIT, mock).await.unwrap().unwrap();
}

#[tokio::test]
async fn resolver_profiles_keep_same_question_answers_and_warm_caches_isolated() {
    let mut resolvers = Vec::new();
    let mut mocks = Vec::new();
    for last_octet in [1, 2] {
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        resolvers.push(
            parins::resolver::Resolver::try_from_config(&config(upstream.local_addr().unwrap()))
                .unwrap(),
        );
        mocks.push(tokio::spawn(async move {
            let mut buffer = [0; 4096];
            let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
            let query = protocol::decode(&buffer[..length]).unwrap();
            let mut response = answer(&query);
            response.answers[0].data = RData::A(A::new(192, 0, 2, last_octet));
            upstream
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
            // One response per profile; warm lookups cannot consult these closed mocks.
        }));
    }
    for (id, profile) in [(11, 0), (12, 1), (13, 1), (14, 0)] {
        let response = resolvers[profile]
            .resolve(&query(id).to_vec().unwrap(), "192.0.2.50".parse().unwrap())
            .await
            .unwrap()
            .message;
        assert_eq!(response.id, id);
        assert_eq!(response.answers.len(), 1);
        assert_eq!(
            response.answers[0].data,
            RData::A(A::new(192, 0, 2, profile as u8 + 1))
        );
    }
    for mock in mocks {
        timeout(WAIT, mock).await.unwrap().unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn binary_sighup_reloads_rule_file_and_sigterm_exits_cleanly() {
    // Always reap the owned child, including when an assertion or deadline fails.
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("parins.toml");
    let rules = directory.path().join("rules.toml");
    std::fs::write(&rules, "enabled = true\nblock_exact = ['runtime.test']").unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let text = format!(
        "filter_file = 'rules.toml'\n{}",
        include_str!("../parins.example.toml")
    )
    .replace("127.0.0.1:5353", &address.to_string())
    .replace(
        "127.0.0.1:5354",
        &upstream.local_addr().unwrap().to_string(),
    );
    std::fs::write(&path, text).unwrap();
    let mut child = ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_parins"))
            .args(["--config", path.to_str().unwrap()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    timeout(WAIT, async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "server exited before readiness"
            );
            if let Ok(response) = timeout(Duration::from_millis(100), udp(address, 51)).await {
                assert_eq!(response.response_code, ResponseCode::NoError);
                assert!(response.answers.is_empty());
                break;
            }
        }
    })
    .await
    .unwrap();
    std::fs::write(&rules, "enabled = false").unwrap();
    assert!(
        std::process::Command::new("kill")
            .args(["-HUP", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let query = protocol::decode(&buffer[..length]).unwrap();
        upstream
            .send_to(&answer(&query).to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    timeout(WAIT, async {
        loop {
            let response = udp(address, 52).await;
            if !response.answers.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = timeout(WAIT, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
    timeout(WAIT, mock).await.unwrap().unwrap();
    TcpListener::bind(address).await.unwrap();
    UdpSocket::bind(address).await.unwrap();
}
