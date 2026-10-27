//! An upstream SERVFAIL is cached briefly (RFC 9520) for the exact sent network.
mod common;

use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{RData, Record, RecordType, rdata::A, rdata::opt::ClientSubnet},
};
use parins::{config::Config, ecs, ecs::Scope, protocol, resolver::Resolver};
use tokio::{net::UdpSocket, task::JoinHandle};

/// An upstream that answers every query with `code`, or not at all, and
/// echoes ECS with scope /0 (a claim that the answer suits every network).
fn upstream(socket: UdpSocket, code: Option<ResponseCode>) -> (Arc<AtomicUsize>, JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let task = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        loop {
            let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
            counted.fetch_add(1, Ordering::SeqCst);
            let Some(code) = code else { continue };
            let query = protocol::decode(&buffer[..length]).unwrap();
            let mut response = protocol::error_response(&query, code);
            if let Some(sent) = ecs::subnet(&query) {
                ecs::set_subnet(
                    &mut response,
                    Some(ClientSubnet::new(sent.addr(), sent.source_prefix(), 0)),
                );
            }
            socket
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    (calls, task)
}

async fn setup(
    code: Option<ResponseCode>,
    configure: impl FnOnce(&mut Config),
) -> (Resolver, Arc<AtomicUsize>, JoinHandle<()>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    configure(&mut config);
    let (calls, task) = upstream(socket, code);
    (Resolver::from_config(&config), calls, task)
}

async fn resolve(resolver: &Resolver, name: &str, peer: &str) -> Message {
    let wire = common::query(1, name, RecordType::A).to_vec().unwrap();
    resolver
        .resolve(&wire, peer.parse::<IpAddr>().unwrap())
        .await
        .unwrap()
        .message
}

#[tokio::test]
async fn a_cached_failure_is_answered_without_upstream_io_until_it_expires() {
    let (resolver, calls, task) = setup(Some(ResponseCode::ServFail), |config| {
        config.cache.servfail_ttl_secs = 1;
    })
    .await;
    for _ in 0..3 {
        let reply = resolve(&resolver, "failing.test.", "192.0.2.10").await;
        assert_eq!(reply.response_code, ResponseCode::ServFail);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let counters = resolver.metrics().snapshot().counters;
    assert_eq!(counters["cache_hits"], 2);
    assert_eq!(counters["upstream_operations"], 1);
    assert_eq!(resolver.cache().snapshot()["servfail_hits"], 2);
    tokio::time::sleep(Duration::from_millis(1050)).await;
    resolve(&resolver, "failing.test.", "192.0.2.10").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn a_disabled_failure_cache_queries_the_upstream_each_time() {
    let (resolver, calls, task) = setup(Some(ResponseCode::ServFail), |config| {
        config.cache.servfail_ttl_secs = 0;
    })
    .await;
    for _ in 0..2 {
        resolve(&resolver, "failing.test.", "192.0.2.10").await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn a_cached_failure_stays_in_the_sent_network() {
    let (resolver, calls, task) = setup(Some(ResponseCode::ServFail), |config| {
        config.ecs.enabled = true;
    })
    .await;
    resolve(&resolver, "failing.test.", "192.0.2.10").await;
    resolve(&resolver, "failing.test.", "192.0.2.20").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "same /24");
    // The upstream's scope /0 does not spread the failure to other networks.
    resolve(&resolver, "failing.test.", "198.51.100.10").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn timeouts_are_not_cached() {
    let (resolver, calls, task) = setup(None, |config| {
        config.query_timeout_ms = 50;
    })
    .await;
    for _ in 0..2 {
        let reply = resolve(&resolver, "silent.test.", "192.0.2.10").await;
        assert_eq!(reply.response_code, ResponseCode::ServFail);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.cache().snapshot()["servfail_entries"], 0);
    task.abort();
}

#[tokio::test]
async fn a_cached_failure_prefers_a_usable_stale_answer() {
    let (resolver, calls, task) = setup(Some(ResponseCode::ServFail), |config| {
        config.cache.stale.enabled = true;
    })
    .await;
    let query = common::query(1, "stale.test.", RecordType::A);
    let mut answer = protocol::error_response(&query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        1,
        RData::A(A::new(192, 0, 2, 7)),
    ));
    let cache = resolver.cache();
    cache.insert_decision_if_epoch(
        &query,
        &answer,
        Scope::NoEcs,
        Instant::now() - Duration::from_secs(5),
        cache.epoch(),
    );
    // The first query fails upstream and falls back; the second is answered
    // from the cached failure, which falls back the same way.
    for expected_calls in [1, 1] {
        let reply = resolve(&resolver, "stale.test.", "192.0.2.10").await;
        assert_eq!(reply.response_code, ResponseCode::NoError);
        assert_eq!(reply.answers[0].data, RData::A(A::new(192, 0, 2, 7)));
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    assert_eq!(resolver.cache().snapshot()["servfail_hits"], 1);
    task.abort();
}
