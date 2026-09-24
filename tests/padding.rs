use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, opt::EdnsOption},
    },
};
use parins::{config::Config, ecs, ingress::Ingress, protocol, resolver::Resolver, tls::TlsFiles};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Semaphore, watch},
    time::timeout,
};

fn query(id: u16, padded: bool) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("cache.fixture.test.").unwrap(),
        RecordType::A,
    ));
    let mut edns = Edns::new();
    edns.set_max_payload(1232);
    if padded {
        edns.options_mut().insert(EdnsOption::Unknown(
            12,
            vec![id as u8; 31 + usize::from(id)],
        ));
    }
    query.edns = Some(edns);
    query
}

fn ingress(config: &Config, capacity: usize) -> Ingress {
    let (_stop, stop) = watch::channel(false);
    Ingress {
        resolver: Arc::new(Resolver::from_config(config)),
        queries: Arc::new(Semaphore::new(capacity)),
        connections: Arc::new(Semaphore::new(16)),
        source_limits: Arc::new(parins::limits::Limiter::new(&config.source_limits).unwrap()),
        stop,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_secs(1),
        max_streams: 16,
    }
}

#[tokio::test]
async fn concurrent_padding_payloads_share_work_and_restore_each_clients_ecs_and_id() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.ecs.enabled = true;
    config.ecs.ipv4_prefix = 24;
    config.query_timeout_ms = 2000;
    let ingress = ingress(&config, 16);
    let mut a = query(21, true);
    let mut b = query(22, true);
    ecs::set_subnet(&mut a, Some("192.0.2.10/32".parse().unwrap()));
    ecs::set_subnet(&mut b, Some("192.0.2.11/32".parse().unwrap()));
    let a = a.to_vec().unwrap();
    let b = b.to_vec().unwrap();
    let mut first =
        Box::pin(ingress.handle_with_transport(&a, "192.0.2.10".parse().unwrap(), "dot"));
    assert!(futures_util::poll!(&mut first).is_pending());
    let mut buffer = [0; 4096];
    let (length, peer) = tokio::select! {
        result = timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)) => result.unwrap().unwrap(),
        _ = &mut first => panic!("completed before upstream request"),
    };
    let out = protocol::decode(&buffer[..length]).unwrap();
    assert!(
        !protocol::has_padding(&out),
        "plaintext upstream strips padding"
    );
    let mut second =
        Box::pin(ingress.handle_with_transport(&b, "192.0.2.11".parse().unwrap(), "dot"));
    assert!(futures_util::poll!(&mut second).is_pending());
    assert_eq!(
        ingress.resolver.metrics().snapshot().counters["flight_joined"],
        1
    );
    let mut response = protocol::error_response(&out, ResponseCode::NoError);
    response.add_answer(Record::from_rdata(
        out.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 42)),
    ));
    let mut subnet = ecs::subnet(&out).unwrap();
    subnet.set_scope_prefix(24);
    ecs::set_subnet(&mut response, Some(subnet));
    socket
        .send_to(&response.to_vec().unwrap(), peer)
        .await
        .unwrap();
    let (first, second) = timeout(Duration::from_secs(2), async {
        tokio::join!(first, second)
    })
    .await
    .unwrap();
    for (wire, id, ip) in [
        (first.unwrap(), 21, "192.0.2.10"),
        (second.unwrap(), 22, "192.0.2.11"),
    ] {
        assert_eq!(wire.len() % 468, 0);
        let response = protocol::decode(&wire).unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.answers.len(), 1);
        assert!(protocol::has_padding(&response));
        let subnet = ecs::subnet(&response).unwrap();
        assert_eq!(subnet.addr().to_string(), ip);
        assert_eq!(subnet.source_prefix(), 32);
        assert_eq!(subnet.scope_prefix(), 24);
    }
    assert_eq!(
        ingress.resolver.metrics().snapshot().counters["upstream_operations"],
        1
    );
}

#[tokio::test]
async fn admission_rejections_reencode_padding_without_upstream_work() {
    for source_rejection in [false, true] {
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.source_limits.enabled = source_rejection;
        config.source_limits.max_inflight = 1;
        let ingress = ingress(&config, if source_rejection { 16 } else { 0 });
        let peer = "192.0.2.10".parse().unwrap();
        let _held = source_rejection.then(|| ingress.admit_query(peer).unwrap());
        for padded in [true, false] {
            let q = query(31, padded);
            let wire = ingress
                .handle_with_transport(&q.to_vec().unwrap(), peer, "doh")
                .await
                .unwrap();
            let response = protocol::decode(&wire).unwrap();
            assert_eq!(response.id, 31);
            assert_eq!(response.response_code, ResponseCode::ServFail);
            assert!(response.answers.is_empty());
            assert_eq!(protocol::has_padding(&response), padded);
            if padded {
                assert_eq!(wire.len() % 468, 0);
            }
        }
        let counters = ingress.resolver.metrics().snapshot().counters;
        assert_eq!(counters["encrypted_rejected"], 2);
        assert_eq!(counters["upstream_operations"], 0);
    }
}

#[tokio::test]
async fn stale_answer_is_padded_after_failed_upstream_and_keeps_its_answer() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.ecs.enabled = false;
    config.cache.stale.enabled = true;
    let ingress = ingress(&config, 16);
    let question = query(41, true);
    let mut cached = protocol::error_response(&question, ResponseCode::NoError);
    cached.add_answer(Record::from_rdata(
        question.queries[0].name().clone(),
        1,
        RData::A(A::new(192, 0, 2, 42)),
    ));
    ingress.resolver.cache().insert(
        &question,
        &cached,
        ecs::Scope::NoEcs,
        std::time::Instant::now() - Duration::from_secs(2),
    );
    let serve = async {
        let mut buffer = [0; 4096];
        let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let question = protocol::decode(&buffer[..length]).unwrap();
        assert!(!protocol::has_padding(&question));
        socket
            .send_to(
                &protocol::error_response(&question, ResponseCode::ServFail)
                    .to_vec()
                    .unwrap(),
                peer,
            )
            .await
            .unwrap();
    };
    let wire = question.to_vec().unwrap();
    let (answer, ()) = timeout(Duration::from_secs(2), async {
        tokio::join!(
            ingress.handle_with_transport(&wire, "192.0.2.10".parse().unwrap(), "doq"),
            serve
        )
    })
    .await
    .unwrap();
    let wire = answer.unwrap();
    assert_eq!(wire.len() % 468, 0);
    let answer = protocol::decode(&wire).unwrap();
    assert_eq!(answer.id, 41);
    assert_eq!(answer.response_code, ResponseCode::NoError);
    assert_eq!(answer.answers.len(), 1);
    assert_eq!(answer.answers[0].ttl, config.cache.stale.reply_ttl_secs);
    assert!(protocol::has_padding(&answer));
    assert!(!answer.truncation);
    assert_eq!(ingress.resolver.cache().snapshot()["stale_hits"], 1);
}

#[tokio::test]
async fn filtered_cname_miss_and_hit_both_receive_final_per_client_padding() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.ecs.enabled = false;
    config.filter = toml::from_str("enabled=true\nblock_exact=['blocked.test']").unwrap();
    let ingress = ingress(&config, 16);
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let question = protocol::decode(&buffer[..length]).unwrap();
        let mut answer = protocol::error_response(&question, ResponseCode::NoError);
        answer.add_answer(Record::from_rdata(
            question.queries[0].name().clone(),
            60,
            RData::CNAME(hickory_proto::rr::rdata::CNAME(
                Name::from_ascii("blocked.test.").unwrap(),
            )),
        ));
        answer.add_answer(Record::from_rdata(
            Name::from_ascii("blocked.test.").unwrap(),
            60,
            RData::A(A::new(192, 0, 2, 42)),
        ));
        socket
            .send_to(&answer.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    for id in [51, 52] {
        let wire = timeout(
            Duration::from_secs(2),
            ingress.handle_with_transport(
                &query(id, true).to_vec().unwrap(),
                "192.0.2.10".parse().unwrap(),
                "doh3",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(wire.len() % 468, 0);
        let answer = protocol::decode(&wire).unwrap();
        assert_eq!(answer.id, id);
        assert_eq!(answer.response_code, ResponseCode::NoError);
        assert!(
            answer.answers.is_empty(),
            "policy removes blocked CNAME before padding"
        );
        assert!(protocol::has_padding(&answer));
        assert!(!answer.truncation);
    }
    mock.await.unwrap();
    let counters = ingress.resolver.metrics().snapshot().counters;
    assert_eq!(counters["upstream_operations"], 1);
    assert_eq!(counters["cache_hits"], 1);
    assert_eq!(counters["response_blocked"], 2);
}

#[tokio::test]
async fn encrypted_no_ecs_padded_answer_caches_by_actual_source_and_reencodes_per_client() {
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let files = TlsFiles {
        cert_file: dir.path().join("cert.pem"),
        key_file: dir.path().join("key.pem"),
    };
    std::fs::write(&files.cert_file, cert.cert.pem()).unwrap();
    std::fs::write(&files.key_file, cert.signing_key.serialize_pem()).unwrap();
    let acceptor =
        tokio_rustls::TlsAcceptor::from(parins::tls::server_config(&files, &[b"dot"]).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let mock = tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let n = socket.read_u16().await.unwrap();
            let mut bytes = vec![0; usize::from(n)];
            socket.read_exact(&mut bytes).await.unwrap();
            assert_eq!(bytes.len() % 128, 0, "upstream padding block");
            let q = protocol::decode(&bytes).unwrap();
            assert!(protocol::has_padding(&q));
            assert_eq!(ecs::subnet(&q).unwrap().source_prefix(), 24);
            observed.fetch_add(1, Ordering::SeqCst);
            let mut response = protocol::error_response(&q, ResponseCode::NoError);
            response.add_answer(Record::from_rdata(
                q.queries[0].name().clone(),
                60,
                RData::A(A::new(192, 0, 2, 42)),
            ));
            response
                .edns
                .as_mut()
                .unwrap()
                .options_mut()
                .insert(EdnsOption::Unknown(12, vec![7; 12]));
            assert!(ecs::subnet(&response).is_none());
            let bytes = response.to_vec().unwrap();
            socket.write_u16(bytes.len() as u16).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
        }
    });
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.ecs.enabled = true;
    config.ecs.ipv4_prefix = 24;
    config.query_timeout_ms = 2000;
    config.upstreams.servers = vec![format!("tls://{address}")];
    config.upstreams.ca_file = Some(files.cert_file);
    let resolver = Arc::new(Resolver::from_config(&config));
    let (_stop, stop) = watch::channel(false);
    let ingress = Ingress {
        resolver: resolver.clone(),
        queries: Arc::new(Semaphore::new(16)),
        connections: Arc::new(Semaphore::new(16)),
        source_limits: Arc::new(parins::limits::Limiter::new(&config.source_limits).unwrap()),
        stop,
        io_timeout: Duration::from_secs(2),
        shutdown_grace: Duration::from_secs(1),
        max_streams: 16,
    };
    for (id, peer, padded, expected_count) in [
        (1, "192.0.2.1", true, 1),
        (2, "192.0.2.99", true, 1),
        (3, "192.0.3.1", true, 2),
        (4, "192.0.2.19", false, 2),
    ] {
        let bytes = timeout(
            Duration::from_secs(3),
            ingress.handle_with_transport(
                &query(id, padded).to_vec().unwrap(),
                peer.parse().unwrap(),
                "dot",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        if padded {
            assert_eq!(bytes.len() % 468, 0);
        }
        let response = protocol::decode(&bytes).unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.answers.len(), 1);
        assert_eq!(protocol::has_padding(&response), padded);
        assert!(
            ecs::subnet(&response).is_none(),
            "no unsolicited downstream ECS"
        );
        assert_eq!(count.load(Ordering::SeqCst), expected_count);
    }
    mock.await.unwrap();
    let q = query(5, true);
    let reply = resolver
        .resolve(&q.to_vec().unwrap(), "192.0.2.1".parse().unwrap())
        .await
        .unwrap();
    let wire = protocol::encode_udp(&reply.message, reply.udp_limit).unwrap();
    assert!(!protocol::has_padding(&protocol::decode(&wire).unwrap()));
}
