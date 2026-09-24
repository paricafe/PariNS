use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RecordType,
        rdata::opt::{ClientSubnet, EdnsOption},
    },
};
use parins::{
    config::Config,
    upstreams::{Mode, Pool, Settings},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    time::timeout,
};

fn config(servers: Vec<String>, mode: Mode) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.listen = "127.0.0.1:1053".parse().unwrap();
    config.upstreams = Settings {
        servers,
        mode,
        ..Settings::default()
    };
    config
}

fn query() -> Message {
    let mut query = Message::new(123, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query.metadata.checking_disabled = true;
    query.add_query(Query::query(
        Name::from_ascii("FoRwArD.TeSt.").unwrap(),
        RecordType::AAAA,
    ));
    let mut edns = Edns::new();
    edns.set_dnssec_ok(true).set_max_payload(1400);
    edns.options_mut()
        .insert(EdnsOption::Unknown(10, vec![7; 8]));
    edns.options_mut()
        .insert(EdnsOption::Subnet(ClientSubnet::new(
            "192.0.2.0".parse().unwrap(),
            24,
            0,
        )));
    edns.options_mut()
        .insert(EdnsOption::Unknown(65001, vec![1, 2, 3]));
    query.edns = Some(edns);
    query
}

fn client(config: &Config) -> anyhow::Result<Arc<Pool>> {
    Pool::new(&config.upstreams, config).map(Arc::new)
}

fn verify_forwarded(mut received: Message, expected: &Message) -> Message {
    let id = received.id;
    received.metadata.id = expected.id;
    assert_eq!(
        received, *expected,
        "all client fields and options except transaction ID must survive"
    );
    received.metadata.id = id;
    received.metadata.message_type = MessageType::Response;
    received
}

async fn read(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let size = stream.read_u16().await.unwrap();
    let mut bytes = vec![0; size as usize];
    stream.read_exact(&mut bytes).await.unwrap();
    bytes
}
async fn write(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) {
    stream.write_u16(bytes.len() as u16).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    stream.flush().await.unwrap();
}

async fn udp() -> (SocketAddr, tokio::task::JoinHandle<usize>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut count = 0;
        let mut bytes = vec![0; 65535];
        while let Ok(Ok((length, peer))) =
            timeout(Duration::from_millis(150), socket.recv_from(&mut bytes)).await
        {
            let response = verify_forwarded(Message::from_vec(&bytes[..length]).unwrap(), &query());
            socket
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
            count += 1;
        }
        count
    });
    (address, task)
}

#[tokio::test]
async fn weighted_distribution_and_udp_preserve_edns() {
    let (a, ta) = udp().await;
    let (b, tb) = udp().await;
    let client = client(&config(
        vec![format!("udp://{a} weight=3"), format!("{b} weight=1")],
        Mode::Weighted,
    ))
    .unwrap();
    for _ in 0..40 {
        assert_eq!(client.exchange(&query()).await.unwrap().message.id, 123);
    }
    assert_eq!(ta.await.unwrap(), 30);
    assert_eq!(tb.await.unwrap(), 10);
}

#[tokio::test]
async fn tcp_preserves_edns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = client(&config(
        vec![format!("tcp://{}", listener.local_addr().unwrap())],
        Mode::Weighted,
    ))
    .unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let response = verify_forwarded(
            Message::from_vec(&read(&mut stream).await).unwrap(),
            &query(),
        );
        write(&mut stream, &response.to_vec().unwrap()).await;
    });
    assert_eq!(client.exchange(&query()).await.unwrap().message.id, 123);
    task.await.unwrap();
}

#[tokio::test]
async fn parallel_servfail_cannot_win_and_loser_is_cancelled() {
    let mut sockets = Vec::new();
    for _ in 0..3 {
        sockets.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }
    let servers = sockets
        .iter()
        .map(|s| format!("udp://{}", s.local_addr().unwrap()))
        .collect();
    let winner = sockets[1].local_addr().unwrap();
    let client = client(&config(servers, Mode::Parallel)).unwrap();
    let task = tokio::spawn(async move { client.exchange(&query()).await.unwrap() });
    let mut loser = None;
    for (i, socket) in sockets.iter().enumerate() {
        let mut bytes = [0; 65535];
        let (n, peer) = timeout(Duration::from_secs(2), socket.recv_from(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        if i == 2 {
            loser = Some(peer);
            continue;
        }
        let mut response = verify_forwarded(Message::from_vec(&bytes[..n]).unwrap(), &query());
        response.metadata.response_code = if i == 0 {
            ResponseCode::ServFail
        } else {
            ResponseCode::NoError
        };
        socket
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    }
    let response = task.await.unwrap();
    assert_eq!(response.upstream, format!("udp://{winner}"));
    assert_eq!(response.message.response_code, ResponseCode::NoError);
    let _released = UdpSocket::bind(loser.unwrap()).await.unwrap();
}

fn identity(
    alpn: &[u8],
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Arc<rustls::ServerConfig>,
) {
    let generated =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into(), "resolver.test".into()])
            .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, generated.cert.pem()).unwrap();
    let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![generated.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
    )
    .unwrap();
    server.alpn_protocols = vec![alpn.to_vec()];
    (directory, ca, Arc::new(server))
}

#[tokio::test]
async fn dot_authenticated_and_edns_preserved() {
    let (_dir, ca, tls) = identity(b"dot");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        vec![format!("tls://{}", listener.local_addr().unwrap())],
        Mode::Weighted,
    );
    config.upstreams.ca_file = Some(ca);
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = tokio_rustls::TlsAcceptor::from(tls)
            .accept(stream)
            .await
            .unwrap();
        let response = verify_forwarded(
            Message::from_vec(&read(&mut stream).await).unwrap(),
            &query(),
        );
        write(&mut stream, &response.to_vec().unwrap()).await;
    });
    assert_eq!(
        client(&config)
            .unwrap()
            .exchange(&query())
            .await
            .unwrap()
            .message
            .id,
        123
    );
    task.await.unwrap();
}

#[tokio::test]
async fn doh_authenticated_and_edns_preserved() {
    let (_dir, ca, tls) = identity(b"h2");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        vec![format!(
            "https://{}/dns-query",
            listener.local_addr().unwrap()
        )],
        Mode::Weighted,
    );
    config.upstreams.ca_file = Some(ca);
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let stream = tokio_rustls::TlsAcceptor::from(tls)
            .accept(stream)
            .await
            .unwrap();
        let mut connection = h2::server::handshake(stream).await.unwrap();
        let (request, mut respond) = connection.accept().await.unwrap().unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(request.uri().path(), "/dns-query");
        let handler = async move {
            let mut body = request.into_body();
            let mut bytes = Vec::new();
            while let Some(data) = body.data().await {
                let data = data.unwrap();
                bytes.extend_from_slice(&data);
                body.flow_control().release_capacity(data.len()).unwrap();
            }
            let response = verify_forwarded(Message::from_vec(&bytes).unwrap(), &query());
            let headers = http::Response::builder()
                .header("content-type", "application/dns-message")
                .body(())
                .unwrap();
            respond
                .send_response(headers, false)
                .unwrap()
                .send_data(bytes::Bytes::from(response.to_vec().unwrap()), true)
                .unwrap();
        };
        tokio::pin!(handler);
        tokio::select! { _ = &mut handler => {}, _ = connection.accept() => panic!("closed early") }
        while connection.accept().await.is_some() {}
    });
    assert_eq!(
        client(&config)
            .unwrap()
            .exchange(&query())
            .await
            .unwrap()
            .message
            .id,
        123
    );
    task.await.unwrap();
}

#[tokio::test]
async fn doq_authenticated_preserves_edns_except_prohibited_keepalive() {
    let (_dir, ca, tls) = identity(b"doq");
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from((*tls).clone()).unwrap();
    let endpoint = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let mut config = config(
        vec![format!("quic://{}", endpoint.local_addr().unwrap())],
        Mode::Weighted,
    );
    config.upstreams.ca_file = Some(ca);
    let task = tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        for _ in 0..2 {
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let frame = recv.read_to_end(65537).await.unwrap();
            let received = Message::from_vec(&frame[2..]).unwrap();
            assert_eq!(received.id, 0);
            let response = verify_forwarded(received, &query());
            write(&mut send, &response.to_vec().unwrap()).await;
            send.finish().unwrap();
        }
        connection.closed().await;
    });
    let mut q = query();
    q.edns
        .as_mut()
        .unwrap()
        .options_mut()
        .insert(EdnsOption::Unknown(11, Vec::new()));
    let client = client(&config).unwrap();
    let (first, second) = tokio::join!(client.exchange(&q), client.exchange(&q));
    assert_eq!(first.unwrap().message.id, 123);
    assert_eq!(second.unwrap().message.id, 123);
    drop(client);
    task.await.unwrap();
}

#[tokio::test]
async fn bootstrap_resolved_self_loop_is_rejected_before_dns_send() {
    use hickory_proto::rr::{RData, Record, rdata::A};
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        vec![format!(
            "udp://resolver.test:{}",
            listener.local_addr().unwrap().port()
        )],
        Mode::Weighted,
    );
    config.listen = listener.local_addr().unwrap();
    config.upstreams.bootstrap = vec![bootstrap.local_addr().unwrap()];
    let task = tokio::spawn(async move {
        let mut bytes = [0; 65535];
        let (n, peer) = bootstrap.recv_from(&mut bytes).await.unwrap();
        let mut response = Message::from_vec(&bytes[..n]).unwrap();
        response.metadata.message_type = MessageType::Response;
        response.add_answer(Record::from_rdata(
            response.queries[0].name().clone(),
            60,
            RData::A(A::new(127, 0, 0, 1)),
        ));
        bootstrap
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    assert!(
        client(&config)
            .unwrap()
            .exchange(&query())
            .await
            .unwrap_err()
            .to_string()
            .contains("PariNS listener")
    );
    task.await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), listener.recv_from(&mut [0; 512]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn encrypted_upstreams_reject_untrusted_certificate() {
    use parins::upstreams::diagnostics::{Operation, Reason, Stage};
    for protocol in ["tls", "https"] {
        let (_dir, _ca, tls) = identity(if protocol == "tls" { b"dot" } else { b"h2" });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(
            vec![format!("{protocol}://{}", listener.local_addr().unwrap())],
            Mode::Weighted,
        );
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(
                tokio_rustls::TlsAcceptor::from(tls)
                    .accept(stream)
                    .await
                    .is_err()
            );
        });
        let operation = Operation::new(true, None);
        assert!(
            client(&config)
                .unwrap()
                .exchange_observed(
                    &query(),
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    &operation
                )
                .await
                .is_err()
        );
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts.len(), 1);
        assert_eq!(trace.attempts[0].stage, Stage::TlsHandshake);
        assert_eq!(trace.attempts[0].reason, Some(Reason::Tls));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_bootstrap_resolves_hostname_and_caches_within_ttl() {
    use hickory_proto::rr::{RData, Record, rdata::A};
    let (address, upstream) = udp().await;
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bootstrap_address = bootstrap.local_addr().unwrap();
    let mut config = config(
        vec![format!("udp://resolver.test:{}", address.port())],
        Mode::Weighted,
    );
    config.upstreams.bootstrap = vec![bootstrap_address];
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let mut bytes = [0; 65535];
            let (n, peer) = bootstrap.recv_from(&mut bytes).await.unwrap();
            let mut message = Message::from_vec(&bytes[..n]).unwrap();
            assert!(
                message.edns.is_none(),
                "bootstrap must not leak client ECS/options"
            );
            assert_eq!(message.queries[0].name().to_ascii(), "resolver.test.");
            message.metadata.message_type = MessageType::Response;
            if message.queries[0].query_type() == RecordType::A {
                message.add_answer(Record::from_rdata(
                    message.queries[0].name().clone(),
                    60,
                    RData::A(A::new(127, 0, 0, 1)),
                ));
            }
            bootstrap
                .send_to(&message.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
        assert!(
            timeout(
                Duration::from_millis(100),
                bootstrap.recv_from(&mut [0; 512])
            )
            .await
            .is_err()
        );
    });
    let client = client(&config).unwrap();
    for _ in 0..2 {
        client.exchange(&query()).await.unwrap();
    }
    task.await.unwrap();
    assert_eq!(upstream.await.unwrap(), 2);
}

#[test]
fn rejects_direct_bootstrap_and_mapped_self_loops() {
    for server in ["udp://127.0.0.1:1053", "tcp://[::ffff:127.0.0.1]:1053"] {
        let config = config(vec![server.into()], Mode::Weighted);
        assert!(client(&config).is_err());
    }
    let mut config = config(vec!["udp://resolver.test".into()], Mode::Weighted);
    config.listen = "0.0.0.0:1053".parse().unwrap();
    config.upstreams.bootstrap = vec!["127.0.0.1:1053".parse().unwrap()];
    assert!(client(&config).is_err());
}

#[tokio::test]
async fn parallel_extra_budget_is_released_when_caller_cancels() {
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        vec![
            format!("udp://{}", a.local_addr().unwrap()),
            format!("udp://{}", b.local_addr().unwrap()),
        ],
        Mode::Parallel,
    );
    config.upstreams.max_extra_inflight = 1;
    let client = client(&config).unwrap();
    for _ in 0..2 {
        let clone = client.clone();
        let task = tokio::spawn(async move { clone.exchange(&query()).await });
        let (_, pa) = timeout(Duration::from_secs(2), a.recv_from(&mut [0; 65535]))
            .await
            .unwrap()
            .unwrap();
        let (_, pb) = timeout(Duration::from_secs(2), b.recv_from(&mut [0; 65535]))
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let _sa = UdpSocket::bind(pa).await.unwrap();
        let _sb = UdpSocket::bind(pb).await.unwrap();
    }
}

#[tokio::test]
async fn parallel_refused_formerr_notimp_wait_for_valid_answer() {
    for code in [
        ResponseCode::Refused,
        ResponseCode::FormErr,
        ResponseCode::NotImp,
    ] {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = config(
            vec![
                format!("udp://{}", a.local_addr().unwrap()),
                format!("udp://{}", b.local_addr().unwrap()),
            ],
            Mode::Parallel,
        );
        let client = client(&config).unwrap();
        let task = tokio::spawn(async move { client.exchange(&query()).await.unwrap() });
        let mut bytes = [0; 65535];
        let (n, peer) = a.recv_from(&mut bytes).await.unwrap();
        let mut response = verify_forwarded(Message::from_vec(&bytes[..n]).unwrap(), &query());
        response.metadata.response_code = code;
        a.send_to(&response.to_vec().unwrap(), peer).await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(!task.is_finished(), "{code:?} must not win");
        let (n, peer) = b.recv_from(&mut bytes).await.unwrap();
        let response = verify_forwarded(Message::from_vec(&bytes[..n]).unwrap(), &query());
        b.send_to(&response.to_vec().unwrap(), peer).await.unwrap();
        assert_eq!(
            task.await.unwrap().message.response_code,
            ResponseCode::NoError
        );
    }
}

#[tokio::test]
async fn doh_rejects_redirect_wrong_content_type_and_oversize_body() {
    for case in 0..3 {
        let (_dir, ca, tls) = identity(b"h2");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config(
            vec![format!("https://{}", listener.local_addr().unwrap())],
            Mode::Weighted,
        );
        config.upstreams.ca_file = Some(ca);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let stream = tokio_rustls::TlsAcceptor::from(tls)
                .accept(stream)
                .await
                .unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
            let headers = http::Response::builder()
                .status(if case == 0 { 302 } else { 200 })
                .header(
                    "content-type",
                    if case == 1 {
                        "text/plain"
                    } else {
                        "application/dns-message"
                    },
                )
                .body(())
                .unwrap();
            let mut stream = respond.send_response(headers, false).unwrap();
            let bytes = if case == 2 {
                vec![0; 65536]
            } else {
                verify_forwarded(query(), &query()).to_vec().unwrap()
            };
            stream.send_data(bytes::Bytes::from(bytes), true).unwrap();
            while connection.accept().await.is_some() {}
        });
        let result = timeout(
            Duration::from_secs(2),
            client(&config).unwrap().exchange(&query()),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "case {case}");
        task.await.unwrap();
    }
}

fn h3_endpoint(tls: &rustls::ServerConfig) -> quinn::Endpoint {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls.clone()).unwrap();
    quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap()
}

async fn dual_endpoint(tls: &rustls::ServerConfig) -> (quinn::Endpoint, TcpListener) {
    for _ in 0..32 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls.clone()).unwrap();
        match quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            listener.local_addr().unwrap(),
        ) {
            Ok(endpoint) => return (endpoint, listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("bind QUIC: {error}"),
        }
    }
    panic!("no available dual TCP/UDP test port")
}

async fn serve_h3(endpoint: quinn::Endpoint, case: usize) -> usize {
    use bytes::{Buf, Bytes};
    let connection = endpoint.accept().await.unwrap().await.unwrap();
    let mut server = h3::server::builder()
        .build::<_, Bytes>(h3_quinn::Connection::new(connection.clone()))
        .await
        .unwrap();
    let mut handlers = tokio::task::JoinSet::new();
    let mut count = 0;
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            result = handlers.join_next(), if !handlers.is_empty() => { result.unwrap().unwrap(); },
            request = server.accept() => {
                let Ok(Some(request)) = request else { break };
                count += 1;
                handlers.spawn(async move {
                    let (request, mut stream) = request.resolve_request().await.unwrap();
                    assert_eq!(request.method(), "POST");
                    assert_eq!(request.uri().path(), "/dns-query");
                    let mut body = Vec::new();
                    while let Some(mut bytes) = stream.recv_data().await.unwrap() {
                        body.extend_from_slice(&bytes.copy_to_bytes(bytes.remaining()));
                    }
                    let received = Message::from_vec(&body).unwrap();
                    assert_eq!(received.id, 0);
                    let mut response = verify_forwarded(received, &query());
                    if case == 4 { response.metadata.truncation = true; }
                    if case == 5 { response.metadata.id = 99; }
                    if case == 6 { response.queries.clear(); }
                    if case == 7 {
                        stream.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
                        return;
                    }
                    if case == 8 { tokio::time::sleep(Duration::from_millis(350)).await; }
                    if case == 10 { response.metadata.response_code = ResponseCode::ServFail; }
                    let mut headers = http::Response::builder()
                        .status(if case == 1 { 302 } else { 200 })
                        .header("content-type", if case == 2 { "text/plain" } else { "application/dns-message" });
                    if case == 9 { headers = headers.header("x-large", "a".repeat(17 * 1024)); }
                    let headers = headers.body(()).unwrap();
                    if stream.send_response(headers).await.is_err() { return; }
                    let bytes = if case == 3 { vec![0; 65536] } else { response.to_vec().unwrap() };
                    if stream.send_data(Bytes::from(bytes)).await.is_ok() {
                        let _ = stream.finish().await;
                    }
                });
            }
        }
    }
    while let Some(result) = handlers.join_next().await {
        result.unwrap();
    }
    count
}

async fn serve_h2(listener: TcpListener, tls: Arc<rustls::ServerConfig>, count: usize) {
    for _ in 0..count {
        let (socket, _) = listener.accept().await.unwrap();
        let stream = tokio_rustls::TlsAcceptor::from(tls.clone())
            .accept(socket)
            .await
            .unwrap();
        let mut connection = h2::server::handshake(stream).await.unwrap();
        let (request, mut respond) = connection.accept().await.unwrap().unwrap();
        let handler = async move {
            let mut bytes = Vec::new();
            let mut body = request.into_body();
            while let Some(data) = body.data().await {
                let data = data.unwrap();
                bytes.extend_from_slice(&data);
                body.flow_control().release_capacity(data.len()).unwrap();
            }
            let response = verify_forwarded(Message::from_vec(&bytes).unwrap(), &query());
            respond
                .send_response(
                    http::Response::builder()
                        .header("content-type", "application/dns-message")
                        .body(())
                        .unwrap(),
                    false,
                )
                .unwrap()
                .send_data(bytes::Bytes::from(response.to_vec().unwrap()), true)
                .unwrap();
        };
        tokio::pin!(handler);
        tokio::select! { _ = &mut handler => {}, _ = connection.accept() => panic!("closed early") }
        while connection.accept().await.is_some() {}
    }
}

#[tokio::test]
async fn h3_preferred_reuses_connection_concurrently_preserves_edns_and_closes_on_drop() {
    let (_dir, ca, tls) = identity(b"h3");
    let endpoint = h3_endpoint(&tls);
    let mut config = config(
        vec![format!("https://{}", endpoint.local_addr().unwrap())],
        Mode::Weighted,
    );
    let settings = &mut config.upstreams;
    settings.ca_file = Some(ca);
    settings.prefer_h3 = true;
    let server = tokio::spawn(serve_h3(endpoint, 0));
    let client = client(&config).unwrap();
    let q = query();
    let (a, b, c) = tokio::join!(
        client.exchange(&q),
        client.exchange(&q),
        client.exchange(&q)
    );
    for response in [a, b, c] {
        assert_eq!(response.unwrap().message.id, 123);
    }
    drop(client);
    assert_eq!(
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn h3_unavailable_falls_back_to_authenticated_h2_and_cools_down() {
    use parins::upstreams::diagnostics::{ActualProtocol, Operation, Outcome, Reason, Stage};
    let (_dir, ca, tls) = identity(b"h2");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // A bound silent UDP port simulates a firewall dropping QUIC packets.
    let silent = UdpSocket::bind(address).await.unwrap();
    let peers = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let received = peers.clone();
    let blackhole = tokio::spawn(async move {
        loop {
            let (_, peer) = silent.recv_from(&mut [0; 65535]).await.unwrap();
            received.lock().unwrap().insert(peer);
        }
    });
    let mut config = config(vec![format!("https://{address}")], Mode::Weighted);
    config.query_timeout_ms = 80;
    let settings = &mut config.upstreams;
    settings.ca_file = Some(ca);
    settings.prefer_h3 = true;
    let server = tokio::spawn(serve_h2(listener, tls, 2));
    let client = client(&config).unwrap();
    let start = tokio::time::Instant::now();
    let operation = Operation::new(true, None);
    assert_eq!(
        timeout(
            Duration::from_secs(2),
            client.exchange_observed(&query(), start + Duration::from_millis(80), &operation)
        )
        .await
        .unwrap()
        .unwrap()
        .message
        .id,
        123
    );
    assert!(start.elapsed() >= Duration::from_millis(40));
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 2);
    assert_eq!(trace.attempts[0].protocol, Some(ActualProtocol::Doh3));
    assert_eq!(trace.attempts[0].stage, Stage::QuicHandshake);
    assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline));
    assert_eq!(trace.attempts[1].protocol, Some(ActualProtocol::Doh2));
    assert_eq!(trace.attempts[1].outcome, Outcome::Succeeded);
    let operation = Operation::new(true, None);
    assert_eq!(
        client
            .exchange_observed(
                &query(),
                tokio::time::Instant::now() + Duration::from_millis(80),
                &operation
            )
            .await
            .unwrap()
            .message
            .id,
        123
    );
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts[0].outcome, Outcome::Skipped);
    assert_eq!(trace.attempts[0].reason, None);
    assert_eq!(trace.attempts[1].outcome, Outcome::Succeeded);
    assert_eq!(
        peers.lock().unwrap().len(),
        1,
        "cooldown avoids another QUIC handshake"
    );
    blackhole.abort();
    server.await.unwrap();
}

#[tokio::test]
async fn dot_slot_wait_and_h3_connecting_wait_consume_original_deadline() {
    use parins::upstreams::diagnostics::{Operation, Reason, Stage};
    let (_dir, ca, tls) = identity(b"dot");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        vec![format!("tls://{}", listener.local_addr().unwrap())],
        Mode::Weighted,
    );
    config.upstreams.ca_file = Some(ca);
    config.upstreams.dot_pool.enabled = true;
    config.upstreams.dot_pool.max_connections = 1;
    let client = client(&config).unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = tokio_rustls::TlsAcceptor::from(tls)
            .accept(socket)
            .await
            .unwrap();
        read(&mut stream).await;
        ready_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.exchange(&query()).await });
    ready_rx.await.unwrap();
    let operation = Operation::new(true, None);
    let start = tokio::time::Instant::now();
    assert!(
        client
            .exchange_observed(&query(), start + Duration::from_millis(30), &operation)
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(150));
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 1);
    assert_eq!(trace.attempts[0].stage, Stage::Wait);
    assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline));
    first.abort();
    let _ = first.await;
    server.abort();
    let _ = server.await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent = UdpSocket::bind(listener.local_addr().unwrap())
        .await
        .unwrap();
    config.upstreams.servers = vec![format!("https://{}", listener.local_addr().unwrap())];
    config.upstreams.prefer_h3 = true;
    let client = Pool::new(&config.upstreams, &config).map(Arc::new).unwrap();
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.exchange(&query()).await });
    silent.recv_from(&mut [0; 65535]).await.unwrap();
    let operation = Operation::new(true, None);
    let start = tokio::time::Instant::now();
    assert!(
        client
            .exchange_observed(&query(), start + Duration::from_millis(60), &operation)
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(180));
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 2);
    assert_eq!(trace.attempts[0].stage, Stage::Wait);
    assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline));
    assert_eq!(trace.attempts[1].stage, Stage::TlsHandshake);
    assert_eq!(trace.attempts[1].reason, Some(Reason::Deadline));
    first.abort();
    let _ = first.await;
}

#[tokio::test]
async fn h3_preference_off_uses_h2_without_quic_packets() {
    let (_dir, ca, tls) = identity(b"h2");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let udp = UdpSocket::bind(address).await.unwrap();
    let mut config = config(vec![format!("https://{address}")], Mode::Weighted);
    config.upstreams.ca_file = Some(ca);
    let server = tokio::spawn(serve_h2(listener, tls, 1));
    client(&config).unwrap().exchange(&query()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(20), udp.recv_from(&mut [0; 1500]))
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn h3_untrusted_certificate_and_h2_fallback_both_fail_closed() {
    use parins::upstreams::diagnostics::{Operation, Reason, Stage};
    let (_dir, _ca, tls) = identity(b"h3");
    let (endpoint, listener) = dual_endpoint(&tls).await;
    let address = endpoint.local_addr().unwrap();
    let mut h2_tls = (*tls).clone();
    h2_tls.alpn_protocols = vec![b"h2".to_vec()];
    let quic_server = tokio::spawn(async move {
        assert!(endpoint.accept().await.unwrap().await.is_err());
    });
    let tcp_server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        assert!(
            tokio_rustls::TlsAcceptor::from(Arc::new(h2_tls))
                .accept(socket)
                .await
                .is_err()
        );
    });
    let mut config = config(vec![format!("https://{address}")], Mode::Weighted);
    config.upstreams.prefer_h3 = true;
    let operation = Operation::new(true, None);
    assert!(
        timeout(
            Duration::from_secs(2),
            client(&config).unwrap().exchange_observed(
                &query(),
                tokio::time::Instant::now() + Duration::from_secs(1),
                &operation
            )
        )
        .await
        .unwrap()
        .is_err()
    );
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 2);
    assert_eq!(trace.attempts[0].stage, Stage::QuicHandshake);
    assert_eq!(trace.attempts[0].reason, Some(Reason::Tls));
    assert_eq!(trace.attempts[1].stage, Stage::TlsHandshake);
    assert_eq!(trace.attempts[1].reason, Some(Reason::Tls));
    timeout(Duration::from_secs(2), quic_server)
        .await
        .unwrap()
        .unwrap();
    tcp_server.await.unwrap();
}

#[tokio::test]
async fn h3_invalid_response_falls_back_to_verified_h2() {
    use parins::upstreams::diagnostics::{Operation, Outcome, Reason};
    for case in 1..=9 {
        let (_dir, ca, tls) = identity(b"h3");
        let (endpoint, listener) = dual_endpoint(&tls).await;
        let address = endpoint.local_addr().unwrap();
        let mut h2_tls = (*tls).clone();
        h2_tls.alpn_protocols = vec![b"h2".to_vec()];
        let quic_server = tokio::spawn(serve_h3(endpoint, case));
        let tcp_server = tokio::spawn(serve_h2(listener, Arc::new(h2_tls), 1));
        let mut config = config(vec![format!("https://{address}")], Mode::Weighted);
        let settings = &mut config.upstreams;
        settings.ca_file = Some(ca);
        settings.prefer_h3 = true;
        let client = client(&config).unwrap();
        let operation = Operation::new(true, None);
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                client.exchange_observed(
                    &query(),
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    &operation
                )
            )
            .await
            .unwrap()
            .unwrap()
            .message
            .id,
            123,
            "case {case}"
        );
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts.len(), 2);
        assert_eq!(trace.attempts[1].outcome, Outcome::Succeeded);
        match case {
            1 => {
                assert_eq!(trace.attempts[0].reason, Some(Reason::HttpStatus));
                assert_eq!(trace.attempts[0].code, Some(302));
            }
            2..=6 => assert_eq!(
                trace.attempts[0].reason,
                Some(Reason::ProtocolInvalid),
                "case {case}"
            ),
            8 => assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline)),
            _ => {}
        }
        drop(client);
        assert_eq!(
            timeout(Duration::from_secs(2), quic_server)
                .await
                .unwrap()
                .unwrap(),
            1
        );
        tcp_server.await.unwrap();
    }
}

#[tokio::test]
async fn h3_valid_dns_failure_is_not_a_transport_failure() {
    let (_dir, ca, tls) = identity(b"h3");
    let endpoint = h3_endpoint(&tls);
    let mut config = config(
        vec![format!("https://{}", endpoint.local_addr().unwrap())],
        Mode::Weighted,
    );
    let settings = &mut config.upstreams;
    settings.ca_file = Some(ca);
    settings.prefer_h3 = true;
    let server = tokio::spawn(serve_h3(endpoint, 10));
    let client = client(&config).unwrap();
    for _ in 0..2 {
        assert_eq!(
            client
                .exchange(&query())
                .await
                .unwrap()
                .message
                .response_code,
            ResponseCode::ServFail
        );
    }
    drop(client);
    assert_eq!(
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn h3_caller_cancellation_preserves_other_streams_and_reuses_connection() {
    use bytes::{Buf, Bytes};
    let (_dir, ca, tls) = identity(b"h3");
    let endpoint = h3_endpoint(&tls);
    let mut config = config(
        vec![format!("https://{}", endpoint.local_addr().unwrap())],
        Mode::Weighted,
    );
    let settings = &mut config.upstreams;
    settings.ca_file = Some(ca);
    settings.prefer_h3 = true;
    let (started, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let mut server = h3::server::builder()
            .build::<_, Bytes>(h3_quinn::Connection::new(connection.clone()))
            .await
            .unwrap();
        let (_, mut stalled) = server
            .accept()
            .await
            .unwrap()
            .unwrap()
            .resolve_request()
            .await
            .unwrap();
        while stalled.recv_data().await.unwrap().is_some() {}
        started.send(()).unwrap();
        let (_, mut stream) = server
            .accept()
            .await
            .unwrap()
            .unwrap()
            .resolve_request()
            .await
            .unwrap();
        let handler = async move {
            let mut body = Vec::new();
            while let Some(mut bytes) = stream.recv_data().await.unwrap() {
                body.extend_from_slice(&bytes.copy_to_bytes(bytes.remaining()));
            }
            let response = verify_forwarded(Message::from_vec(&body).unwrap(), &query());
            stream
                .send_response(
                    http::Response::builder()
                        .header("content-type", "application/dns-message")
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            stream
                .send_data(Bytes::from(response.to_vec().unwrap()))
                .await
                .unwrap();
            stream.finish().await.unwrap();
        };
        tokio::pin!(handler);
        tokio::select! { _ = &mut handler => {}, _ = server.accept() => panic!("unexpected connection close") }
        // STOP_SENDING propagated for the cancelled request, without closing the connection.
        assert!(
            stalled
                .send_response(http::Response::builder().body(()).unwrap())
                .await
                .is_err()
        );
        connection.closed().await;
    });
    let client = client(&config).unwrap();
    let clone = client.clone();
    let stalled = tokio::spawn(async move { clone.exchange(&query()).await });
    timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    stalled.abort();
    assert!(stalled.await.unwrap_err().is_cancelled());
    assert_eq!(client.exchange(&query()).await.unwrap().message.id, 123);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn unified_doh_listener_self_loop_is_rejected_for_either_upstream_preference() {
    let mut config = config(vec!["https://127.0.0.1:4443".into()], Mode::Weighted);
    config.doh = Some(parins::config::DohConfig {
        http3: true,
        listen: "0.0.0.0:4443".parse().unwrap(),
        files: parins::tls::TlsFiles {
            cert_file: "unused.pem".into(),
            key_file: "unused.key".into(),
        },
    });
    let settings = &mut config.upstreams;
    assert!(!settings.prefer_h3);
    assert!(config.upstreams.validate_listeners(&config).is_err());
    config.upstreams.prefer_h3 = true;
    assert!(config.upstreams.validate_listeners(&config).is_err());
}

fn age_answer(mut message: Message, negative: bool) -> Message {
    use hickory_proto::rr::{
        RData, Record,
        rdata::{A, SOA},
    };
    message.metadata.message_type = MessageType::Response;
    let name = message.queries[0].name().clone();
    if negative {
        message.metadata.response_code = ResponseCode::NXDomain;
        message.add_authority(Record::from_rdata(
            name,
            601,
            RData::SOA(SOA::new(
                Name::from_ascii("ns.test.").unwrap(),
                Name::from_ascii("admin.test.").unwrap(),
                42,
                900,
                600,
                86400,
                601,
            )),
        ));
    } else {
        message.add_answer(Record::from_rdata(
            name.clone(),
            601,
            RData::A(A::new(192, 0, 2, 1)),
        ));
        message.add_additional(Record::from_rdata(
            name,
            602,
            RData::A(A::new(192, 0, 2, 2)),
        ));
    }
    message
}

async fn age_upstream(
    h3: bool,
    age: Option<&'static str>,
    negative: bool,
) -> (
    tempfile::TempDir,
    Config,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use bytes::{Buf, Bytes};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (directory, ca, tls) = identity(if h3 { b"h3" } else { b"h2" });
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let headers = move || {
        let mut headers =
            http::Response::builder().header("content-type", "application/dns-message");
        if let Some(age) = age {
            headers = headers.header("age", age);
        }
        headers.body(()).unwrap()
    };
    let (address, task) = if h3 {
        let endpoint = h3_endpoint(&tls);
        let address = endpoint.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let connection = endpoint.accept().await.unwrap().await.unwrap();
            let mut server = h3::server::builder()
                .build::<_, Bytes>(h3_quinn::Connection::new(connection.clone()))
                .await
                .unwrap();
            while let Ok(Some(request)) = server.accept().await {
                let (_, mut stream) = request.resolve_request().await.unwrap();
                let handler = async {
                    let mut bytes = Vec::new();
                    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
                        bytes.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let answer = age_answer(Message::from_vec(&bytes).unwrap(), negative);
                    stream.send_response(headers()).await.unwrap();
                    stream
                        .send_data(Bytes::from(answer.to_vec().unwrap()))
                        .await
                        .unwrap();
                    stream.finish().await.unwrap();
                };
                tokio::pin!(handler);
                tokio::select! { _ = &mut handler => {}, _ = server.accept() => panic!("closed early") }
            }
        });
        (address, task)
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let stream = tokio_rustls::TlsAcceptor::from(tls.clone())
                    .accept(socket)
                    .await
                    .unwrap();
                let mut connection = h2::server::handshake(stream).await.unwrap();
                let (request, mut respond) = connection.accept().await.unwrap().unwrap();
                let handler = async {
                    let mut bytes = Vec::new();
                    let mut body = request.into_body();
                    while let Some(chunk) = body.data().await {
                        let chunk = chunk.unwrap();
                        bytes.extend_from_slice(&chunk);
                        body.flow_control().release_capacity(chunk.len()).unwrap();
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let answer = age_answer(Message::from_vec(&bytes).unwrap(), negative);
                    respond
                        .send_response(headers(), false)
                        .unwrap()
                        .send_data(Bytes::from(answer.to_vec().unwrap()), true)
                        .unwrap();
                };
                tokio::pin!(handler);
                tokio::select! { _ = &mut handler => {}, _ = connection.accept() => panic!("closed early") }
                while connection.accept().await.is_some() {}
            }
        });
        (address, task)
    };
    let mut config = config(vec![format!("https://{address}")], Mode::Weighted);
    config.upstreams.ca_file = Some(ca);
    config.upstreams.prefer_h3 = h3;
    config.cache.negative_ttl_cap_secs = 1000;
    (directory, config, calls, task)
}

#[tokio::test]
async fn doh_age_normalizes_downstream_ttls_and_cache_lifetime_for_h2_and_h3() {
    use parins::{cache::Cache, ecs::Scope, resolver::Resolver};
    use std::{sync::atomic::Ordering, time::Instant};
    for h3 in [false, true] {
        for negative in [false, true] {
            for (age, remaining) in [
                (None, 601),
                (Some("0"), 601),
                (Some("600"), 1),
                (Some("999"), 0),
                (Some("999999999999999999999999999"), 0),
                (Some("invalid"), 601),
                (Some("-1"), 601),
                (Some("+1"), 601),
                (Some("1.5"), 601),
                (Some("600, 999"), 1),
            ] {
                let (_dir, config, calls, server) = age_upstream(h3, age, negative).await;
                let resolver = Resolver::from_config(&config);
                let mut query = Message::new(123, MessageType::Query, OpCode::Query);
                query.add_query(Query::query(
                    Name::from_ascii("age.test.").unwrap(),
                    RecordType::A,
                ));
                let response = resolver
                    .resolve(&query.to_vec().unwrap(), "127.0.0.1".parse().unwrap())
                    .await
                    .unwrap()
                    .message;
                let records = if negative {
                    &response.authorities
                } else {
                    &response.answers
                };
                assert_eq!(
                    records[0].ttl, remaining,
                    "h3={h3} negative={negative} Age={age:?}"
                );
                if negative {
                    let original = age_answer(query.clone(), true);
                    assert_eq!(
                        response.authorities[0].data, original.authorities[0].data,
                        "SOA RDATA is not a record TTL"
                    );
                } else {
                    assert_eq!(
                        response.additionals[0].ttl,
                        if remaining == 0 { 0 } else { remaining + 1 }
                    );
                }
                let cache = Cache::new(config.cache.clone());
                let now = Instant::now();
                cache.insert(&query, &response, Scope::NoEcs, now);
                assert_eq!(cache.get(&query, None, now).is_some(), remaining != 0);
                assert!(
                    cache
                        .get(&query, None, now + Duration::from_secs(remaining.into()))
                        .is_none()
                );
                if age == Some("600") {
                    resolver
                        .resolve(&query.to_vec().unwrap(), "127.0.0.1".parse().unwrap())
                        .await
                        .unwrap();
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        1,
                        "immediate resolver cache hit"
                    );
                    tokio::time::sleep(Duration::from_millis(1100)).await;
                    resolver
                        .resolve(&query.to_vec().unwrap(), "127.0.0.1".parse().unwrap())
                        .await
                        .unwrap();
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        2,
                        "resolver expires the aged answer after one second"
                    );
                }
                drop(resolver);
                server.abort();
                let _ = server.await;
            }
        }
    }
}

async fn doq_rotation(cancel_old: bool) {
    use hickory_proto::rr::{
        RData, Record,
        rdata::{A, AAAA},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_dir, ca, tls) = identity(b"doq");
    let old_endpoint = h3_endpoint(&tls);
    let port = old_endpoint.local_addr().unwrap().port();
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from((*tls).clone()).unwrap();
    let new_endpoint = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        format!("[::1]:{port}").parse().unwrap(),
    )
    .unwrap();
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(vec![format!("quic://resolver.test:{port}")], Mode::Weighted);
    config.upstreams.ca_file = Some(ca);
    config.upstreams.bootstrap = vec![bootstrap.local_addr().unwrap()];
    let rotated = Arc::new(AtomicBool::new(false));
    let current = rotated.clone();
    let bootstrap_server = tokio::spawn(async move {
        for _ in 0..4 {
            let mut bytes = [0; 65535];
            let (n, peer) = bootstrap.recv_from(&mut bytes).await.unwrap();
            let mut response = Message::from_vec(&bytes[..n]).unwrap();
            response.metadata.message_type = MessageType::Response;
            let query = &response.queries[0];
            let data = match (current.load(Ordering::SeqCst), query.query_type()) {
                (false, RecordType::A) => Some(RData::A(A::new(127, 0, 0, 1))),
                (true, RecordType::AAAA) => Some(RData::AAAA(AAAA("::1".parse().unwrap()))),
                _ => None,
            };
            if let Some(data) = data {
                // Zero is a short bootstrap TTL: the very next query must resolve again.
                response.add_answer(Record::from_rdata(query.name().clone(), 0, data));
            }
            bootstrap
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let (released, acknowledged) = tokio::sync::oneshot::channel();
    let old_server = tokio::spawn(async move {
        let connection = old_endpoint.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = connection.accept_bi().await.unwrap();
        let frame = recv.read_to_end(65537).await.unwrap();
        started.send(()).unwrap();
        tokio::select! {
            result = wait => { result.unwrap(); },
            _ = connection.closed() => panic!("address rotation closed an in-flight DoQ owner"),
        }
        released.send(()).unwrap();
        if !cancel_old {
            let response = verify_forwarded(Message::from_vec(&frame[2..]).unwrap(), &query());
            write(&mut send, &response.to_vec().unwrap()).await;
            send.finish().unwrap();
        }
        connection.closed().await;
    });
    let new_server = tokio::spawn(async move {
        let connection = new_endpoint.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = connection.accept_bi().await.unwrap();
        let frame = recv.read_to_end(65537).await.unwrap();
        let response = verify_forwarded(Message::from_vec(&frame[2..]).unwrap(), &query());
        write(&mut send, &response.to_vec().unwrap()).await;
        send.finish().unwrap();
        connection.closed().await;
    });
    let client = client(&config).unwrap();
    let clone = client.clone();
    let old_request = tokio::spawn(async move { clone.exchange(&query()).await });
    timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    rotated.store(true, Ordering::SeqCst);
    assert_eq!(client.exchange(&query()).await.unwrap().message.id, 123);
    release.send(()).unwrap();
    acknowledged.await.unwrap();
    if cancel_old {
        old_request.abort();
        assert!(old_request.await.unwrap_err().is_cancelled());
    } else {
        assert_eq!(old_request.await.unwrap().unwrap().message.id, 123);
    }
    // The retired owner closes as soon as its last request ends, even while
    // the client (and the new owner) remain alive.
    timeout(Duration::from_secs(2), old_server)
        .await
        .unwrap()
        .unwrap();
    drop(client);
    timeout(Duration::from_secs(2), new_server)
        .await
        .unwrap()
        .unwrap();
    bootstrap_server.await.unwrap();
}

#[tokio::test]
async fn doq_bootstrap_rotation_preserves_old_and_new_requests() {
    timeout(Duration::from_secs(5), doq_rotation(false))
        .await
        .unwrap();
}

#[tokio::test]
async fn doq_rotated_owner_closes_when_last_request_is_cancelled() {
    timeout(Duration::from_secs(5), doq_rotation(true))
        .await
        .unwrap();
}
