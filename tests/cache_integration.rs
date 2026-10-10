use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, CNAME, NS, SOA},
    },
};
use parins::{config::Config, ecs, policy::Policy, protocol, resolver::Resolver};
use tokio::{net::UdpSocket, task::JoinHandle};

struct Upstream {
    address: SocketAddr,
    count: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Upstream {
    async fn start(reply: impl Fn(&Message) -> Message + Send + 'static) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let task = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            loop {
                let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
                let q = protocol::decode(&buffer[..length]).unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                socket
                    .send_to(&reply(&q).to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            }
        });
        Self {
            address,
            count,
            task,
        }
    }

    fn resolver(&self) -> Resolver {
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.upstreams.servers = vec![self.address.to_string()];
        config.ecs.enabled = true;
        config.query_timeout_ms = 200;
        Resolver::from_config(&config)
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

fn query(subnet: Option<&str>, id: u16) -> Message {
    let mut q = Message::new(id, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(
        Name::from_ascii("example.test.").unwrap(),
        RecordType::A,
    ));
    ecs::set_subnet(&mut q, subnet.map(|s| s.parse().unwrap()));
    q
}

fn answer(q: &Message, value: u8, scope: u8) -> Message {
    let mut reply = protocol::error_response(q, ResponseCode::NoError);
    reply.add_answer(Record::from_rdata(
        q.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, value)),
    ));
    if let Some(mut subnet) = ecs::subnet(q) {
        subnet.set_scope_prefix(scope);
        ecs::set_subnet(&mut reply, Some(subnet));
    }
    reply
}

async fn resolve(resolver: &Resolver, subnet: Option<&str>, peer: &str, id: u16) -> Message {
    resolver
        .resolve(&query(subnet, id).to_vec().unwrap(), peer.parse().unwrap())
        .await
        .unwrap()
        .message
}

#[tokio::test]
async fn same_name_different_subnets_have_independent_answers_and_hits() {
    let upstream = Upstream::start(|q| {
        let Some(ecs) = ecs::subnet(q) else {
            panic!("missing outgoing ECS")
        };
        let IpAddr::V4(address) = ecs.addr() else {
            panic!("expected v4")
        };
        answer(q, address.octets()[2], 24)
    })
    .await;
    let resolver = upstream.resolver();
    for (peer, id, value) in [
        ("192.0.2.10", 1, 2),
        ("192.0.3.10", 2, 3),
        ("192.0.2.20", 3, 2),
        ("192.0.3.20", 4, 3),
    ] {
        let reply = resolve(&resolver, None, peer, id).await;
        assert_eq!(reply.answers[0].data, RData::A(A::new(192, 0, 2, value)));
        assert_eq!(reply.id, id);
        assert!(reply.edns.is_none());
    }
    assert_eq!(
        upstream.count(),
        2,
        "one upstream exchange per distinct /24"
    );
}

#[tokio::test]
async fn broader_scope_reuses_answer_but_echoes_current_client_ecs() {
    let upstream = Upstream::start(|q| answer(q, 42, 16)).await;
    let resolver = upstream.resolver();
    for (subnet, peer, id) in [
        ("192.0.2.10/32", "192.0.2.10", 1),
        ("192.0.3.99/32", "192.0.3.99", 2),
    ] {
        let reply = resolve(&resolver, Some(subnet), peer, id).await;
        let returned = ecs::subnet(&reply).unwrap();
        assert_eq!(returned.addr(), peer.parse::<IpAddr>().unwrap());
        assert_eq!(
            (returned.source_prefix(), returned.scope_prefix()),
            (32, 16)
        );
        assert_eq!(reply.id, id);
        assert_eq!(reply.answers.len(), 1);
    }
    assert_eq!(upstream.count(), 1);
}

#[tokio::test]
async fn privacy_global_scope_and_address_families_do_not_cross() {
    let upstream = Upstream::start(|q| {
        let subnet = ecs::subnet(q).unwrap();
        let value = if !subnet.addr().is_ipv4() {
            6
        } else if subnet.source_prefix() == 0 {
            1
        } else {
            2
        };
        answer(q, value, 0)
    })
    .await;
    let resolver = upstream.resolver();
    for (subnet, peer, value) in [
        ("0.0.0.0/0", "192.0.2.10", 1),
        ("192.0.2.10/32", "192.0.2.10", 2),
        ("198.51.100.10/32", "198.51.100.10", 2),
        ("0.0.0.0/0", "198.51.100.10", 1),
        ("2001:db8::/56", "2001:db8::1", 6),
    ] {
        let reply = resolve(&resolver, Some(subnet), peer, 1).await;
        assert_eq!(reply.answers[0].data, RData::A(A::new(192, 0, 2, value)));
    }
    assert_eq!(upstream.count(), 3);
}

#[tokio::test]
async fn narrower_than_source_response_is_still_not_cached() {
    let upstream = Upstream::start(|q| answer(q, 1, 28)).await;
    let resolver = upstream.resolver();
    for id in [1, 2] {
        assert_eq!(
            resolve(&resolver, None, "192.0.2.1", id)
                .await
                .answers
                .len(),
            1
        );
    }
    assert_eq!(upstream.count(), 2);
}

#[tokio::test]
async fn missing_ecs_uses_exact_sent_subnet_and_consistent_miss_hit_reply_scope() {
    let upstream = Upstream::start(|q| {
        let mut reply = answer(q, 1, 0);
        ecs::set_subnet(&mut reply, None);
        reply
    })
    .await;
    let resolver = upstream.resolver();
    for (peer, expected) in [("192.0.2.10", 1), ("192.0.2.20", 1), ("192.0.3.20", 2)] {
        let response = resolve(&resolver, None, peer, 1).await;
        assert_eq!(response.answers.len(), 1);
        assert!(response.edns.is_none());
        assert_eq!(upstream.count(), expected);
    }
    for (source, peer, expected) in [
        ("192.0.2.10/32", "192.0.2.10", 3),
        ("192.0.2.20/32", "192.0.2.20", 3),
    ] {
        let response = resolve(&resolver, Some(source), peer, 2).await;
        let echoed = ecs::subnet(&response).unwrap();
        assert_eq!((echoed.source_prefix(), echoed.scope_prefix()), (32, 24));
        assert_eq!(upstream.count(), expected);
    }
}

#[tokio::test]
async fn missing_ecs_exact_source_does_not_share_contained_prefix_or_privacy_family() {
    let upstream = Upstream::start(|q| {
        let mut reply = answer(q, 1, 0);
        ecs::set_subnet(&mut reply, None);
        reply
    })
    .await;
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![upstream.address.to_string()];
    config.ecs.enabled = true;
    config.ecs.ipv4_prefix = 32;
    let resolver = Resolver::from_config(&config);
    // /24 and /25 overlap for supersession, but they are never cache hits for
    // each other. The last two entries are separate family privacy namespaces.
    for (source, peer, count) in [
        ("192.0.2.0/24", "192.0.2.10", 1),
        ("192.0.2.0/24", "192.0.2.20", 1),
        ("192.0.2.0/25", "192.0.2.10", 2),
        ("192.0.2.0/24", "192.0.2.10", 3),
        ("0.0.0.0/0", "192.0.2.10", 4),
        ("::/0", "2001:db8::1", 5),
        ("0.0.0.0/0", "192.0.3.10", 5),
        ("::/0", "2001:db9::1", 5),
    ] {
        assert_eq!(
            resolve(&resolver, Some(source), peer, 3)
                .await
                .answers
                .len(),
            1
        );
        assert_eq!(upstream.count(), count, "{source}");
    }
}

#[tokio::test]
async fn refused_fallback_success_is_not_saved_under_original_subnet() {
    let upstream = Upstream::start(|q| {
        if ecs::subnet(q).unwrap().source_prefix() > 0 {
            protocol::error_response(q, ResponseCode::Refused)
        } else {
            answer(q, 1, 0)
        }
    })
    .await;
    let resolver = upstream.resolver();
    for id in [1, 2] {
        assert_eq!(
            resolve(&resolver, None, "192.0.2.1", id)
                .await
                .answers
                .len(),
            1
        );
    }
    assert_eq!(upstream.count(), 4);
}

#[tokio::test]
async fn negative_answers_are_cached_only_with_soa_proof() {
    let upstream = Upstream::start(|q| {
        let mut reply = protocol::error_response(q, ResponseCode::NXDomain);
        let soa = SOA::new(
            Name::from_ascii("ns.test.").unwrap(),
            Name::from_ascii("hostmaster.test.").unwrap(),
            1,
            60,
            60,
            3600,
            30,
        );
        reply.add_authority(Record::from_rdata(
            Name::from_ascii("test.").unwrap(),
            120,
            RData::SOA(soa),
        ));
        ecs::set_subnet(&mut reply, ecs::subnet(q));
        reply
    })
    .await;
    let resolver = upstream.resolver();
    assert_eq!(
        resolve(&resolver, None, "192.0.2.1", 1).await.response_code,
        ResponseCode::NXDomain
    );
    let hit = resolve(&resolver, None, "192.0.2.1", 2).await;
    assert_eq!(hit.id, 2);
    assert!(hit.authorities[0].ttl <= 30);
    assert_eq!(upstream.count(), 1);
}

async fn cname_negative_reuses_upstream_and_preserves_filtering(code: ResponseCode) {
    let upstream = Upstream::start(move |q| {
        let outgoing = ecs::subnet(q).unwrap();
        assert_eq!(outgoing.source_prefix(), 24);
        let mut reply = protocol::error_response(q, code);
        reply.add_answer(Record::from_rdata(
            q.queries[0].name().clone(),
            60,
            RData::CNAME(CNAME(Name::from_ascii("missing.other.test.").unwrap())),
        ));
        reply.add_authority(Record::from_rdata(
            Name::from_ascii("other.test.").unwrap(),
            120,
            RData::SOA(SOA::new(
                Name::from_ascii("ns.other.test.").unwrap(),
                Name::from_ascii("hostmaster.other.test.").unwrap(),
                1,
                60,
                60,
                3600,
                30,
            )),
        ));
        reply.add_authority(Record::from_rdata(
            Name::from_ascii("other.test.").unwrap(),
            90,
            RData::NS(NS(Name::from_ascii("ns.other.test.").unwrap())),
        ));
        reply.add_additional(Record::from_rdata(
            Name::from_ascii("ns.other.test.").unwrap(),
            100,
            RData::A(A::new(192, 0, 2, 53)),
        ));
        // No ECS echo: the resolver must retain ExactSource reuse and reconstruct
        // the original client's ECS consistently on upstream and fresh replies.
        ecs::set_subnet(&mut reply, None);
        reply
    })
    .await;
    let resolver = upstream.resolver();
    let cold = resolve(&resolver, Some("192.0.2.10/32"), "192.0.2.10", 10).await;
    assert_eq!(upstream.count(), 1);
    let fresh = resolve(&resolver, Some("192.0.2.20/32"), "192.0.2.20", 20).await;
    let counters = resolver.metrics().snapshot().counters;
    assert_eq!(
        (upstream.count(), counters["cache_lookup_fresh"]),
        (1, 1),
        "{code:?}: two sequential queries must need only one upstream exchange"
    );
    for (reply, peer, id) in [(&cold, "192.0.2.10", 10), (&fresh, "192.0.2.20", 20)] {
        assert_eq!(reply.id, id);
        assert_eq!(reply.queries, query(None, id).queries);
        assert_eq!(reply.response_code, code);
        assert!(!reply.authoritative && !reply.truncation);
        assert_eq!(reply.answers.len(), 1);
        assert_eq!(reply.authorities.len(), 2);
        assert_eq!(reply.additionals.len(), 1);
        let echoed = ecs::subnet(reply).unwrap();
        assert_eq!(echoed.addr(), peer.parse::<IpAddr>().unwrap());
        assert_eq!((echoed.source_prefix(), echoed.scope_prefix()), (32, 24));
    }
    for (original, cached) in cold
        .answers
        .iter()
        .chain(&cold.authorities)
        .chain(&cold.additionals)
        .zip(
            fresh
                .answers
                .iter()
                .chain(&fresh.authorities)
                .chain(&fresh.additionals),
        )
    {
        assert_eq!(cached.name, original.name);
        assert_eq!(cached.dns_class, original.dns_class);
        assert_eq!(cached.data, original.data);
        assert!(cached.ttl > 0 && cached.ttl <= original.ttl);
    }
    assert!(fresh.authorities[0].ttl <= 30);

    resolver
        .replace_policy(
            toml::from_str("enabled = true\nblock_exact = ['missing.other.test']").unwrap(),
        )
        .unwrap();
    // Filtering applies both to a fresh hit and a cold answer in another exact
    // ECS source. Neither synthesized block may replace the upstream message.
    for (peer, source, id, expected_upstream) in [
        ("192.0.2.20", "192.0.2.20/32", 30, 1),
        ("192.0.3.10", "192.0.3.10/32", 40, 2),
        ("192.0.3.20", "192.0.3.20/32", 50, 2),
    ] {
        let blocked = resolve(&resolver, Some(source), peer, id).await;
        assert_eq!(blocked.id, id);
        assert_eq!(blocked.queries, query(None, id).queries);
        assert_eq!(blocked.response_code, ResponseCode::NoError);
        assert!(blocked.answers.is_empty());
        assert!(blocked.authorities.is_empty());
        assert!(blocked.additionals.is_empty());
        let echoed = ecs::subnet(&blocked).unwrap();
        assert_eq!(echoed.addr(), peer.parse::<IpAddr>().unwrap());
        assert_eq!((echoed.source_prefix(), echoed.scope_prefix()), (32, 24));
        assert_eq!(upstream.count(), expected_upstream);
    }
    resolver.replace_policy(Policy::default()).unwrap();
    let unblocked = resolve(&resolver, Some("192.0.3.20/32"), "192.0.3.20", 60).await;
    assert_eq!(unblocked.response_code, code);
    assert_eq!(unblocked.answers[0].data, cold.answers[0].data);
    assert_eq!(unblocked.authorities[0].data, cold.authorities[0].data);
    assert!(unblocked.authorities[0].ttl <= 30);
    assert_eq!(upstream.count(), 2);
    let counters = resolver.metrics().snapshot().counters;
    assert_eq!(counters["cache_lookup_fresh"], 4);
    assert_eq!(counters["response_blocked"], 3);
}

#[tokio::test]
async fn cname_negative_nodata_reuses_upstream_and_preserves_filtering() {
    cname_negative_reuses_upstream_and_preserves_filtering(ResponseCode::NoError).await;
}

#[tokio::test]
async fn cname_negative_nxdomain_reuses_upstream_and_preserves_filtering() {
    cname_negative_reuses_upstream_and_preserves_filtering(ResponseCode::NXDomain).await;
}
