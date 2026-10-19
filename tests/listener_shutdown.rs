//! An isolated listener-shutdown assertion without parallel runtime tests or subprocess fixtures.
#[path = "support/runtime.rs"]
#[allow(dead_code)] // The shared fixture also serves the broader runtime suite.
mod runtime_support;

use parins::{ecs, protocol, server::Server};
use runtime_support::{Certificate, WAIT, answer, config, finish, query, read, run, udp, write};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    time::timeout,
};

#[tokio::test]
async fn all_listeners_bind_together_and_udp_dot_tcp_share_peer_ecs_cache() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    // Global query capacity can exceed the QUIC per-connection stream cap.
    config.max_inflight = 2048;
    config.ecs.enabled = true;
    config.dot = Some(cert.listener());
    config.doh = Some(parins::config::DohConfig {
        listen: cert.listener().listen,
        files: cert.listener().files,
        http3: true,
    });
    config.doq = Some(cert.listener());
    config.admin_listen = Some("127.0.0.1:0".parse().unwrap());
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr();
    let encrypted = server.encrypted_addrs();
    assert_eq!(
        encrypted.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["dot", "doh", "doh3", "doq"]
    );
    assert_eq!(encrypted[1].1, encrypted[2].1);
    let dot = encrypted[0].1;
    let admin = server.admin_addr().unwrap();
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
            TcpListener::bind(address)
                .await
                .unwrap_or_else(|error| panic!("failed to rebind {name} at {address}: {error}"));
        } else {
            UdpSocket::bind(address)
                .await
                .unwrap_or_else(|error| panic!("failed to rebind {name} at {address}: {error}"));
        }
    }
    TcpListener::bind(admin).await.unwrap();
}

/// Stops a DoH server while a peer that completed TLS/h2 no longer reads.
async fn stop_with_silent_doh_peer(mode: PeerMode) -> (bool, std::time::Duration) {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    config.tcp_io_timeout_ms = 5000;
    config.shutdown_grace_ms = 300;
    config.doh = Some(parins::config::DohConfig {
        listen: cert.listener().listen,
        files: cert.listener().files,
        http3: false,
    });
    let server = Server::bind(config).await.unwrap();
    let resolver = server.resolver().clone();
    let doh = server.encrypted_addrs()[0].1;
    let (stop, task) = run(server);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client))
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(doh).await.unwrap(),
        )
        .await
        .unwrap();
    // Drive the handshake once, then keep the connection open without polling it.
    let (mut sender, mut connection) = h2::client::handshake(tls).await.unwrap();
    assert!(
        timeout(std::time::Duration::from_millis(200), &mut connection)
            .await
            .is_err()
    );
    let mut received = None;
    if mode != PeerMode::Idle {
        // The upstream has not answered: this request is still resolving at stop.
        let request = http::Request::builder()
            .uri(format!(
                "https://localhost/dns-query?dns={}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                    query(7).to_vec().unwrap()
                )
            ))
            .body(())
            .unwrap();
        let _response = sender.send_request(request, true).unwrap();
        let mut buffer = [0; 512];
        // Polling the client connection sends the request until the upstream sees it.
        let (length, peer) = timeout(WAIT, async {
            tokio::select! {
                received = upstream.recv_from(&mut buffer) => received.unwrap(),
                _ = &mut connection => panic!("peer connection closed"),
            }
        })
        .await
        .unwrap();
        received = Some((protocol::decode(&buffer[..length]).unwrap(), peer));
    }
    let started = std::time::Instant::now();
    if mode == PeerMode::ClosesAfterStop {
        // The upstream answers 150ms after receiving the query, well within the
        // grace. The peer closes once the server has begun its graceful stop.
        let (query, peer) = received.unwrap();
        let answering = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            upstream
                .send_to(&answer(&query).to_vec().unwrap(), peer)
                .await
                .unwrap();
        });
        stop.send(()).unwrap();
        let _ = timeout(std::time::Duration::from_millis(50), &mut connection).await;
        drop((sender, connection));
        timeout(WAIT, task).await.unwrap().unwrap().unwrap();
        answering.await.unwrap();
        return (resolver.is_quiescent(), started.elapsed());
    }
    finish(stop, task).await;
    let elapsed = started.elapsed();
    drop((sender, connection));
    (resolver.is_quiescent(), elapsed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerMode {
    /// Completes TLS/h2, then never reads or writes again.
    Idle,
    /// Leaves a request resolving (the upstream never answers) at stop.
    Resolving,
    /// Leaves a request resolving, then closes after the stop has begun.
    ClosesAfterStop,
}

/// An h2 peer that completed its handshake but no longer reads cannot answer
/// the graceful-shutdown PING. It owns no resolver work, so stopping must stay
/// quiescent and within the shutdown grace instead of waiting for the peer.
#[tokio::test]
async fn silent_idle_doh_peer_keeps_shutdown_quiescent() {
    let (quiescent, elapsed) = stop_with_silent_doh_peer(PeerMode::Idle).await;
    // Reaching the grace deadline would force the stop, so quiescence alone
    // proves that the silent peer did not hold shutdown.
    assert!(quiescent, "idle DoH peer forced shutdown after {elapsed:?}");
}

/// A peer closing during the stop must not turn a request that finishes within
/// the grace into a forced shutdown.
#[tokio::test]
async fn doh_peer_closing_during_stop_lets_admitted_request_finish() {
    let (quiescent, elapsed) = stop_with_silent_doh_peer(PeerMode::ClosesAfterStop).await;
    assert!(quiescent, "closed peer forced shutdown after {elapsed:?}");
}

/// Resolution still running past the grace keeps the existing contract: the
/// stop is forced and the cache is not publishable.
#[tokio::test]
async fn unfinished_doh_resolution_still_forces_shutdown() {
    let (quiescent, _) = stop_with_silent_doh_peer(PeerMode::Resolving).await;
    assert!(!quiescent);
}

/// A QUIC peer whose handshake never completes (here: it completes address
/// validation, but no later server packet reaches it) owns no resolver work.
/// Stopping must not wait for the handshake timeout, so the stop stays quiescent
/// and the clean snapshot remains possible.
#[tokio::test]
async fn unfinished_quic_handshake_keeps_shutdown_quiescent() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap());
    config.tcp_io_timeout_ms = 5000;
    config.shutdown_grace_ms = 300;
    config.doq = Some(cert.listener());
    let server = Server::bind(config).await.unwrap();
    let resolver = server.resolver().clone();
    let metrics = server.metrics().clone();
    let doq = server.encrypted_addrs()[0].1;
    let (stop, task) = run(server);
    // Relay every client datagram but only the server's first reply (its Retry):
    // the client's validated Initial starts a handshake that never completes.
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_address = relay.local_addr().unwrap();
    let forward = tokio::spawn(async move {
        let mut buffer = [0; 2048];
        let mut client = None;
        let mut retried = false;
        loop {
            let (length, from) = relay.recv_from(&mut buffer).await.unwrap();
            if from != doq {
                client = Some(from);
                relay.send_to(&buffer[..length], doq).await.unwrap();
            } else if !retried && let Some(client) = client {
                retried = true;
                relay.send_to(&buffer[..length], client).await.unwrap();
            }
        }
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"doq".to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(crypto)));
    let connecting = client.connect(relay_address, "localhost").unwrap();
    timeout(WAIT, async {
        while metrics.quic.snapshot()["doq"]["handshake_inflight"] == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    finish(stop, task).await;
    assert!(
        resolver.is_quiescent(),
        "unfinished QUIC handshake forced shutdown"
    );
    assert_eq!(metrics.quic.snapshot()["doq"]["handshake_shutdown"], 1);
    assert_eq!(metrics.quic.snapshot()["doq"]["incoming"], 1);
    forward.abort();
    drop(connecting);
    client.close(0u32.into(), b"done");
}

/// A server whose grace is long enough that waiting on it would be visible:
/// the stop must finish well before it, and without forcing.
async fn incomplete_query_server(
    cert: &Certificate,
    upstream: &UdpSocket,
    doh: bool,
    doq: bool,
) -> Server {
    let mut config = config(upstream.local_addr().unwrap());
    config.tcp_io_timeout_ms = 5000;
    config.shutdown_grace_ms = 2000;
    if doh {
        config.doh = Some(parins::config::DohConfig {
            listen: cert.listener().listen,
            files: cert.listener().files,
            http3: true,
        });
    }
    if doq {
        config.doq = Some(cert.listener());
    }
    Server::bind(config).await.unwrap()
}

const GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

fn encrypted(server: &Server, name: &str) -> std::net::SocketAddr {
    server
        .encrypted_addrs()
        .into_iter()
        .find(|(listener, _)| *listener == name)
        .unwrap()
        .1
}

fn quic_client(cert: &Certificate, alpn: &[u8]) -> quinn::Endpoint {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(crypto)));
    client
}

async fn wait_for_stream(server_metrics: &parins::metrics::Metrics, protocol: &str) {
    timeout(WAIT, async {
        while server_metrics.quic.snapshot()[protocol]["stream_started"] == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Let the adapter reach its query read before the stop.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// A DoH POST whose body has not fully arrived owns no resolver work: the stop
/// refuses it at once instead of waiting for the grace and forcing.
#[tokio::test]
async fn doh_post_with_partial_body_is_cancelled_at_stop() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = incomplete_query_server(&cert, &upstream, true, false).await;
    let resolver = server.resolver().clone();
    let doh = encrypted(&server, "doh");
    let (stop, task) = run(server);
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client))
        .connect(
            "localhost".try_into().unwrap(),
            TcpStream::connect(doh).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut sender, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    let request = http::Request::builder()
        .method("POST")
        .uri("https://localhost/dns-query")
        .header("content-type", "application/dns-message")
        .body(())
        .unwrap();
    let (response, mut body) = sender.send_request(request, false).unwrap();
    body.send_data(bytes::Bytes::from_static(&[0, 1]), false)
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    finish(stop, task).await;
    let elapsed = started.elapsed();
    let reset = timeout(WAIT, response).await.unwrap().unwrap_err();
    assert_eq!(reset.reason(), Some(h2::Reason::REFUSED_STREAM));
    assert!(resolver.is_quiescent(), "partial DoH body forced shutdown");
    assert!(
        elapsed < GRACE,
        "partial DoH body held the stop for {elapsed:?}"
    );
    drop((sender, body));
    let _ = timeout(WAIT, driver).await.unwrap();
}

/// A DoQ stream without FIN has no complete query and is cancelled at the stop.
#[tokio::test]
async fn doq_stream_without_fin_is_cancelled_at_stop() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = incomplete_query_server(&cert, &upstream, false, true).await;
    let resolver = server.resolver().clone();
    let metrics = server.metrics().clone();
    let doq = encrypted(&server, "doq");
    let (stop, task) = run(server);
    let client = quic_client(&cert, b"doq");
    let connection = client.connect(doq, "localhost").unwrap().await.unwrap();
    let (mut send, _recv) = connection.open_bi().await.unwrap();
    let frame = query(9).to_vec().unwrap();
    send.write_all(&(frame.len() as u16).to_be_bytes())
        .await
        .unwrap();
    send.write_all(&frame).await.unwrap();
    wait_for_stream(&metrics, "doq").await;
    let started = std::time::Instant::now();
    finish(stop, task).await;
    let elapsed = started.elapsed();
    assert!(
        resolver.is_quiescent(),
        "DoQ stream without FIN forced shutdown"
    );
    assert!(
        elapsed < GRACE,
        "DoQ stream without FIN held the stop for {elapsed:?}"
    );
    assert_eq!(metrics.quic.snapshot()["doq"]["stream_shutdown"], 1);
    client.close(0u32.into(), b"done");
}

/// An HTTP/3 POST whose body has not ended is cancelled at the stop.
#[tokio::test]
async fn h3_post_with_partial_body_is_cancelled_at_stop() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = incomplete_query_server(&cert, &upstream, true, false).await;
    let resolver = server.resolver().clone();
    let metrics = server.metrics().clone();
    let h3_address = encrypted(&server, "doh3");
    let (stop, task) = run(server);
    let client = quic_client(&cert, b"h3");
    let connection = client
        .connect(h3_address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut driver, mut sender) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .unwrap();
    let drive =
        tokio::spawn(
            async move { futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await },
        );
    let request = http::Request::builder()
        .method("POST")
        .uri("https://localhost/dns-query")
        .header("content-type", "application/dns-message")
        .body(())
        .unwrap();
    let mut stream = sender.send_request(request).await.unwrap();
    stream
        .send_data(bytes::Bytes::from_static(&[0, 1]))
        .await
        .unwrap();
    wait_for_stream(&metrics, "doh3").await;
    let started = std::time::Instant::now();
    finish(stop, task).await;
    let elapsed = started.elapsed();
    assert!(resolver.is_quiescent(), "partial H3 body forced shutdown");
    assert!(
        elapsed < GRACE,
        "partial H3 body held the stop for {elapsed:?}"
    );
    assert_eq!(metrics.quic.snapshot()["doh3"]["stream_shutdown"], 1);
    drop((stream, sender));
    let _ = timeout(WAIT, drive).await.unwrap();
    client.close(0u32.into(), b"done");
}

/// A DoQ query received in full before the stop still completes within the
/// grace, and the stop stays quiescent.
#[tokio::test]
async fn complete_doq_query_resolving_at_stop_completes_within_grace() {
    let cert = Certificate::new();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = incomplete_query_server(&cert, &upstream, false, true).await;
    let resolver = server.resolver().clone();
    let metrics = server.metrics().clone();
    let doq = encrypted(&server, "doq");
    let (stop, task) = run(server);
    let client = quic_client(&cert, b"doq");
    let connection = client.connect(doq, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    let mut frame = query(0).to_vec().unwrap();
    frame.splice(0..0, (frame.len() as u16).to_be_bytes());
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let mut buffer = [0; 512];
    let (length, peer) = timeout(WAIT, upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let forwarded = protocol::decode(&buffer[..length]).unwrap();
    // The query is resolving at the stop; the upstream answers within the grace.
    let answering = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        upstream
            .send_to(&answer(&forwarded).to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    stop.send(()).unwrap();
    let response = timeout(WAIT, recv.read_to_end(65537))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(protocol::decode(&response[2..]).unwrap().answers.len(), 1);
    timeout(WAIT, task).await.unwrap().unwrap().unwrap();
    answering.await.unwrap();
    assert!(
        resolver.is_quiescent(),
        "complete DoQ query forced shutdown"
    );
    assert_eq!(
        metrics.quic.snapshot()["doq"]["stream_response_handed_to_transport"],
        1
    );
    client.close(0u32.into(), b"done");
}
