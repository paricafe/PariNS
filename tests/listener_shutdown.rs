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
    let address = server.local_addr().unwrap();
    let encrypted = server.encrypted_addrs().unwrap();
    assert_eq!(
        encrypted.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["dot", "doh3", "doh", "doq"]
    );
    assert_eq!(encrypted[1].1, encrypted[2].1);
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
async fn stop_with_silent_doh_peer(idle: bool) -> (bool, std::time::Duration) {
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
    let doh = server.encrypted_addrs().unwrap()[0].1;
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
    if !idle {
        // The upstream never answers: this request is still resolving at stop.
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
        timeout(WAIT, async {
            tokio::select! {
                received = upstream.recv_from(&mut buffer) => received.unwrap(),
                _ = &mut connection => panic!("peer connection closed"),
            }
        })
        .await
        .unwrap();
    }
    let started = std::time::Instant::now();
    finish(stop, task).await;
    let elapsed = started.elapsed();
    drop((sender, connection));
    (resolver.is_quiescent(), elapsed)
}

/// An h2 peer that completed its handshake but no longer reads cannot answer
/// the graceful-shutdown PING. It owns no resolver work, so stopping must stay
/// quiescent and within the shutdown grace instead of waiting for the peer.
#[tokio::test]
async fn silent_idle_doh_peer_keeps_shutdown_quiescent() {
    let (quiescent, elapsed) = stop_with_silent_doh_peer(true).await;
    // Reaching the grace deadline would force the stop, so quiescence alone
    // proves that the silent peer did not hold shutdown.
    assert!(quiescent, "idle DoH peer forced shutdown after {elapsed:?}");
}

/// Resolution still running past the grace keeps the existing contract: the
/// stop is forced and the cache is not publishable.
#[tokio::test]
async fn unfinished_doh_resolution_still_forces_shutdown() {
    let (quiescent, _) = stop_with_silent_doh_peer(false).await;
    assert!(!quiescent);
}
