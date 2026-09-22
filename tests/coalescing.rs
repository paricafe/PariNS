use futures_util::poll;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, opt::EdnsOption},
    },
};
use parins::{config::Config, ecs, protocol, resolver::Resolver};
use std::time::Duration;
use tokio::{net::UdpSocket, time::timeout};

fn query(id: u16, peer: &str) -> Vec<u8> {
    let mut q = Message::new(id, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(
        Name::from_ascii(if id == 1 {
            "Example.Test."
        } else {
            "example.test."
        })
        .unwrap(),
        RecordType::A,
    ));
    ecs::set_subnet(&mut q, Some(format!("{peer}/32").parse().unwrap()));
    q.to_vec().unwrap()
}

fn config(socket: &UdpSocket) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstream = socket.local_addr().unwrap();
    config.ecs.enabled = true;
    config.query_timeout_ms = 200;
    config
}

async fn received(socket: &UdpSocket) -> (Message, std::net::SocketAddr) {
    let mut buffer = [0; 4096];
    let (len, peer) = timeout(Duration::from_secs(1), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    (protocol::decode(&buffer[..len]).unwrap(), peer)
}

async fn receive_while<F: std::future::Future + Unpin>(
    socket: &UdpSocket,
    work: &mut F,
) -> (Message, std::net::SocketAddr) {
    tokio::select! {
        result = received(socket) => result,
        _ = work => panic!("query completed before the mock received it"),
    }
}

async fn respond(socket: &UdpSocket, q: &Message, peer: std::net::SocketAddr) {
    let mut reply = protocol::error_response(q, ResponseCode::NoError);
    reply.add_answer(Record::from_rdata(
        q.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    let mut subnet = ecs::subnet(q).unwrap();
    subnet.set_scope_prefix(24);
    ecs::set_subnet(&mut reply, Some(subnet));
    socket
        .send_to(&reply.to_vec().unwrap(), peer)
        .await
        .unwrap();
}

#[tokio::test]
async fn identical_outbound_work_is_shared_and_client_identity_is_restored() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::from_config(&config(&socket));
    let a = query(1, "192.0.2.10");
    let b = query(2, "192.0.2.11");
    let mut first = Box::pin(resolver.resolve(&a, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut first).is_pending());
    let (out, peer) = receive_while(&socket, &mut first).await;
    let mut second = Box::pin(resolver.resolve(&b, "192.0.2.11".parse().unwrap()));
    assert!(poll!(&mut second).is_pending());
    assert_eq!(resolver.metrics().snapshot().counters["flight_joined"], 1);
    respond(&socket, &out, peer).await;
    let (first, second) = tokio::join!(first, second);
    for (reply, id, ip) in [
        (first.unwrap().message, 1, "192.0.2.10"),
        (second.unwrap().message, 2, "192.0.2.11"),
    ] {
        assert_eq!(reply.id, id);
        assert_eq!(reply.answers.len(), 1);
        let subnet = ecs::subnet(&reply).unwrap();
        assert_eq!(subnet.addr().to_string(), ip);
        assert_eq!(subnet.source_prefix(), 32);
    }
    assert_eq!(
        resolver.metrics().snapshot().counters["upstream_operations"],
        1
    );
    let hit = resolver
        .resolve(&b, "192.0.2.11".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(hit.message.answers.len(), 1);
    let metrics = resolver.metrics().snapshot();
    assert_eq!(metrics.counters["cache_hits"], 1);
    assert_eq!(metrics.counters["completed"], 3);
    assert_eq!(metrics.request_inflight, 0);
}

#[tokio::test]
async fn cancelling_first_waiter_does_not_cancel_the_other() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::from_config(&config(&socket));
    let a = query(1, "192.0.2.10");
    let mut first = Box::pin(resolver.resolve(&a, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut first).is_pending());
    let (out, peer) = receive_while(&socket, &mut first).await;
    let mut second = Box::pin(resolver.resolve(&a, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut second).is_pending());
    drop(first);
    assert_eq!(resolver.metrics().snapshot().upstream_inflight, 1);
    respond(&socket, &out, peer).await;
    assert_eq!(second.await.unwrap().message.answers.len(), 1);
    let metrics = resolver.metrics().snapshot();
    assert_eq!(metrics.counters["cancelled"], 1);
    assert_eq!(metrics.counters["completed"], 1);
    assert_eq!(metrics.upstream_inflight, 0);
}

#[tokio::test]
async fn budgets_reject_without_extra_io_and_last_cancel_releases_capacity() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(&socket);
    cfg.coalescing.max_groups = 1;
    cfg.coalescing.max_waiters = 2;
    let resolver = Resolver::from_config(&cfg);
    let a = query(1, "192.0.2.10");
    let b = query(2, "192.0.3.10");
    let mut first = Box::pin(resolver.resolve(&a, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut first).is_pending());
    let _ = receive_while(&socket, &mut first).await;
    let mut second = Box::pin(resolver.resolve(&a, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut second).is_pending());
    for (wire, peer) in [(&a, "192.0.2.10"), (&b, "192.0.3.10")] {
        assert_eq!(
            resolver
                .resolve(wire, peer.parse().unwrap())
                .await
                .unwrap()
                .message
                .response_code,
            ResponseCode::ServFail
        );
    }
    assert_eq!(
        resolver.metrics().snapshot().counters["upstream_operations"],
        1
    );
    drop((first, second));
    assert_eq!(resolver.metrics().snapshot().upstream_inflight, 0);
    let mut next = Box::pin(resolver.resolve(&b, "192.0.3.10".parse().unwrap()));
    assert!(poll!(&mut next).is_pending());
    let (out, peer) = receive_while(&socket, &mut next).await;
    respond(&socket, &out, peer).await;
    assert_eq!(next.await.unwrap().message.answers.len(), 1);
    assert_eq!(resolver.metrics().snapshot().counters["flight_rejected"], 2);
}

#[tokio::test]
async fn different_subnets_and_cookie_requests_do_not_share() {
    for cookie in [false, true] {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let resolver = Resolver::from_config(&config(&socket));
        let a = query(1, "192.0.2.10");
        let b = query(2, if cookie { "192.0.2.10" } else { "192.0.3.10" });
        let wires: Vec<_> = [a, b]
            .into_iter()
            .map(|bytes| {
                let mut q = protocol::decode(&bytes).unwrap();
                if cookie {
                    q.edns
                        .as_mut()
                        .unwrap()
                        .options_mut()
                        .insert(EdnsOption::Unknown(10, vec![1; 8]));
                }
                q.to_vec().unwrap()
            })
            .collect();
        let mut first = Box::pin(resolver.resolve(&wires[0], "192.0.2.10".parse().unwrap()));
        assert!(poll!(&mut first).is_pending());
        let (one, p1) = receive_while(&socket, &mut first).await;
        let mut second = Box::pin(
            resolver.resolve(
                &wires[1],
                if cookie { "192.0.2.10" } else { "192.0.3.10" }
                    .parse()
                    .unwrap(),
            ),
        );
        assert!(poll!(&mut second).is_pending());
        let (two, p2) = receive_while(&socket, &mut second).await;
        respond(&socket, &one, p1).await;
        respond(&socket, &two, p2).await;
        assert_eq!(first.await.unwrap().message.answers.len(), 1);
        assert_eq!(second.await.unwrap().message.answers.len(), 1);
        assert_eq!(resolver.metrics().snapshot().counters["flight_joined"], 0);
    }
}

#[tokio::test]
async fn late_joiner_does_not_restart_the_upstream_deadline() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(&socket);
    cfg.query_timeout_ms = 100;
    let resolver = Resolver::from_config(&cfg);
    let wire = query(1, "192.0.2.10");
    let mut first = Box::pin(resolver.resolve(&wire, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut first).is_pending());
    receive_while(&socket, &mut first).await;
    tokio::time::sleep(Duration::from_millis(70)).await;
    let mut second = Box::pin(resolver.resolve(&wire, "192.0.2.10".parse().unwrap()));
    assert!(poll!(&mut second).is_pending());
    let (a, b) = timeout(Duration::from_millis(70), async {
        tokio::join!(first, second)
    })
    .await
    .unwrap();
    assert_eq!(a.unwrap().message.response_code, ResponseCode::ServFail);
    assert_eq!(b.unwrap().message.response_code, ResponseCode::ServFail);
    let metrics = resolver.metrics().snapshot();
    assert_eq!(metrics.counters["upstream_operations"], 1);
    assert_eq!(metrics.counters["upstream_timeouts"], 1);
    assert_eq!(metrics.counters["responses_servfail"], 2);
}
