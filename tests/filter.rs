use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, CNAME},
    },
};
use parins::{config::Config, ecs, policy::Policy, protocol, resolver::Resolver};
use tokio::net::UdpSocket;

fn config(address: std::net::SocketAddr, rules: &str) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams = None;
    config.upstream = Some(address);
    config.query_timeout_ms = 100;
    config.filter = toml::from_str(&format!("enabled = true\n{rules}")).unwrap();
    config
}

fn query(name: &str, kind: RecordType) -> Message {
    let mut q = Message::new(27, MessageType::Query, OpCode::Query);
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    q.add_query(Query::query(name, kind));
    q.metadata.recursion_desired = true;
    q.metadata.checking_disabled = true;
    let mut edns = Edns::new();
    edns.set_dnssec_ok(true);
    q.edns = Some(edns);
    q
}

async fn resolve(resolver: &Resolver, q: &Message) -> Message {
    resolver
        .resolve(&q.to_vec().unwrap(), "192.0.2.10".parse().unwrap())
        .await
        .unwrap()
        .message
}

fn assert_blocked(q: &Message, reply: &Message) {
    assert_eq!(reply.response_code, ResponseCode::NoError);
    assert_eq!(reply.id, q.id);
    assert_eq!(reply.queries, q.queries);
    assert!(
        reply.answers.is_empty() && reply.authorities.is_empty() && reply.additionals.is_empty()
    );
    assert!(!reply.authoritative && !reply.authentic_data && !reply.truncation);
    assert_eq!(reply.recursion_desired, q.recursion_desired);
    assert_eq!(reply.checking_disabled, q.checking_disabled);
    assert!(reply.edns.as_ref().unwrap().flags().dnssec_ok);
}

#[tokio::test]
async fn direct_blocks_all_supported_types_without_upstream_and_preserves_ecs() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        upstream.local_addr().unwrap(),
        "block_suffix = ['ads.test']",
    );
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    for kind in [
        RecordType::A,
        RecordType::AAAA,
        RecordType::HTTPS,
        RecordType::SVCB,
        RecordType::CNAME,
        RecordType::TXT,
    ] {
        for subnet in [None, Some("0.0.0.0/0"), Some("192.0.2.10/32")] {
            let mut q = query("Ads.Test.", kind);
            ecs::set_subnet(&mut q, subnet.map(|value| value.parse().unwrap()));
            let reply = resolve(&resolver, &q).await;
            assert_blocked(&q, &reply);
            match ecs::subnet(&q) {
                Some(original) => {
                    let echo = ecs::subnet(&reply).unwrap();
                    assert_eq!(echo.addr(), original.addr());
                    assert_eq!(echo.source_prefix(), original.source_prefix());
                }
                None => assert!(ecs::subnet(&reply).is_none()),
            }
        }
    }
    let mut buffer = [0; 4096];
    assert_eq!(
        upstream.try_recv(&mut buffer).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn filtering_cannot_bypass_protocol_or_ecs_validation() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        upstream.local_addr().unwrap(),
        "block_suffix = ['ads.test']",
    );
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    let mut q = query("ads.test", RecordType::ANY);
    assert_eq!(
        resolve(&resolver, &q).await.response_code,
        ResponseCode::Refused
    );
    q.queries[0].set_query_type(RecordType::A);
    ecs::set_subnet(&mut q, Some("198.51.100.0/24".parse().unwrap()));
    assert_eq!(
        resolve(&resolver, &q).await.response_code,
        ResponseCode::Refused
    );
    q.queries.push(q.queries[0].clone());
    assert_eq!(
        resolve(&resolver, &q).await.response_code,
        ResponseCode::FormErr
    );
}

#[tokio::test]
async fn cname_blocking_applies_to_cold_and_warm_independent_subnet_answers() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(
        upstream.local_addr().unwrap(),
        "block_suffix = ['ads.test']\nallow_exact = ['alias.test']",
    );
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    let mock = tokio::spawn(async move {
        for network in ["192.0.2.0", "192.0.3.0"] {
            let mut buffer = [0; 4096];
            let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
            let q = protocol::decode(&buffer[..length]).unwrap();
            let mut subnet = ecs::subnet(&q).unwrap();
            assert_eq!(subnet.addr().to_string(), network);
            subnet.set_scope_prefix(24);
            let mut reply = protocol::error_response(&q, ResponseCode::NoError);
            reply.metadata.authentic_data = true;
            reply.metadata.authoritative = true;
            for (owner, target) in [("middle.test", "ads.test"), ("alias.test", "middle.test")] {
                reply.add_answer(Record::from_rdata(
                    Name::from_ascii(owner).unwrap(),
                    60,
                    RData::CNAME(CNAME(Name::from_ascii(target).unwrap())),
                ));
            }
            let address = Record::from_rdata(
                Name::from_ascii("ads.test").unwrap(),
                60,
                RData::A(A::new(192, 0, 2, 99)),
            );
            reply.answers.push(address.clone());
            reply.authorities.push(address.clone());
            reply.additionals.push(address);
            ecs::set_subnet(&mut reply, Some(subnet));
            upstream
                .send_to(&reply.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
        // Closing the mock proves any further cache miss cannot get a valid answer.
    });
    for (id, peer) in [
        (1, "192.0.2.10"),
        (2, "192.0.2.11"),
        (3, "192.0.3.10"),
        (4, "192.0.3.11"),
    ] {
        let mut q = query(
            if id % 2 == 0 {
                "ALIAS.Test."
            } else {
                "alias.test"
            },
            RecordType::A,
        );
        q.metadata.id = id;
        ecs::set_subnet(&mut q, Some(format!("{peer}/32").parse().unwrap()));
        let reply = resolver
            .resolve(&q.to_vec().unwrap(), peer.parse().unwrap())
            .await
            .unwrap()
            .message;
        assert_blocked(&q, &reply);
        let returned = ecs::subnet(&reply).unwrap();
        assert_eq!(returned.addr().to_string(), peer);
        assert_eq!(
            (returned.source_prefix(), returned.scope_prefix()),
            (32, 24)
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), mock)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn allow_exception_reaches_upstream_and_keeps_normal_cache_behavior() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::from_config(&config(
        upstream.local_addr().unwrap(),
        "block_suffix = ['test']\nallow_suffix = ['safe.test']",
    ));
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..length]).unwrap();
        let mut reply = protocol::error_response(&q, ResponseCode::NoError);
        reply.add_answer(Record::from_rdata(
            q.queries[0].name().clone(),
            60,
            RData::A(A::new(192, 0, 2, 1)),
        ));
        upstream
            .send_to(&reply.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    for id in [1, 2] {
        let mut q = query("www.safe.test", RecordType::A);
        q.metadata.id = id;
        let reply = resolve(&resolver, &q).await;
        assert_eq!(reply.id, id);
        assert_eq!(reply.answers.len(), 1);
        assert_eq!(reply.answers[0].data, RData::A(A::new(192, 0, 2, 1)));
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), mock)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn local_rule_file_is_bounded_and_failed_reload_keeps_old_policy() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rules.toml");
    std::fs::write(&path, "enabled = true\nblock_suffix = ['ads.test']").unwrap();
    let mut active = Policy::load(&path).unwrap();
    let blocked = Name::from_ascii("www.ads.test").unwrap();
    assert!(active.blocks(&blocked));
    for invalid in [
        "enabled = true\nunknown = []",
        "enabled = true\nblock_exact = ['https://ads.test']",
        "enabled = true\nblock_exact = [",
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(Policy::load(&path).is_err());
        if let Ok(next) = Policy::load(&path) {
            active = next;
        }
        assert!(active.blocks(&blocked));
    }
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(Policy::load(&path).is_err());
    std::fs::File::create(&path)
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    assert!(
        Policy::load(&path)
            .unwrap_err()
            .to_string()
            .contains("exceeds 8 MiB")
    );
    assert!(active.blocks(&blocked));
    std::fs::write(&path, "enabled = false").unwrap();
    active = Policy::load(&path).unwrap();
    assert!(!active.blocks(&blocked));
}

#[tokio::test]
async fn reload_changes_cached_cname_filter_but_inflight_keeps_starting_snapshot() {
    use std::{sync::Arc, time::Duration};
    use tokio::time::timeout;

    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(upstream.local_addr().unwrap(), "block_exact = ['ads.test']");
    config.query_timeout_ms = 2000;
    let resolver = Arc::new(Resolver::from_config(&config));
    let q = query("alias.test", RecordType::A);
    let active_resolver = resolver.clone();
    let active_query = q.clone();
    let pending = tokio::spawn(async move { resolve(&active_resolver, &active_query).await });
    let mut buffer = [0; 4096];
    let (length, peer) = timeout(Duration::from_secs(1), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let outbound = protocol::decode(&buffer[..length]).unwrap();
    // The active request has snapshotted the blocking generation before upstream IO.
    resolver.replace_policy(Policy::default());
    let mut answer = protocol::error_response(&outbound, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        outbound.queries[0].name().clone(),
        60,
        RData::CNAME(CNAME(Name::from_ascii("ads.test").unwrap())),
    ));
    answer.add_answer(Record::from_rdata(
        Name::from_ascii("ads.test").unwrap(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    upstream
        .send_to(&answer.to_vec().unwrap(), peer)
        .await
        .unwrap();
    assert_blocked(
        &q,
        &timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap(),
    );

    // Cache contains the original response, not the old generation's synthesized block.
    assert_eq!(resolve(&resolver, &q).await.answers.len(), 2);
    resolver.replace_policy(toml::from_str("enabled = true\nblock_exact = ['ads.test']").unwrap());
    assert_blocked(&q, &resolve(&resolver, &q).await);
    resolver
        .replace_policy(toml::from_str("enabled = true\nblock_exact = ['alias.test']").unwrap());
    assert_blocked(&q, &resolve(&resolver, &q).await);
    resolver.replace_policy(Policy::default());
    assert_eq!(resolve(&resolver, &q).await.answers.len(), 2);
    assert_eq!(
        upstream.try_recv(&mut buffer).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
