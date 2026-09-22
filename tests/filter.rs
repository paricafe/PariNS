use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{config::Config, ecs, resolver::Resolver};
use tokio::net::UdpSocket;

fn config(address: std::net::SocketAddr, rules: &str) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstream = address;
    config.query_timeout_ms = 100;
    config.filter = toml::from_str(&format!("enabled = true\n{rules}")).unwrap();
    config
}

fn query(name: &str, kind: RecordType) -> Message {
    let mut q = Message::new(27, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(Name::from_ascii(name).unwrap(), kind));
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
