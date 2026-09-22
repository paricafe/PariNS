use std::{sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{config::Config, doh, ecs, ingress::Ingress, protocol, resolver::Resolver};
use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::PrivatePkcs8KeyDer};
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
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
        )
        .unwrap();
    server.alpn_protocols = vec![b"h2".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let mut client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];
    (Arc::new(server), Arc::new(client))
}

#[tokio::test]
async fn real_h2_tls_get_post_errors_and_shutdown() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstream = upstream.local_addr().unwrap();
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
        resolver,
        queries: queries.clone(),
        connections: connections.clone(),
        stop: stopped,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_millis(300),
        max_streams: 4,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress));
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

#[tokio::test]
async fn incomplete_body_expires_and_shutdown_releases_admission() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(2));
    let queries = Arc::new(Semaphore::new(1));
    let ingress = Ingress {
        resolver: Arc::new(Resolver::new(
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
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress));
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
async fn resetting_last_h2_waiter_cancels_upstream_and_releases_query_budget() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(Resolver::new(
        upstream.local_addr().unwrap(),
        Duration::from_secs(10),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let queries = Arc::new(Semaphore::new(1));
    let ingress = Ingress {
        resolver: resolver.clone(),
        queries: queries.clone(),
        connections: Arc::new(Semaphore::new(1)),
        stop: stopped,
        io_timeout: Duration::from_secs(10),
        shutdown_grace: Duration::from_millis(150),
        max_streams: 2,
    };
    let (server_tls, client_tls) = certificates();
    let server = tokio::spawn(doh::serve(listener, server_tls, ingress));
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
    let (_, upstream_peer) = timeout(Duration::from_secs(2), upstream.recv_from(&mut buffer))
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
    assert_eq!(snapshot.upstream_inflight, 0);
    // No detached shared operation retains the last waiter's UDP socket.
    let _released_socket = UdpSocket::bind(upstream_peer).await.unwrap();
    stop.send(true).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop((client, response, sender));
    let _ = timeout(Duration::from_secs(2), driver).await.unwrap();
}
