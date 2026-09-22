use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::opt::EdnsOption,
        rdata::{A, SOA},
    },
};
use parins::{
    config::Config,
    ecs::{self, Context, Scope},
    protocol,
    resolver::Resolver,
};
use tokio::{net::UdpSocket, task::JoinHandle};

struct Upstream {
    address: SocketAddr,
    count: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Upstream {
    async fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let selected = mode.clone();
        let task = tokio::spawn(async move {
            let mut buffer = [0; 4096];
            loop {
                let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
                let query = protocol::decode(&buffer[..length]).unwrap();
                observed.fetch_add(1, SeqCst);
                let reply = match selected.load(SeqCst) {
                    0 => positive(&query, 60, 42),
                    1 => protocol::error_response(&query, ResponseCode::ServFail),
                    2 => protocol::error_response(&query, ResponseCode::Refused),
                    3 => continue, // Deliberately silent upstream, for bounded timeout/cancellation.
                    4 if ecs::subnet(&query).is_some_and(|ecs| ecs.source_prefix() > 0) => {
                        protocol::error_response(&query, ResponseCode::Refused)
                    }
                    4 => protocol::error_response(&query, ResponseCode::ServFail),
                    5 => positive(&query, 0, 42),
                    6 => with_ede(positive(&query, 0, 42)),
                    7 => {
                        let mut reply = protocol::error_response(&query, ResponseCode::NXDomain);
                        reply.add_authority(Record::from_rdata(
                            Name::from_ascii("test.").unwrap(),
                            60,
                            RData::SOA(SOA::new(
                                Name::from_ascii("ns.test.").unwrap(),
                                Name::from_ascii("hostmaster.test.").unwrap(),
                                1,
                                60,
                                60,
                                3600,
                                60,
                            )),
                        ));
                        with_ede(reply)
                    }
                    _ => unreachable!(),
                };
                socket
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            }
        });
        Self {
            address,
            count,
            mode,
            task,
        }
    }

    fn config(&self) -> Config {
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.upstreams.servers = vec![self.address.to_string()];
        config.ecs.enabled = false;
        config.query_timeout_ms = 60;
        config.cache.stale.enabled = true;
        config.cache.prefetch.enabled = true;
        config.cache.prefetch.min_hits = 1;
        config.cache.prefetch.remaining_percent = 20;
        config.cache.prefetch.rate_per_sec = 100;
        config
    }
}

fn query(name: &str) -> Message {
    let mut query = Message::new(1, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    query
}

fn positive(query: &Message, ttl: u32, value: u8) -> Message {
    let mut answer = protocol::error_response(query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        ttl,
        RData::A(A::new(192, 0, 2, value)),
    ));
    ecs::set_subnet(&mut answer, ecs::subnet(query));
    answer
}

fn fill(resolver: &Resolver, query: &Message, age: u64) {
    resolver.cache().insert(
        query,
        &positive(query, 60, 1),
        Scope::NoEcs,
        Instant::now() - Duration::from_secs(age),
    );
}

fn with_ede(mut response: Message) -> Message {
    response
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(15, vec![0, 0]));
    response
}

async fn resolve(resolver: &Resolver, query: &Message) -> Message {
    resolver
        .resolve(&query.to_vec().unwrap(), "192.0.2.1".parse().unwrap())
        .await
        .unwrap()
        .message
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition completed");
}

#[tokio::test]
async fn defaults_do_not_prefetch_or_serve_expired_answers() {
    let upstream = Upstream::start().await;
    let mut config = upstream.config();
    config.cache.prefetch.enabled = false;
    config.cache.stale.enabled = false;
    let resolver = Resolver::from_config(&config);
    let query = query("default.test.");
    fill(&resolver, &query, 59);
    assert_eq!(
        resolve(&resolver, &query).await.answers[0].data,
        RData::A(A::new(192, 0, 2, 1))
    );
    assert_eq!(resolver.refresh_snapshot().scheduled, 0);
    fill(&resolver, &query, 61);
    upstream.mode.store(1, SeqCst);
    assert_eq!(
        resolve(&resolver, &query).await.response_code,
        ResponseCode::ServFail
    );
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn stale_positive_is_failure_only_and_has_bounded_reply_ttl() {
    let upstream = Upstream::start().await;
    let config = upstream.config();
    let resolver = Resolver::from_config(&config);
    let query = query("stale.test.");
    fill(&resolver, &query, 61);
    // A successful upstream answer always wins over retained stale data.
    assert_eq!(
        resolve(&resolver, &query).await.answers[0].data,
        RData::A(A::new(192, 0, 2, 42))
    );
    for mode in [1, 3] {
        fill(&resolver, &query, 61);
        upstream.mode.store(mode, SeqCst);
        let stale = resolve(&resolver, &query).await;
        assert_eq!(stale.answers[0].data, RData::A(A::new(192, 0, 2, 1)));
        assert_eq!(stale.answers[0].ttl, config.cache.stale.reply_ttl_secs);
    }
    fill(&resolver, &query, 61);
    upstream.mode.store(2, SeqCst);
    assert_eq!(
        resolve(&resolver, &query).await.response_code,
        ResponseCode::Refused
    );
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn ede_success_prevents_subsequent_servfail_from_resurrecting_stale() {
    let upstream = Upstream::start().await;
    let mut config = upstream.config();
    config.cache.prefetch.enabled = false;
    let resolver = Resolver::from_config(&config);
    let mut query = query("ede-stale.test.");
    query.edns = Some(Edns::new());
    for mode in [6, 7] {
        fill(&resolver, &query, 61);
        upstream.mode.store(mode, SeqCst);
        let response = resolve(&resolver, &query).await;
        assert_eq!(
            response.response_code,
            if mode == 6 {
                ResponseCode::NoError
            } else {
                ResponseCode::NXDomain
            }
        );
        assert!(
            response
                .edns
                .as_ref()
                .unwrap()
                .options()
                .options
                .iter()
                .any(|(code, _)| u16::from(*code) == 15)
        );
        upstream.mode.store(1, SeqCst);
        let response = resolve(&resolver, &query).await;
        assert_eq!(
            response.response_code,
            ResponseCode::ServFail,
            "EDE success mode {mode} must supersede the old stale answer"
        );
        assert!(response.answers.is_empty());
    }
    assert_eq!(upstream.count.load(SeqCst), 4);
    assert_eq!(resolver.cache().snapshot()["stale_hits"], 0);
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn negative_and_other_ecs_namespaces_never_supply_stale() {
    let upstream = Upstream::start().await;
    upstream.mode.store(1, SeqCst);
    let mut config = upstream.config();
    let resolver = Resolver::from_config(&config);
    let query = query("negative.test.");
    let mut negative = protocol::error_response(&query, ResponseCode::NXDomain);
    negative.add_authority(Record::from_rdata(
        Name::from_ascii("test.").unwrap(),
        1,
        RData::SOA(SOA::new(
            Name::from_ascii("ns.test.").unwrap(),
            Name::from_ascii("hostmaster.test.").unwrap(),
            1,
            60,
            60,
            3600,
            1,
        )),
    ));
    resolver.cache().insert(
        &query,
        &negative,
        Scope::NoEcs,
        Instant::now() - Duration::from_secs(2),
    );
    assert_eq!(
        resolve(&resolver, &query).await.response_code,
        ResponseCode::ServFail
    );
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    fill(&resolver, &query, 61); // No-ECS must not satisfy an ECS request.
    assert_eq!(
        resolve(&resolver, &query).await.response_code,
        ResponseCode::ServFail
    );
    // A Refused then anonymous retry failure must not resurrect original-subnet stale.
    let (outbound, context) =
        Context::prepare(&query, "192.0.2.1".parse().unwrap(), &config.ecs).unwrap();
    let answer = positive(&outbound, 60, 1);
    resolver.cache().insert(
        &query,
        &answer,
        context.cache_scope(&answer).unwrap(),
        Instant::now() - Duration::from_secs(61),
    );
    upstream.mode.store(4, SeqCst);
    assert_eq!(
        resolve(&resolver, &query).await.response_code,
        ResponseCode::ServFail
    );
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn prefetch_deduplicates_and_replaces_hot_entry() {
    let upstream = Upstream::start().await;
    let resolver = Resolver::from_config(&upstream.config());
    let query = query("hot.test.");
    fill(&resolver, &query, 59);
    for _ in 0..16 {
        assert_eq!(
            resolve(&resolver, &query).await.response_code,
            ResponseCode::NoError
        );
    }
    until(|| resolver.refresh_snapshot().success == 1).await;
    assert_eq!(upstream.count.load(SeqCst), 1);
    assert_eq!(
        resolve(&resolver, &query).await.answers[0].data,
        RData::A(A::new(192, 0, 2, 42))
    );
    assert_eq!(resolver.refresh_snapshot().active, 0);
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn failed_prefetch_backs_off_and_limits_are_bounded() {
    let upstream = Upstream::start().await;
    upstream.mode.store(1, SeqCst);
    let resolver = Resolver::from_config(&upstream.config());
    let query = query("backoff.test.");
    fill(&resolver, &query, 59);
    resolve(&resolver, &query).await;
    until(|| resolver.refresh_snapshot().failure == 1).await;
    for _ in 0..10 {
        resolve(&resolver, &query).await;
    }
    assert_eq!(resolver.refresh_snapshot().scheduled, 1);
    assert!(resolver.refresh_snapshot().rejected >= 10);
    resolver.shutdown_refresh().await;

    let mut config = upstream.config();
    config.cache.prefetch.max_inflight = 1;
    upstream.mode.store(3, SeqCst);
    let resolver = Resolver::from_config(&config);
    for index in 0..10 {
        let query = query_for_index(index);
        fill(&resolver, &query, 59);
        resolve(&resolver, &query).await;
    }
    assert_eq!(resolver.refresh_snapshot().scheduled, 1);
    assert_eq!(resolver.refresh_snapshot().active, 1);
    resolver.shutdown_refresh().await;
    assert_eq!(resolver.refresh_snapshot().active, 0);
}

fn query_for_index(index: usize) -> Message {
    query(&format!("name{index}.test."))
}

#[tokio::test]
async fn prefetch_rate_limit_and_cache_replacement_cancel_old_work() {
    let upstream = Upstream::start().await;
    let mut config = upstream.config();
    config.cache.prefetch.rate_per_sec = 1;
    let resolver = Resolver::from_config(&config);
    let first = query_for_index(1);
    fill(&resolver, &first, 59);
    resolve(&resolver, &first).await;
    until(|| resolver.refresh_snapshot().success == 1).await;
    let second = query_for_index(2);
    fill(&resolver, &second, 59);
    resolve(&resolver, &second).await;
    assert_eq!(resolver.refresh_snapshot().scheduled, 1);

    resolver.replace_cache(config.cache.clone());
    upstream.mode.store(3, SeqCst);
    fill(&resolver, &first, 59);
    let old = resolver.cache();
    resolve(&resolver, &first).await;
    until(|| upstream.count.load(SeqCst) == 2).await;
    resolver.replace_cache(config.cache.clone());
    assert!(!Arc::ptr_eq(&old, &resolver.cache()));
    assert_eq!(resolver.refresh_snapshot().scheduled, 0);
    assert!(resolver.cache().get(&first, None, Instant::now()).is_none());
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn invalidation_and_reconfiguration_do_not_join_or_refill_old_flights() {
    for replace in [false, true] {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
        config.ecs.enabled = false;
        config.query_timeout_ms = 2000;
        let resolver = Arc::new(Resolver::from_config(&config));
        let query = query("epoch.test.");
        let first_resolver = resolver.clone();
        let first_query = query.clone();
        let first = tokio::spawn(async move { resolve(&first_resolver, &first_query).await });
        let mut buffer = [0; 4096];
        let (length, first_peer) = socket.recv_from(&mut buffer).await.unwrap();
        let first_outbound = protocol::decode(&buffer[..length]).unwrap();
        if replace {
            resolver.replace_cache(config.cache.clone());
        } else {
            resolver.cache().invalidate(None, None, None);
        }
        let second_resolver = resolver.clone();
        let second_query = query.clone();
        let second = tokio::spawn(async move { resolve(&second_resolver, &second_query).await });
        let (length, second_peer) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buffer))
                .await
                .expect("new epoch starts independent upstream work")
                .unwrap();
        let second_outbound = protocol::decode(&buffer[..length]).unwrap();
        socket
            .send_to(
                &positive(&second_outbound, 60, 2).to_vec().unwrap(),
                second_peer,
            )
            .await
            .unwrap();
        assert_eq!(
            second.await.unwrap().answers[0].data,
            RData::A(A::new(192, 0, 2, 2))
        );
        socket
            .send_to(
                &positive(&first_outbound, 60, 1).to_vec().unwrap(),
                first_peer,
            )
            .await
            .unwrap();
        assert_eq!(
            first.await.unwrap().answers[0].data,
            RData::A(A::new(192, 0, 2, 1))
        );
        assert_eq!(
            resolve(&resolver, &query).await.answers[0].data,
            RData::A(A::new(192, 0, 2, 2)),
            "old flight must not overwrite new epoch entry"
        );
        resolver.shutdown_refresh().await;
    }
}

#[tokio::test]
async fn prefetch_keys_keep_ecs_namespaces_separate() {
    let upstream = Upstream::start().await;
    upstream.mode.store(3, SeqCst);
    let mut config = upstream.config();
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    let query = query("ecs-refresh.test.");
    for peer in ["192.0.2.1", "192.0.3.1"] {
        let (outbound, context) =
            Context::prepare(&query, peer.parse().unwrap(), &config.ecs).unwrap();
        let answer = positive(&outbound, 60, 1);
        resolver.cache().insert(
            &query,
            &answer,
            context.cache_scope(&answer).unwrap(),
            Instant::now() - Duration::from_secs(59),
        );
        for _ in 0..3 {
            let response = resolver
                .resolve(&query.to_vec().unwrap(), peer.parse().unwrap())
                .await
                .unwrap();
            assert_eq!(response.message.response_code, ResponseCode::NoError);
        }
    }
    assert_eq!(resolver.refresh_snapshot().scheduled, 2);
    assert_eq!(resolver.refresh_snapshot().active, 2);
    resolver.shutdown_refresh().await;
    assert_eq!(resolver.refresh_snapshot().active, 0);
}

#[tokio::test]
async fn uncacheable_refresh_response_backs_off_instead_of_reporting_success() {
    let upstream = Upstream::start().await;
    upstream.mode.store(5, SeqCst);
    let resolver = Resolver::from_config(&upstream.config());
    let query = query("ttl-zero.test.");
    fill(&resolver, &query, 59);
    resolve(&resolver, &query).await;
    until(|| resolver.refresh_snapshot().failure == 1).await;
    resolve(&resolver, &query).await;
    assert_eq!(resolver.refresh_snapshot().scheduled, 1);
    assert_eq!(resolver.refresh_snapshot().success, 0);
    resolver.shutdown_refresh().await;
}

#[tokio::test]
async fn foreground_shares_pending_prefetch_across_expiry_and_falls_back_per_caller() {
    for (fail, cancel_background) in [(false, false), (true, false), (false, true)] {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
        config.ecs.enabled = false;
        config.query_timeout_ms = 2000;
        config.cache.prefetch.enabled = true;
        config.cache.prefetch.min_hits = 1;
        config.cache.prefetch.remaining_percent = 20;
        config.cache.stale.enabled = true;
        let resolver = Arc::new(Resolver::from_config(&config));
        let query = query("expiry-race.test.");
        resolver.cache().insert(
            &query,
            &positive(&query, 2, 1),
            Scope::NoEcs,
            Instant::now() - Duration::from_millis(1600),
        );
        assert_eq!(
            resolve(&resolver, &query).await.answers[0].data,
            RData::A(A::new(192, 0, 2, 1))
        );
        let mut buffer = [0; 4096];
        let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let outbound = protocol::decode(&buffer[..length]).unwrap();
        // Keep the prefetch exchange pending until the original entry expires.
        tokio::time::timeout(Duration::from_secs(1), async {
            while resolver
                .cache()
                .peek(&query, None, Instant::now(), false)
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut callers = Vec::new();
        for _ in 0..2 {
            let resolver = resolver.clone();
            let query = query.clone();
            callers.push(tokio::spawn(
                async move { resolve(&resolver, &query).await },
            ));
        }
        until(|| resolver.metrics().snapshot().counters["flight_joined"] == 2).await;
        assert!(
            matches!(socket.try_recv_from(&mut buffer), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "foreground expiry must not issue another upstream query"
        );
        if cancel_background {
            resolver.shutdown_refresh().await;
            assert_eq!(resolver.refresh_snapshot().active, 0);
        }
        let answer = if fail {
            protocol::error_response(&outbound, ResponseCode::ServFail)
        } else {
            positive(&outbound, 60, 42)
        };
        socket
            .send_to(&answer.to_vec().unwrap(), peer)
            .await
            .unwrap();
        for caller in callers {
            let response = caller.await.unwrap();
            assert_eq!(
                response.answers[0].data,
                RData::A(A::new(192, 0, 2, if fail { 1 } else { 42 }))
            );
        }
        if !cancel_background {
            until(|| resolver.refresh_snapshot().active == 0).await;
            assert_eq!(resolver.refresh_snapshot().success, u64::from(!fail));
            assert_eq!(resolver.refresh_snapshot().failure, u64::from(fail));
        }
        assert_eq!(
            resolver.metrics().snapshot().counters["upstream_operations"],
            1
        );
        if fail {
            assert_eq!(
                resolver.cache().snapshot()["stale_hits"],
                2,
                "fallback accounting belongs to each foreground caller"
            );
        } else {
            assert_eq!(
                resolve(&resolver, &query).await.answers[0].data,
                RData::A(A::new(192, 0, 2, 42)),
                "there is no older competing exchange that could overwrite the refreshed entry"
            );
        }
        resolver.shutdown_refresh().await;
    }
}
