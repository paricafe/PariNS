//! A caller that leaves after its upstream query was sent lets the query finish
//! in the bounded refresh budget, only to fill the cache.
mod common;

use std::{net::IpAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::ResponseCode,
    rr::{RData, Record, RecordType, rdata::A},
};
use parins::{config::Config, protocol, resolver::Resolver};
use tokio::{
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
    time::timeout,
};

const PEER: &str = "192.0.2.10";

fn config(servers: Vec<String>) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = servers;
    config
}

/// Starts a resolution and returns once `upstream` received its query.
async fn started(
    resolver: &Arc<Resolver>,
    name: &str,
    upstream: &UdpSocket,
) -> (JoinHandle<()>, Vec<u8>, std::net::SocketAddr) {
    let task = tokio::spawn({
        let resolver = resolver.clone();
        let wire = common::query(7, name, RecordType::A).to_vec().unwrap();
        async move {
            resolver
                .resolve_with_transport(&wire, PEER.parse::<IpAddr>().unwrap(), "doh")
                .await;
        }
    });
    let mut buffer = [0; 4096];
    let (length, peer) = timeout(Duration::from_secs(1), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    (task, buffer[..length].to_vec(), peer)
}

async fn cancel(task: JoinHandle<()>) {
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

async fn answer(upstream: &UdpSocket, query: &[u8], peer: std::net::SocketAddr) {
    let query = protocol::decode(query).unwrap();
    let mut response = protocol::error_response(&query, ResponseCode::NoError);
    response.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        300,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    upstream
        .send_to(&response.to_vec().unwrap(), peer)
        .await
        .unwrap();
}

async fn until(mut done: impl FnMut() -> bool) {
    timeout(Duration::from_secs(1), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

fn counter(resolver: &Resolver, name: &str) -> u64 {
    resolver.metrics().snapshot().counters[name]
}

#[tokio::test]
async fn unsent_work_is_cancelled_with_its_caller() {
    // The upstream accepts TCP but never answers the TLS handshake.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(Resolver::from_config(&config(vec![format!(
        "tls://{}",
        listener.local_addr().unwrap()
    )])));
    let task = tokio::spawn({
        let resolver = resolver.clone();
        let wire = common::query(7, "unsent.test.", RecordType::A)
            .to_vec()
            .unwrap();
        async move {
            resolver
                .resolve_with_transport(&wire, PEER.parse::<IpAddr>().unwrap(), "dot")
                .await;
        }
    });
    let (_connection, _) = timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    cancel(task).await;
    until(|| resolver.metrics().snapshot().upstream_inflight == 0).await;
    assert_eq!(resolver.refresh_snapshot().scheduled, 0);
}

#[tokio::test]
async fn work_over_the_refresh_budget_is_cancelled() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(vec![upstream.local_addr().unwrap().to_string()]);
    config.cache.prefetch.max_inflight = 1;
    let resolver = Arc::new(Resolver::from_config(&config));
    let (first, query, peer) = started(&resolver, "first.test.", &upstream).await;
    let (second, _, _) = started(&resolver, "second.test.", &upstream).await;
    cancel(first).await;
    cancel(second).await;
    // Only the first fill fits in the one refresh slot.
    until(|| resolver.metrics().snapshot().upstream_inflight == 1).await;
    let refresh = resolver.refresh_snapshot();
    assert_eq!((refresh.active, refresh.rejected), (1, 1));
    answer(&upstream, &query, peer).await;
    until(|| resolver.refresh_snapshot().active == 0).await;
    assert_eq!(counter(&resolver, "cache_store_admitted"), 1);
}

#[tokio::test]
async fn invalidation_stops_a_cache_fill_from_writing_back() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(Resolver::from_config(&config(vec![
        upstream.local_addr().unwrap().to_string(),
    ])));
    let (task, query, peer) = started(&resolver, "invalidated.test.", &upstream).await;
    cancel(task).await;
    assert_eq!(resolver.refresh_snapshot().active, 1);
    resolver.cache().invalidate(None, None, None);
    answer(&upstream, &query, peer).await;
    until(|| resolver.refresh_snapshot().active == 0).await;
    assert_eq!(counter(&resolver, "cache_store_admitted"), 0);
    assert_eq!(counter(&resolver, "cache_reason_epoch_changed"), 1);
    assert_eq!(resolver.refresh_snapshot().failure, 1);
}

#[tokio::test]
async fn replacing_or_stopping_the_generation_cancels_a_cache_fill() {
    for stop in [false, true] {
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = config(vec![upstream.local_addr().unwrap().to_string()]);
        let resolver = Arc::new(Resolver::from_config(&config));
        let (task, _, _) = started(&resolver, "stopped.test.", &upstream).await;
        cancel(task).await;
        assert_eq!(resolver.refresh_snapshot().active, 1);
        if stop {
            resolver.shutdown_refresh().await;
        } else {
            resolver.replace_cache(config.cache.clone());
        }
        until(|| resolver.metrics().snapshot().upstream_inflight == 0).await;
        assert_eq!(counter(&resolver, "cache_store_admitted"), 0);
    }
}

#[tokio::test]
async fn parallel_losers_are_still_cancelled_during_a_cache_fill() {
    let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = config(vec![
        first.local_addr().unwrap().to_string(),
        second.local_addr().unwrap().to_string(),
    ]);
    config.upstreams.mode = parins::upstreams::Mode::Parallel;
    let resolver = Arc::new(Resolver::from_config(&config));
    let (task, query, peer) = started(&resolver, "parallel.test.", &first).await;
    let mut buffer = [0; 4096];
    timeout(Duration::from_secs(1), second.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    cancel(task).await;
    answer(&first, &query, peer).await;
    until(|| resolver.refresh_snapshot().active == 0).await;
    assert_eq!(resolver.metrics().snapshot().upstream_inflight, 0);
    assert_eq!(counter(&resolver, "cache_store_admitted"), 1);
    assert_eq!(counter(&resolver, "upstream_attempt_succeeded"), 1);
    assert_eq!(counter(&resolver, "upstream_attempt_cancelled"), 1);
    // The cached answer serves the next query without upstream IO.
    let reply = resolver
        .resolve_with_transport(
            &common::query(8, "parallel.test.", RecordType::A)
                .to_vec()
                .unwrap(),
            PEER.parse().unwrap(),
            "doh",
        )
        .await
        .unwrap();
    assert_eq!(reply.message.answers.len(), 1);
    assert_eq!(counter(&resolver, "upstream_operations"), 1);
}

#[tokio::test]
async fn a_joined_caller_that_leaves_last_hands_over_the_sent_work() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Arc::new(Resolver::from_config(&config(vec![
        upstream.local_addr().unwrap().to_string(),
    ])));
    let (leader, query, peer) = started(&resolver, "joined.test.", &upstream).await;
    let follower = tokio::spawn({
        let resolver = resolver.clone();
        let wire = common::query(9, "joined.test.", RecordType::A)
            .to_vec()
            .unwrap();
        async move {
            resolver
                .resolve_with_transport(&wire, PEER.parse::<IpAddr>().unwrap(), "doh")
                .await;
        }
    });
    until(|| counter(&resolver, "flight_joined") == 1).await;
    cancel(leader).await;
    // The follower still waits, so no refresh slot is taken yet.
    assert_eq!(resolver.refresh_snapshot().scheduled, 0);
    cancel(follower).await;
    assert_eq!(resolver.refresh_snapshot().active, 1);
    answer(&upstream, &query, peer).await;
    until(|| resolver.refresh_snapshot().active == 0).await;
    assert_eq!(counter(&resolver, "cache_store_admitted"), 1);
}
