use std::time::Duration;

use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RecordType,
        rdata::opt::{ClientSubnet, EdnsOption},
    },
};
use parins::{
    config::{Config, EcsConfig},
    ecs::{self, Context, Scope},
    protocol,
    resolver::Resolver,
};
use tokio::net::UdpSocket;

fn query(subnet: Option<&str>) -> Message {
    let mut query = Message::new(7, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("example.test.").unwrap(),
        RecordType::A,
    ));
    ecs::set_subnet(&mut query, subnet.map(|s| s.parse().unwrap()));
    query
}

fn enabled() -> EcsConfig {
    EcsConfig {
        enabled: true,
        ..Default::default()
    }
}

#[test]
fn derives_masked_ipv4_ipv6_and_honors_opt_out() {
    for (peer, expected) in [
        ("192.0.2.123", "192.0.2.0/24"),
        ("2001:db8:1234:5678::9", "2001:db8:1234:5600::/56"),
    ] {
        let (out, _) = Context::prepare(&query(None), peer.parse().unwrap(), &enabled()).unwrap();
        assert_eq!(
            ecs::subnet(&out).unwrap(),
            expected.parse::<ClientSubnet>().unwrap()
        );
    }
    let (out, context) = Context::prepare(
        &query(Some("0.0.0.0/0")),
        "192.0.2.123".parse().unwrap(),
        &enabled(),
    )
    .unwrap();
    assert_eq!(ecs::subnet(&out).unwrap().source_prefix(), 0);
    assert_eq!(
        context.cache_scope(&out),
        Some(Scope::Privacy { ipv4: true })
    );
    let (out, _) = Context::prepare(
        &query(Some("192.0.0.0/16")),
        "192.0.2.123".parse().unwrap(),
        &enabled(),
    )
    .unwrap();
    assert_eq!(ecs::subnet(&out).unwrap().source_prefix(), 16);
}

#[test]
fn rejects_forgery_and_disabled_mode_strips_ecs() {
    let input = query(Some("198.51.100.0/24"));
    assert!(matches!(
        Context::prepare(&input, "192.0.2.123".parse().unwrap(), &enabled()),
        Err(ResponseCode::Refused)
    ));
    let (out, _) = Context::prepare(
        &input,
        "192.0.2.123".parse().unwrap(),
        &EcsConfig::default(),
    )
    .unwrap();
    assert!(ecs::subnet(&out).is_none());
}

#[test]
fn downstream_echoes_original_source_and_never_leaks_inserted_ecs() {
    let input = query(Some("192.0.2.123/32"));
    let (out, context) =
        Context::prepare(&input, "192.0.2.123".parse().unwrap(), &enabled()).unwrap();
    assert_eq!(ecs::subnet(&out).unwrap().source_prefix(), 24);
    let mut reply = protocol::error_response(&out, ResponseCode::NoError);
    context.finish(&input, &mut reply, Some(20));
    let echoed = ecs::subnet(&reply).unwrap();
    assert_eq!(
        echoed.addr(),
        "192.0.2.123".parse::<std::net::IpAddr>().unwrap()
    );
    assert_eq!((echoed.source_prefix(), echoed.scope_prefix()), (32, 20));
    let input = query(None);
    let (out, context) =
        Context::prepare(&input, "192.0.2.123".parse().unwrap(), &enabled()).unwrap();
    let mut reply = out;
    context.finish(&input, &mut reply, Some(24));
    assert!(reply.edns.is_none());
}

#[test]
fn malformed_ecs_lengths_padding_scope_and_duplicates_are_rejected() {
    for raw in [
        vec![0, 1, 0, 0, 1], // too many address bytes (ignored by typed decoder alone)
        vec![0, 1, 24, 0, 192, 0], // too few
        vec![0, 1, 25, 0, 192, 0, 2, 1], // nonzero padding bits
        vec![0, 1, 24, 33, 192, 0, 2], // invalid scope
        vec![0, 3, 0, 0],    // unknown family
    ] {
        let mut q = query(None);
        let mut edns = Edns::new();
        edns.options_mut().insert(EdnsOption::Unknown(8, raw));
        q.edns = Some(edns);
        assert!(
            matches!(protocol::request(&q.to_vec().unwrap()), protocol::Request::Reply(r) if r.response_code == ResponseCode::FormErr)
        );
    }
    let mut q = query(Some("192.0.2.0/24"));
    q.edns
        .as_mut()
        .unwrap()
        .options_mut()
        .insert(EdnsOption::Subnet("192.0.2.0/24".parse().unwrap()));
    assert!(protocol::decode(&q.to_vec().unwrap()).is_err());
}

#[test]
fn mismatched_responses_and_ambiguous_cache_scopes_are_distinct() {
    let (out, context) =
        Context::prepare(&query(None), "192.0.2.123".parse().unwrap(), &enabled()).unwrap();
    let mut response = protocol::error_response(&out, ResponseCode::NoError);
    assert!(context.cache_scope(&response).is_none());
    ecs::set_subnet(
        &mut response,
        Some(ClientSubnet::new("192.0.2.0".parse().unwrap(), 24, 28)),
    );
    assert!(ecs::response_matches(&out, &response));
    assert!(context.cache_scope(&response).is_none());
    ecs::set_subnet(&mut response, Some("198.51.100.0/24".parse().unwrap()));
    assert!(!ecs::response_matches(&out, &response));
}

#[tokio::test]
async fn refused_nonzero_ecs_retries_anonymously_once() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstream = upstream.local_addr().unwrap();
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    let mock = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        for (source, code) in [(24, ResponseCode::Refused), (0, ResponseCode::NoError)] {
            let (n, peer) =
                tokio::time::timeout(Duration::from_secs(1), upstream.recv_from(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
            let q = protocol::decode(&bytes[..n]).unwrap();
            let subnet = ecs::subnet(&q).unwrap();
            assert_eq!(subnet.source_prefix(), source);
            let mut reply = protocol::error_response(&q, code);
            ecs::set_subnet(&mut reply, Some(subnet));
            upstream
                .send_to(&reply.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let reply = resolver
        .resolve(
            &query(Some("192.0.2.123/32")).to_vec().unwrap(),
            "192.0.2.123".parse().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reply.message.response_code, ResponseCode::NoError);
    let returned = ecs::subnet(&reply.message).unwrap();
    assert_eq!(
        (returned.source_prefix(), returned.scope_prefix()),
        (32, 32)
    );
    mock.await.unwrap();
}
