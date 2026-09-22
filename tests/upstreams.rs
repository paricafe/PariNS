use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RecordType,
        rdata::opt::{ClientSubnet, EdnsOption},
    },
};
use parins::{
    config::Config,
    scheduler::Client,
    upstreams::{Mode, Settings},
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
    config.upstreams = Some(Settings {
        servers,
        mode,
        ..Settings::default()
    });
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
    let client = Client::from_config(&config(
        vec![format!("udp://{a} weight=3"), format!("{b} weight=1")],
        Mode::Weighted,
    ))
    .unwrap();
    for _ in 0..40 {
        assert_eq!(client.exchange(&query()).await.unwrap().id, 123);
    }
    assert_eq!(ta.await.unwrap(), 30);
    assert_eq!(tb.await.unwrap(), 10);
}

#[tokio::test]
async fn tcp_preserves_edns() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::from_config(&config(
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
    assert_eq!(client.exchange(&query()).await.unwrap().id, 123);
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
    let client = Client::from_config(&config(servers, Mode::Parallel)).unwrap();
    let task = tokio::spawn(async move { client.exchange_traced(&query()).await.unwrap() });
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
    let generated = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
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
    config.upstreams.as_mut().unwrap().ca_file = Some(ca);
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
        Client::from_config(&config)
            .unwrap()
            .exchange(&query())
            .await
            .unwrap()
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
    config.upstreams.as_mut().unwrap().ca_file = Some(ca);
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
        Client::from_config(&config)
            .unwrap()
            .exchange(&query())
            .await
            .unwrap()
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
    config.upstreams.as_mut().unwrap().ca_file = Some(ca);
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
    let client = Client::from_config(&config).unwrap();
    let (first, second) = tokio::join!(client.exchange(&q), client.exchange(&q));
    assert_eq!(first.unwrap().id, 123);
    assert_eq!(second.unwrap().id, 123);
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
    config.upstreams.as_mut().unwrap().bootstrap = vec![bootstrap.local_addr().unwrap()];
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
        Client::from_config(&config)
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
        assert!(
            Client::from_config(&config)
                .unwrap()
                .exchange(&query())
                .await
                .is_err()
        );
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
    config.upstreams.as_mut().unwrap().bootstrap = vec![bootstrap_address];
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
    let client = Client::from_config(&config).unwrap();
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
        assert!(Client::from_config(&config).is_err());
    }
    let mut config = config(vec!["udp://resolver.test".into()], Mode::Weighted);
    config.listen = "0.0.0.0:1053".parse().unwrap();
    config.upstreams.as_mut().unwrap().bootstrap = vec!["127.0.0.1:1053".parse().unwrap()];
    assert!(Client::from_config(&config).is_err());
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
    config.upstreams.as_mut().unwrap().max_extra_inflight = 1;
    let client = Client::from_config(&config).unwrap();
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
        let client = Client::from_config(&config).unwrap();
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
        assert_eq!(task.await.unwrap().response_code, ResponseCode::NoError);
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
        config.upstreams.as_mut().unwrap().ca_file = Some(ca);
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
            Client::from_config(&config).unwrap().exchange(&query()),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "case {case}");
        task.await.unwrap();
    }
}
