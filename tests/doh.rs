mod common;

use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{config::Config, doh, ecs, ingress::Ingress, protocol, resolver::Resolver};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Semaphore, watch},
    time::timeout,
};
use tokio_rustls::TlsConnector;

fn query() -> Vec<u8> {
    let mut message = Message::new(123, MessageType::Query, OpCode::Query);
    message.add_query(Query::query(
        Name::from_ascii("doh.test.").unwrap(),
        RecordType::A,
    ));
    message.to_vec().unwrap()
}

#[test]
fn shared_http_contract_rejects_ambiguous_or_oversized_requests() {
    let wire = query();
    let path = format!("/dns-query?dns={}", URL_SAFE_NO_PAD.encode(&wire));
    assert_eq!(
        doh::request_bytes("GET", &path, None, &[]),
        Ok(wire.clone())
    );
    assert_eq!(
        doh::request_bytes("POST", "/dns-query", Some("application/dns-message"), &wire),
        Ok(wire)
    );
    for (method, path, kind, body, status) in [
        ("GET", "/elsewhere", None, vec![], 404),
        ("PUT", "/dns-query", None, vec![], 405),
        ("POST", "/dns-query", Some("text/plain"), query(), 415),
        ("GET", "/dns-query?dns=AAAA&dns=AAAA", None, vec![], 400),
        ("GET", "/dns-query?dns=AAAA=", None, vec![], 400),
        ("GET", "/dns-query?dns=AAAA", None, vec![1], 400),
        (
            "POST",
            "/dns-query?extra=1",
            Some("application/dns-message"),
            query(),
            400,
        ),
        (
            "POST",
            "/dns-query",
            Some("application/dns-message"),
            vec![0; 65536],
            413,
        ),
        (
            "POST",
            "/dns-query",
            Some("application/dns-message"),
            vec![0; 11],
            400,
        ),
    ] {
        assert_eq!(doh::request_bytes(method, path, kind, &body), Err(status));
    }
}

fn certificates() -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    let identity = common::Identity::new(&["localhost"]);
    let mut roots = RootCertStore::empty();
    roots.add(identity.der.clone()).unwrap();
    let mut client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];
    (identity.server_config(&[b"h2"]), Arc::new(client))
}

#[tokio::test]
async fn real_h2_tls_get_post_errors_and_shutdown() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![upstream.local_addr().unwrap().to_string()];
    config.ecs.enabled = true;
    let resolver = Arc::new(Resolver::from_config(&config));
    let mock = tokio::spawn(async move {
        for _ in 0..2 {
            let mut buffer = [0; 4096];
            let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
            let query = protocol::decode(&buffer[..length]).unwrap();
            // X-Forwarded-For cannot forge the ECS identity derived from the socket.
            let subnet = ecs::subnet(&query).unwrap();
            assert_eq!(subnet.addr().to_string(), "127.0.0.0");
            assert_eq!(subnet.source_prefix(), 24);
            // Empty answer without SOA is deliberately not cached.
            let answer = protocol::error_response(&query, ResponseCode::NoError);
            upstream
                .send_to(&answer.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(2));
    let queries = Arc::new(Semaphore::new(4));
    let ingress = Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default())),
        resolver,
        queries: queries.clone(),
        connections: connections.clone(),
        stop: stopped,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_millis(300),
        max_streams: 4,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, None));
    let tls = TlsConnector::from(client_tls)
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut client, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    let wire = query();
    for (method, path, content_type, body, expected) in [
        (
            "GET",
            format!("/dns-query?dns={}", URL_SAFE_NO_PAD.encode(&wire)),
            None,
            Vec::new(),
            200,
        ),
        (
            "POST",
            "/dns-query".into(),
            Some("application/dns-message"),
            wire.clone(),
            200,
        ),
        (
            "POST",
            "/dns-query".into(),
            Some("text/plain"),
            wire.clone(),
            415,
        ),
        (
            "POST",
            "/dns-query".into(),
            Some("application/dns-message"),
            vec![0; 65536],
            413,
        ),
        ("DELETE", "/dns-query".into(), None, Vec::new(), 405),
        ("GET", "/other".into(), None, Vec::new(), 404),
    ] {
        let mut request = http::Request::builder()
            .method(method)
            .uri(format!("https://localhost{path}"))
            .header("x-forwarded-for", "203.0.113.99");
        if let Some(kind) = content_type {
            request = request.header("content-type", kind);
        }
        let (response, mut sender) = client
            .send_request(request.body(()).unwrap(), body.is_empty())
            .unwrap();
        if !body.is_empty() {
            sender.send_data(Bytes::from(body), true).unwrap();
        }
        let response = timeout(Duration::from_secs(3), response)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        if expected == 200 {
            assert_eq!(
                response.headers()["content-type"],
                "application/dns-message"
            );
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await {
                bytes.extend_from_slice(&chunk.unwrap());
            }
            assert_eq!(protocol::decode(&bytes).unwrap().id, 123);
        }
    }
    mock.await.unwrap();
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(connections.available_permits(), 2);
    assert_eq!(queries.available_permits(), 4);
    let _listener = TcpListener::bind(address).await.unwrap();
    drop(client);
    timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Alt-Svc names the HTTP/3 port only while that listener serves.
#[tokio::test]
async fn alt_svc_follows_the_http3_listener_state() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let ingress = Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default())),
        resolver: Arc::new(common::resolver(
            "127.0.0.1:9".parse().unwrap(),
            Duration::from_millis(100),
        )),
        queries: Arc::new(Semaphore::new(1)),
        connections: Arc::new(Semaphore::new(1)),
        stop: stopped,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_millis(150),
        max_streams: 2,
    };
    let serving = Arc::new(AtomicBool::new(true));
    let state = serving.clone();
    let h3_port: doh::H3Port = Arc::new(move || state.load(SeqCst).then_some(8443));
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, Some(h3_port)));
    let tls = TlsConnector::from(client_tls)
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut client, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    for (up, expected) in [(true, "h3=\":8443\"; ma=300"), (false, "clear")] {
        serving.store(up, SeqCst);
        let request = http::Request::builder()
            .uri("https://localhost/other")
            .body(())
            .unwrap();
        let (response, _) = client.send_request(request, true).unwrap();
        let response = timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()["alt-svc"], expected);
    }
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(client);
    let _ = timeout(Duration::from_secs(2), driver).await;
}

#[tokio::test]
async fn tls_handshake_failures_timeouts_and_alpn_mismatches_are_counted() {
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let resolver = Arc::new(common::resolver(
        "127.0.0.1:9".parse().unwrap(),
        Duration::from_millis(100),
    ));
    let ingress = Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default())),
        resolver: resolver.clone(),
        queries: Arc::new(Semaphore::new(4)),
        connections: Arc::new(Semaphore::new(8)),
        stop: stopped,
        io_timeout: Duration::from_millis(200),
        shutdown_grace: Duration::from_millis(200),
        max_streams: 4,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, None));
    // Plain HTTP is not a TLS handshake.
    let mut plain = TcpStream::connect(address).await.unwrap();
    plain
        .write_all(b"GET /dns-query HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    // A silent peer exceeds the I/O timeout.
    let _silent = TcpStream::connect(address).await.unwrap();
    // Without ALPN the handshake completes but selects no h2.
    let mut without = (*client_tls).clone();
    without.alpn_protocols.clear();
    let _without = TlsConnector::from(Arc::new(without))
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await;
    // An offer without h2 ends the handshake.
    let mut other = (*client_tls).clone();
    other.alpn_protocols = vec![b"http/1.1".to_vec()];
    assert!(
        TlsConnector::from(Arc::new(other))
            .connect(
                "localhost".try_into().unwrap(),
                TcpStream::connect(address).await.unwrap(),
            )
            .await
            .is_err()
    );
    let expected = [
        "parins_doh_tls_handshake_failures_total 1\n",
        "parins_doh_tls_handshake_timeouts_total 1\n",
        "parins_doh_alpn_mismatches_total 2\n",
    ];
    let counted = timeout(Duration::from_secs(2), async {
        loop {
            let text = parins::admin::render(resolver.metrics());
            if expected.iter().all(|line| text.contains(line)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        counted.is_ok(),
        "{}",
        parins::admin::render(resolver.metrics())
    );
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn incomplete_body_expires_and_shutdown_releases_admission() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(2));
    let queries = Arc::new(Semaphore::new(1));
    let ingress = Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default())),
        resolver: Arc::new(common::resolver(
            "127.0.0.1:9".parse().unwrap(),
            Duration::from_millis(100),
        )),
        queries: queries.clone(),
        connections: connections.clone(),
        stop: stopped,
        io_timeout: Duration::from_millis(150),
        shutdown_grace: Duration::from_millis(150),
        max_streams: 2,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, None));
    let tls = TlsConnector::from(client_tls)
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut client, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    let request = http::Request::builder()
        .method("POST")
        .uri("https://localhost/dns-query")
        .header("content-type", "application/dns-message")
        .body(())
        .unwrap();
    let (response, mut body) = client.send_request(request, false).unwrap();
    body.send_data(Bytes::from_static(&[0, 1]), false).unwrap();
    assert!(
        timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .is_err()
    );
    // A raw client that never starts TLS must not keep shutdown or admission alive.
    let _stalled_handshake = TcpStream::connect(address).await.unwrap();
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(connections.available_permits(), 2);
    assert_eq!(queries.available_permits(), 1);
    drop(client);
    drop(body);
    let _ = timeout(Duration::from_secs(2), driver).await.unwrap();
}

#[tokio::test]
async fn resetting_last_h2_waiter_releases_budget_and_its_sent_query_fills_the_cache() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(common::resolver(
        upstream.local_addr().unwrap(),
        Duration::from_secs(10),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let queries = Arc::new(Semaphore::new(1));
    let source_limits = Arc::new(parins::limits::Limiter::new(&parins::limits::Settings {
        enabled: true,
        max_inflight: 1,
        ..Default::default()
    }));
    let ingress = Ingress {
        source_limits: source_limits.clone(),
        resolver: resolver.clone(),
        queries: queries.clone(),
        connections: Arc::new(Semaphore::new(1)),
        stop: stopped,
        io_timeout: Duration::from_secs(10),
        shutdown_grace: Duration::from_millis(150),
        max_streams: 2,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, None));
    let tls = TlsConnector::from(client_tls)
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut client, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    let request = http::Request::builder()
        .method("POST")
        .uri("https://localhost/dns-query")
        .header("content-type", "application/dns-message")
        .body(())
        .unwrap();
    let (response, mut sender) = client.send_request(request, false).unwrap();
    sender.send_data(Bytes::from(query()), true).unwrap();
    let mut buffer = [0; 4096];
    let (length, upstream_peer) = timeout(Duration::from_secs(2), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queries.available_permits(), 0);
    sender.send_reset(h2::Reason::CANCEL);
    // This deadline is deliberately shorter than either 10-second operation timeout.
    timeout(Duration::from_secs(1), async {
        while queries.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = resolver.metrics().snapshot();
    assert_eq!(snapshot.counters["cancelled"], 1);
    assert_eq!(snapshot.request_inflight, 0);
    assert!(source_limits.try_query(address.ip()).is_ok());
    // The sent query goes on in the refresh budget, only to fill the cache.
    assert_eq!(snapshot.upstream_inflight, 1);
    assert_eq!(resolver.refresh_snapshot().active, 1);
    let sent = protocol::decode(&buffer[..length]).unwrap();
    let mut answer = protocol::error_response(&sent, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        Name::from_ascii("doh.test.").unwrap(),
        300,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    upstream
        .send_to(&answer.to_vec().unwrap(), upstream_peer)
        .await
        .unwrap();
    timeout(Duration::from_secs(1), async {
        while resolver.refresh_snapshot().active != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = resolver.metrics().snapshot();
    assert_eq!(snapshot.upstream_inflight, 0);
    assert_eq!(snapshot.counters["cache_store_admitted"], 1);
    assert_eq!(resolver.refresh_snapshot().success, 1);
    // The next query of the same key is a cache hit without upstream IO.
    let reply = resolver
        .resolve_with_transport(&query(), address.ip(), "doh")
        .await
        .unwrap();
    assert_eq!(reply.message.answers.len(), 1);
    assert_eq!(
        resolver.metrics().snapshot().counters["upstream_operations"],
        1
    );
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop((client, response, sender));
    let _ = timeout(Duration::from_secs(2), driver).await.unwrap();
}

#[tokio::test]
async fn h2_source_limits_cover_pre_tls_admission_and_ignore_forwarded_identity() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(common::resolver(
        upstream.local_addr().unwrap(),
        Duration::from_secs(2),
    ));
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let query = protocol::decode(&buffer[..length]).unwrap();
        let answer = protocol::error_response(&query, ResponseCode::NoError);
        upstream
            .send_to(&answer.to_vec().unwrap(), peer)
            .await
            .unwrap();
        // Retain the socket: an incorrectly admitted second query cannot get a
        // successful answer or be confused with the expected rate rejection.
        upstream
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(3));
    let source_limits = Arc::new(parins::limits::Limiter::new(&parins::limits::Settings {
        enabled: true,
        max_connections: 1,
        burst: 1,
        rate_per_sec: 1,
        ..Default::default()
    }));
    let ingress = Ingress {
        source_limits: source_limits.clone(),
        resolver: resolver.clone(),
        queries: Arc::new(Semaphore::new(4)),
        connections: connections.clone(),
        stop: stopped,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_millis(150),
        max_streams: 4,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress, None));
    // One raw TCP client already consumes its source's connection budget even
    // though it has sent no TLS ClientHello. Global capacity still has room.
    let stalled = TcpStream::connect(address).await.unwrap();
    timeout(Duration::from_secs(1), async {
        while connections.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let connector = TlsConnector::from(client_tls);
    let rejected = timeout(
        Duration::from_secs(1),
        connector.connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        ),
    )
    .await
    .unwrap();
    assert!(rejected.is_err());
    assert_eq!(
        resolver.metrics().snapshot().counters["source_connections_rejected"],
        1
    );
    assert_eq!(connections.available_permits(), 2);
    drop(stalled);
    timeout(Duration::from_secs(1), async {
        while connections.available_permits() != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let tls = connector
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut client, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    timeout(Duration::from_millis(900), async {
        for (index, expected) in [ResponseCode::NoError, ResponseCode::ServFail]
            .into_iter()
            .enumerate()
        {
            let mut message = protocol::decode(&query()).unwrap();
            if index == 1 {
                ecs::set_subnet(
                    &mut message,
                    Some(hickory_proto::rr::rdata::opt::ClientSubnet::new(
                        "203.0.113.0".parse().unwrap(),
                        24,
                        0,
                    )),
                );
            }
            let request = http::Request::builder()
                .method("POST")
                .uri("https://localhost/dns-query")
                .header("content-type", "application/dns-message")
                .header("x-forwarded-for", format!("203.0.113.{}", index + 1))
                .header("forwarded", format!("for=203.0.113.{}", index + 1))
                .body(())
                .unwrap();
            let (response, mut sender) = client.send_request(request, false).unwrap();
            sender
                .send_data(Bytes::from(message.to_vec().unwrap()), true)
                .unwrap();
            let response = response.await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.headers()["content-type"],
                "application/dns-message"
            );
            let mut body = response.into_body();
            let mut answer = Vec::new();
            while let Some(data) = body.data().await {
                answer.extend_from_slice(&data.unwrap());
            }
            let answer = protocol::decode(&answer).unwrap();
            assert_eq!(answer.id, 123);
            assert_eq!(answer.response_code, expected);
        }
    })
    .await
    .unwrap();
    assert_eq!(
        resolver.metrics().snapshot().counters["source_queries_rejected"],
        1
    );
    let upstream = timeout(Duration::from_secs(1), mock)
        .await
        .unwrap()
        .unwrap();
    let mut buffer = [0; 4096];
    assert!(
        matches!(upstream.try_recv_from(&mut buffer), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(connections.available_permits(), 3);
    assert!(source_limits.try_connection(address.ip()).is_ok());
    drop(client);
    let _ = timeout(Duration::from_secs(2), driver).await.unwrap();
}
