use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::{Buf, Bytes};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{
    config::Config,
    ingress::Ingress,
    protocol,
    quic::{self, Protocol},
    resolver::Resolver,
};
use tokio::{
    net::UdpSocket,
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};

struct Fixture {
    address: SocketAddr,
    roots: rustls::RootCertStore,
    stop: watch::Sender<bool>,
    server: JoinHandle<anyhow::Result<()>>,
    upstream: JoinHandle<()>,
    calls: Arc<AtomicUsize>,
    connections: Arc<Semaphore>,
    connection_capacity: usize,
    source_limits: Arc<parins::limits::Limiter>,
    resolver: Arc<Resolver>,
    queries: Arc<Semaphore>,
}

impl Fixture {
    async fn new(protocol: Protocol) -> Self {
        Self::with_dropped_first_query(protocol, false).await
    }

    async fn with_dropped_first_query(protocol: Protocol, drop_first: bool) -> Self {
        Self::with_limits(protocol, drop_first, Default::default(), 1).await
    }

    async fn with_limits(
        protocol: Protocol,
        drop_first: bool,
        settings: parins::limits::Settings,
        connection_capacity: usize,
    ) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = certified.cert.der().clone();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap();
        tls.alpn_protocols = vec![match protocol {
            Protocol::Doq => b"doq".to_vec(),
            Protocol::H3 => b"h3".to_vec(),
        }];
        let endpoint =
            quic::bind("127.0.0.1:0".parse().unwrap(), Arc::new(tls), 2, protocol).unwrap();
        let address = endpoint.local_addr().unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.upstreams.servers = vec![udp.local_addr().unwrap().to_string()];
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream_calls = calls.clone();
        let upstream = tokio::spawn(async move {
            let mut buffer = [0u8; 65535];
            loop {
                let (len, peer) = udp.recv_from(&mut buffer).await.unwrap();
                let previous = upstream_calls.fetch_add(1, Ordering::SeqCst);
                if drop_first && previous == 0 {
                    continue;
                }
                let query = Message::from_vec(&buffer[..len]).unwrap();
                let response = protocol::error_response(&query, ResponseCode::NoError)
                    .to_vec()
                    .unwrap();
                udp.send_to(&response, peer).await.unwrap();
            }
        });
        let (stop, receiver) = watch::channel(false);
        let connections = Arc::new(Semaphore::new(connection_capacity));
        let source_limits = Arc::new(parins::limits::Limiter::new(&settings).unwrap());
        let resolver = Arc::new(Resolver::from_config(&config));
        let queries = Arc::new(Semaphore::new(8));
        let ingress = Ingress {
            source_limits: source_limits.clone(),
            resolver: resolver.clone(),
            queries: queries.clone(),
            connections: connections.clone(),
            stop: receiver,
            io_timeout: Duration::from_millis(500),
            shutdown_grace: Duration::from_millis(100),
            max_streams: 2,
        };
        let server = tokio::spawn(quic::serve(endpoint, protocol, ingress));
        Self {
            address,
            roots,
            stop,
            server,
            upstream,
            calls,
            connections,
            connection_capacity,
            source_limits,
            resolver,
            queries,
        }
    }

    async fn wait_for_upstream_query(&self) {
        timeout(Duration::from_millis(200), async {
            while self.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.resolver.metrics().snapshot().upstream_inflight, 1);
        assert_eq!(self.resolver.metrics().snapshot().request_inflight, 1);
        assert_eq!(self.queries.available_permits(), 7);
    }

    async fn wait_for_cancelled_query(&self) {
        // Less than both the 500ms adapter and 2000ms resolver deadlines.
        timeout(Duration::from_millis(200), async {
            loop {
                let metrics = self.resolver.metrics().snapshot();
                if metrics.upstream_inflight == 0
                    && metrics.request_inflight == 0
                    && self.queries.available_permits() == 8
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn client(&self, protocol: Protocol) -> quinn::Endpoint {
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(self.roots.clone())
            .with_no_client_auth();
        tls.alpn_protocols = vec![match protocol {
            Protocol::Doq => b"doq".to_vec(),
            Protocol::H3 => b"h3".to_vec(),
        }];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        endpoint
    }

    async fn close(self) {
        self.stop.send(true).unwrap();
        timeout(Duration::from_secs(1), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            self.connections.available_permits(),
            self.connection_capacity
        );
        // Source admission is released as well as the global semaphore.
        assert!(self.source_limits.try_connection(self.address.ip()).is_ok());
        self.upstream.abort();
        let _ = self.upstream.await;
    }
}

fn query(id: u16) -> Vec<u8> {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(
        Name::from_ascii("example.test.").unwrap(),
        RecordType::A,
    ));
    message.to_vec().unwrap()
}

fn frame(id: u16) -> Vec<u8> {
    let query = query(id);
    let mut frame = (query.len() as u16).to_be_bytes().to_vec();
    frame.extend(query);
    frame
}

#[tokio::test]
async fn doq_real_tls_round_trip_and_connection_reuse() {
    let fixture = Fixture::new(Protocol::Doq).await;
    let client = fixture.client(Protocol::Doq);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    for _ in 0..2 {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(&frame(0)).await.unwrap();
        send.finish().unwrap();
        let response = timeout(Duration::from_secs(1), recv.read_to_end(65537))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            u16::from_be_bytes([response[0], response[1]]) as usize,
            response.len() - 2
        );
        let message = Message::from_vec(&response[2..]).unwrap();
        assert_eq!(message.id, 0);
        assert_eq!(message.response_code, ResponseCode::NoError);
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let counters = fixture.resolver.metrics().snapshot().counters;
    assert_eq!(counters["quic_doq_handshake_established"], 1);
    assert_eq!(counters["quic_doq_stream_full_frame"], 2);
    assert_eq!(counters["quic_doq_stream_response_handed_to_transport"], 2);
    assert_eq!(
        fixture.resolver.metrics().quic.snapshot()["doq"]["stream_inflight"],
        0
    );
    fixture.close().await;
    assert!(
        timeout(Duration::from_secs(1), connection.closed())
            .await
            .is_ok()
    );
    client.close(0u32.into(), b"done");
}

#[tokio::test]
async fn doq_rejects_nonzero_id_extra_frames_and_missing_fin_before_dns() {
    for case in 0..6 {
        let fixture = Fixture::new(Protocol::Doq).await;
        let client = fixture.client(Protocol::Doq);
        let connection = client
            .connect(fixture.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut send, _recv) = connection.open_bi().await.unwrap();
        let mut bytes = frame(if case == 0 { 1 } else { 0 });
        if case == 1 {
            bytes.extend(frame(0));
        }
        if case == 3 {
            bytes = vec![0, 11, 0, 0];
        }
        if case == 4 {
            bytes[1] += 1;
        }
        if case == 5 {
            let mut message = Message::from_vec(&query(0)).unwrap();
            let mut edns = hickory_proto::op::Edns::new();
            edns.options_mut()
                .insert(hickory_proto::rr::rdata::opt::EdnsOption::Unknown(
                    11,
                    Vec::new(),
                ));
            message.edns = Some(edns);
            let encoded = message.to_vec().unwrap();
            bytes = (encoded.len() as u16).to_be_bytes().to_vec();
            bytes.extend(encoded);
        }
        send.write_all(&bytes).await.unwrap();
        if case != 2 {
            send.finish().unwrap();
        }
        let error = timeout(Duration::from_secs(2), connection.closed())
            .await
            .unwrap();
        assert!(matches!(
            error,
            quinn::ConnectionError::ApplicationClosed(_)
        ));
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        let counters = fixture.resolver.metrics().snapshot().counters;
        assert_eq!(
            counters["quic_doq_stream_protocol_invalid"],
            u64::from(case != 2)
        );
        assert_eq!(
            counters["quic_doq_stream_read_deadline"],
            u64::from(case == 2)
        );
        assert_eq!(counters["requests"], 0);
        fixture.close().await;
        client.close(0u32.into(), b"done");
    }
}

#[tokio::test]
async fn doq_stream_cancellation_preserves_other_transactions() {
    let fixture = Fixture::new(Protocol::Doq).await;
    let client = fixture.client(Protocol::Doq);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut cancelled, mut abandoned) = connection.open_bi().await.unwrap();
    cancelled.write_all(&[0]).await.unwrap();
    cancelled.reset(3u32.into()).unwrap();
    abandoned.stop(3u32.into()).unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(&frame(0)).await.unwrap();
    send.finish().unwrap();
    let answer = timeout(Duration::from_secs(1), recv.read_to_end(65537))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Message::from_vec(&answer[2..]).unwrap().response_code,
        ResponseCode::NoError
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    fixture.close().await;
    client.close(0u32.into(), b"done");
}

#[tokio::test]
async fn quic_connection_and_stream_caps_and_bounded_shutdown() {
    let fixture = Fixture::new(Protocol::Doq).await;
    let client = fixture.client(Protocol::Doq);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let rejected = timeout(
        Duration::from_secs(1),
        client.connect(fixture.address, "localhost").unwrap(),
    )
    .await
    .unwrap();
    assert!(rejected.is_err());
    let (mut first, _r1) = connection.open_bi().await.unwrap();
    first.write_all(&[0]).await.unwrap();
    let (mut second, _r2) = connection.open_bi().await.unwrap();
    second.write_all(&[0]).await.unwrap();
    assert!(
        timeout(Duration::from_millis(30), connection.open_bi())
            .await
            .is_err()
    );
    fixture.close().await;
    assert!(
        timeout(Duration::from_secs(1), connection.closed())
            .await
            .is_ok()
    );
    client.close(0u32.into(), b"done");
}

#[tokio::test]
async fn h3_real_get_post_and_http_errors() {
    let fixture = Fixture::new(Protocol::H3).await;
    let client = fixture.client(Protocol::H3);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut driver, mut sender) = h3::client::new(h3_quinn::Connection::new(connection.clone()))
        .await
        .unwrap();
    let drive =
        tokio::spawn(
            async move { futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await },
        );
    for (method, path, content_type, body, expected) in [
        (
            "GET",
            format!("/dns-query?dns={}", URL_SAFE_NO_PAD.encode(query(23))),
            None,
            Vec::new(),
            200,
        ),
        (
            "POST",
            "/dns-query".into(),
            Some("application/dns-message"),
            query(42),
            200,
        ),
        (
            "POST",
            "/dns-query".into(),
            Some("text/plain"),
            query(42),
            415,
        ),
        ("GET", "/wrong".into(), None, Vec::new(), 404),
        (
            "POST",
            "/dns-query".into(),
            Some("application/dns-message"),
            vec![0; 65536],
            413,
        ),
    ] {
        let mut request = http::Request::builder()
            .method(method)
            .uri(format!("https://localhost{path}"));
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let mut stream = sender
            .send_request(request.body(()).unwrap())
            .await
            .unwrap();
        if !body.is_empty() {
            stream.send_data(Bytes::from(body.clone())).await.unwrap();
        }
        stream.finish().await.unwrap();
        let response = timeout(Duration::from_secs(1), stream.recv_response())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        if expected == 200 {
            assert_eq!(
                response.headers()["content-type"],
                "application/dns-message"
            );
            let mut answer = Vec::new();
            while let Some(mut data) = stream.recv_data().await.unwrap() {
                let count = data.remaining();
                answer.extend_from_slice(&data.copy_to_bytes(count));
            }
            let message = Message::from_vec(&answer).unwrap();
            assert_eq!(message.id, if method == "GET" { 23 } else { 42 });
            assert_eq!(message.response_code, ResponseCode::NoError);
        }
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    timeout(Duration::from_secs(1), async {
        loop {
            let counters = fixture.resolver.metrics().snapshot().counters;
            if counters["quic_doh3_http_response_2xx"] == 2
                && counters["quic_doh3_http_response_4xx"] == 3
            {
                assert_eq!(counters["quic_doh3_stream_protocol_invalid"], 0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.close().await;
    connection.close(0u32.into(), b"done");
    client.close(0u32.into(), b"done");
    timeout(Duration::from_secs(1), drive)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn doq_inflight_stop_sending_cancels_dns_and_preserves_connection() {
    let fixture = Fixture::with_limits(
        Protocol::Doq,
        true,
        parins::limits::Settings {
            enabled: true,
            max_inflight: 1,
            ..Default::default()
        },
        1,
    )
    .await;
    let client = fixture.client(Protocol::Doq);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(&frame(0)).await.unwrap();
    send.finish().unwrap();
    fixture.wait_for_upstream_query().await;
    recv.stop(3u32.into()).unwrap();
    fixture.wait_for_cancelled_query().await;
    assert_eq!(
        fixture.resolver.metrics().snapshot().counters["quic_doq_stream_peer_cancelled"],
        1
    );
    assert!(
        fixture
            .source_limits
            .try_query(fixture.address.ip())
            .is_ok()
    );
    // The first query really reached the upstream, but its stream-local
    // cancellation must not poison either the connection or singleflight key.
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(&frame(0)).await.unwrap();
    send.finish().unwrap();
    let response = timeout(Duration::from_secs(1), recv.read_to_end(65537))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Message::from_vec(&response[2..]).unwrap().response_code,
        ResponseCode::NoError
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    fixture.close().await;
    client.close(0u32.into(), b"done");
}

#[tokio::test]
async fn h3_connection_loss_cancels_inflight_dns_before_deadline() {
    let fixture = Fixture::with_limits(
        Protocol::H3,
        true,
        parins::limits::Settings {
            enabled: true,
            max_inflight: 1,
            max_connections: 1,
            ..Default::default()
        },
        1,
    )
    .await;
    let client = fixture.client(Protocol::H3);
    let connection = client
        .connect(fixture.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut driver, mut sender) = h3::client::new(h3_quinn::Connection::new(connection.clone()))
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
    stream.send_data(Bytes::from(query(42))).await.unwrap();
    stream.finish().await.unwrap();
    fixture.wait_for_upstream_query().await;
    connection.close(0x100u32.into(), b"cancel connection");
    fixture.wait_for_cancelled_query().await;
    assert!(
        fixture
            .source_limits
            .try_query(fixture.address.ip())
            .is_ok()
    );
    timeout(Duration::from_millis(200), async {
        while fixture.connections.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    timeout(Duration::from_secs(1), drive)
        .await
        .unwrap()
        .unwrap();
    fixture.close().await;
    client.close(0u32.into(), b"done");
}

#[tokio::test]
async fn doq_and_h3_source_rate_budget_returns_dns_servfail_without_upstream_io() {
    for protocol in [Protocol::Doq, Protocol::H3] {
        let fixture = Fixture::with_limits(
            protocol,
            false,
            parins::limits::Settings {
                enabled: true,
                burst: 1,
                rate_per_sec: 1,
                ..Default::default()
            },
            2,
        )
        .await;
        let client = fixture.client(protocol);
        let connection = client
            .connect(fixture.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        // Complete the pair before the one-second token refill. A changed ECS
        // option (and HTTP forwarding headers) cannot create a new source.
        timeout(Duration::from_millis(900), async {
            match protocol {
                Protocol::Doq => {
                    for expected in [ResponseCode::NoError, ResponseCode::ServFail] {
                        let mut message = Message::from_vec(&query(0)).unwrap();
                        if expected == ResponseCode::ServFail {
                            parins::ecs::set_subnet(
                                &mut message,
                                Some(hickory_proto::rr::rdata::opt::ClientSubnet::new(
                                    "203.0.113.0".parse().unwrap(),
                                    24,
                                    0,
                                )),
                            );
                        }
                        let wire = message.to_vec().unwrap();
                        let mut framed = (wire.len() as u16).to_be_bytes().to_vec();
                        framed.extend(wire);
                        let (mut send, mut recv) = connection.open_bi().await.unwrap();
                        send.write_all(&framed).await.unwrap();
                        send.finish().unwrap();
                        let answer = recv.read_to_end(65537).await.unwrap();
                        let answer = Message::from_vec(&answer[2..]).unwrap();
                        assert_eq!(answer.id, 0);
                        assert_eq!(answer.response_code, expected);
                    }
                }
                Protocol::H3 => {
                    let (mut driver, mut sender) =
                        h3::client::new(h3_quinn::Connection::new(connection.clone()))
                            .await
                            .unwrap();
                    let drive = tokio::spawn(async move {
                        futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await
                    });
                    for (index, expected) in [ResponseCode::NoError, ResponseCode::ServFail]
                        .into_iter()
                        .enumerate()
                    {
                        let mut message = Message::from_vec(&query(index as u16)).unwrap();
                        if index == 1 {
                            parins::ecs::set_subnet(
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
                        let mut stream = sender.send_request(request).await.unwrap();
                        stream
                            .send_data(Bytes::from(message.to_vec().unwrap()))
                            .await
                            .unwrap();
                        stream.finish().await.unwrap();
                        let response = stream.recv_response().await.unwrap();
                        assert_eq!(response.status(), 200);
                        let mut answer = Vec::new();
                        while let Some(mut data) = stream.recv_data().await.unwrap() {
                            let len = data.remaining();
                            answer.extend_from_slice(&data.copy_to_bytes(len));
                        }
                        let answer = Message::from_vec(&answer).unwrap();
                        assert_eq!(answer.id, index as u16);
                        assert_eq!(answer.response_code, expected);
                    }
                    drive.abort();
                    let _ = drive.await;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.resolver.metrics().snapshot().counters["source_queries_rejected"],
            1
        );
        assert_eq!(fixture.queries.available_permits(), 8);
        fixture.close().await;
        client.close(0u32.into(), b"done");
    }
}

#[tokio::test]
async fn doq_and_h3_source_connection_cap_is_independent_and_reclaimed() {
    for protocol in [Protocol::Doq, Protocol::H3] {
        let fixture = Fixture::with_limits(
            protocol,
            false,
            parins::limits::Settings {
                enabled: true,
                max_connections: 1,
                ..Default::default()
            },
            3,
        )
        .await;
        let client = fixture.client(protocol);
        let connection = client
            .connect(fixture.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        let rejected = timeout(
            Duration::from_secs(1),
            client.connect(fixture.address, "localhost").unwrap(),
        )
        .await
        .unwrap();
        assert!(rejected.is_err());
        assert_eq!(
            fixture.resolver.metrics().snapshot().counters["source_connections_rejected"],
            1
        );
        assert_eq!(fixture.connections.available_permits(), 2);
        connection.close(0u32.into(), b"release source permit");
        timeout(Duration::from_secs(1), async {
            while fixture.connections.available_permits() != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let next = client
            .connect(fixture.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        fixture.close().await;
        timeout(Duration::from_secs(1), next.closed())
            .await
            .unwrap();
        client.close(0u32.into(), b"done");
    }
}
