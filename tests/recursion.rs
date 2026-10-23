mod common;

use std::time::Instant;

use hickory_proto::{
    op::{Edns, Message, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, CNAME},
    },
};
use parins::{
    config::{Config, ResolutionMode},
    ecs::Scope,
    protocol,
    resolver::Resolver,
};

fn config(mode: ResolutionMode) -> Config {
    let text = include_str!("../parins.example.toml")
        .replace("[filter]\nenabled = false", "[filter]\nenabled = true")
        .replace("block_exact = []", "block_exact = [\"blocked.test\"]");
    let mut config = Config::parse(&text).unwrap();
    config.resolution.mode = mode;
    config.ecs.enabled = false;
    config
}

fn query(name: &str, dnssec_ok: Option<bool>, ad: bool) -> Message {
    let mut query = common::query(9, name, RecordType::A);
    query.metadata.recursion_desired = true;
    query.metadata.authentic_data = ad;
    if let Some(dnssec_ok) = dnssec_ok {
        let mut edns = Edns::new();
        edns.set_dnssec_ok(dnssec_ok);
        query.edns = Some(edns);
    }
    query
}

/// A validated answer as the recursion source stores it: AD set.
fn validated(query: &Message, target: Option<&str>) -> Message {
    let mut response = protocol::error_response(query, ResponseCode::NoError);
    response.metadata.authentic_data = true;
    let name = query.queries[0].name().clone();
    let owner = match target {
        Some(target) => {
            let target = Name::from_ascii(target).unwrap();
            response.add_answer(Record::from_rdata(
                name,
                60,
                RData::CNAME(CNAME(target.clone())),
            ));
            target
        }
        None => name,
    };
    response.add_answer(Record::from_rdata(
        owner,
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    response
}

async fn answer(resolver: &Resolver, query: &Message, target: Option<&str>) -> Message {
    resolver.cache().insert_decision_if_epoch(
        query,
        &validated(query, target),
        Scope::NoEcs,
        Instant::now(),
        resolver.cache().epoch(),
    );
    resolver
        .resolve(&query.to_vec().unwrap(), "192.0.2.10".parse().unwrap())
        .await
        .unwrap()
        .message
}

#[tokio::test]
async fn recursive_mode_sets_ad_only_for_dnssec_aware_clients() {
    let resolver = Resolver::try_from_config(&config(ResolutionMode::Recursive)).unwrap();
    for (dnssec_ok, ad, expected) in [
        (Some(true), false, true),
        (Some(false), true, true),
        (Some(false), false, false),
        (None, false, false),
    ] {
        let reply = answer(&resolver, &query("secure.test.", dnssec_ok, ad), None).await;
        assert_eq!(reply.response_code, ResponseCode::NoError);
        assert_eq!(reply.authentic_data, expected, "DO {dnssec_ok:?} AD {ad}");
    }
}

#[tokio::test]
async fn forwarded_answers_never_carry_ad() {
    let resolver = Resolver::try_from_config(&config(ResolutionMode::Forward)).unwrap();
    let reply = answer(&resolver, &query("secure.test.", Some(true), true), None).await;
    assert_eq!(reply.answers.len(), 1);
    assert!(!reply.authentic_data);
}

#[tokio::test]
async fn filtered_answers_lose_ad() {
    let resolver = Resolver::try_from_config(&config(ResolutionMode::Recursive)).unwrap();
    let reply = answer(
        &resolver,
        &query("alias.test.", Some(true), false),
        Some("blocked.test."),
    )
    .await;
    assert!(
        reply
            .answers
            .iter()
            .all(|record| record.record_type() != RecordType::A),
        "{reply:?}"
    );
    assert!(!reply.authentic_data);
}

#[tokio::test]
async fn only_recursive_mode_exports_recursion_metrics() {
    let recursive = Resolver::try_from_config(&config(ResolutionMode::Recursive)).unwrap();
    let names: Vec<_> = recursive
        .recursion_metrics()
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    for name in [
        "recursion_failed_referral",
        "recursion_outbound_rate_waits",
        "recursion_outbound_inflight",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    let forward = Resolver::try_from_config(&config(ResolutionMode::Forward)).unwrap();
    assert!(forward.recursion_metrics().is_empty());
}
