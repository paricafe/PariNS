use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, TXT},
    },
};
use parins::{
    config::Config,
    ecs, protocol,
    query_log::{ListOptions, UpstreamRelation},
    resolver::Resolver,
    upstreams::diagnostics::{ActualProtocol, Outcome, Reason, Stage},
};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

fn query(name: &str) -> Message {
    let mut q = Message::new(7, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    q.metadata.recursion_desired = true;
    q.metadata.checking_disabled = true;
    let mut edns = Edns::new();
    edns.set_dnssec_ok(true);
    q.edns = Some(edns);
    ecs::set_subnet(&mut q, Some("192.0.2.0/24".parse().unwrap()));
    q
}

#[tokio::test]
async fn real_upstream_cache_filter_error_and_opt_in_history() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (n, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..n]).unwrap();
        let mut response = protocol::error_response(&q, ResponseCode::NoError);
        response.edns = q.edns.clone();
        response.add_answer(Record::from_rdata(
            q.queries[0].name().clone(),
            60,
            RData::A(A::new(192, 0, 2, 42)),
        ));
        socket
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![address.to_string()];
    config.ecs.enabled = true;
    config.query_log.enabled = true;
    config.filter = toml::from_str("enabled = true\nblock_suffix = ['blocked.test']").unwrap();
    let resolver = Resolver::from_config(&config);
    let q = query("cache.test.").to_vec().unwrap();
    let peer = "192.0.2.10".parse().unwrap();
    for _ in 0..2 {
        resolver
            .resolve_with_transport(&q, peer, "doh")
            .await
            .unwrap();
    }
    task.await.unwrap();
    resolver
        .resolve_with_transport(&query("blocked.test.").to_vec().unwrap(), peer, "tcp")
        .await
        .unwrap();
    assert!(
        resolver
            .resolve_with_transport(&[1, 2], peer, "udp")
            .await
            .is_none()
    );
    resolver.query_log().flush().await.unwrap();
    let page = resolver
        .query_log()
        .list(ListOptions::default())
        .await
        .unwrap();
    assert_eq!(page.total, 4);
    let (dropped, blocked, fresh, upstream) = (
        &page.entries[0],
        &page.entries[1],
        &page.entries[2],
        &page.entries[3],
    );
    assert_eq!(dropped.status, "dropped");
    assert_eq!(blocked.status, "blocked");
    assert_eq!(blocked.cache, "blocked");
    assert!(blocked.upstream.is_none());
    assert_eq!(fresh.cache, "fresh");
    assert!(fresh.upstream.is_none());
    assert!(fresh.outgoing_ecs.is_none());
    assert_eq!(fresh.answer[0].data, "192.0.2.42");
    assert_eq!(fresh.transport, "doh");
    assert!(fresh.edns && fresh.dnssec_ok && fresh.checking_disabled && fresh.recursion_desired);
    assert_eq!(upstream.cache, "upstream");
    assert_eq!(
        resolver
            .query_log()
            .list(ListOptions {
                search: Some(address.to_string()),
                ..Default::default()
            })
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert!(
        upstream
            .upstream
            .as_ref()
            .unwrap()
            .contains(&address.to_string())
    );
    assert_eq!(
        upstream.incoming_ecs.as_deref(),
        Some("192.0.2.0/24 (scope /0)")
    );
    assert_eq!(
        upstream.outgoing_ecs.as_deref(),
        Some("192.0.2.0/24 (scope /0)")
    );
    let filtered = resolver
        .query_log()
        .list(ListOptions {
            search: Some("CACHE.TEST".into()),
            status: Some("success".into()),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filtered.entries.len(), 1);
    assert_eq!(filtered.entries[0].id, fresh.id);
    let next = resolver
        .query_log()
        .list(ListOptions {
            search: Some("cache.test".into()),
            before_id: filtered.next_cursor,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(next.entries.len(), 1);
    assert_eq!(next.entries[0].id, upstream.id);
    assert_eq!(
        resolver
            .query_log()
            .clear(resolver.query_log().epoch())
            .await
            .unwrap()
            .removed,
        4
    );
    assert_eq!(
        resolver
            .query_log()
            .list(ListOptions::default())
            .await
            .unwrap()
            .total,
        0
    );
    config.query_log.enabled = false;
    let disabled = Resolver::from_config(&config);
    disabled
        .resolve(&query("blocked.test.").to_vec().unwrap(), peer)
        .await;
    assert!(
        !disabled
            .query_log()
            .list(ListOptions::default())
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(
        disabled
            .query_log()
            .list(ListOptions::default())
            .await
            .unwrap()
            .total,
        0
    );
}

#[tokio::test]
async fn response_detail_is_bounded_and_inflight_clear_does_not_refill() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.query_log.enabled = true;
    config.ecs.enabled = false;
    let resolver = Resolver::from_config(&config);
    let mut message = query("many.test.");
    message.queries[0].set_query_type(RecordType::TXT);
    let q = message.to_vec().unwrap();
    let pending = resolver.resolve(&q, "127.0.0.1".parse().unwrap());
    let serve = async {
        let mut buffer = [0; 4096];
        let (n, peer) = socket.recv_from(&mut buffer).await.unwrap();
        resolver
            .query_log()
            .clear(resolver.query_log().epoch())
            .await
            .unwrap();
        let q = protocol::decode(&buffer[..n]).unwrap();
        let mut response = protocol::error_response(&q, ResponseCode::NoError);
        for index in 0..18 {
            response.add_answer(Record::from_rdata(
                q.queries[0].name().clone(),
                60,
                RData::TXT(TXT::new(if index == 0 {
                    vec!["x".repeat(200), "y".repeat(200), "z".repeat(200)]
                } else {
                    vec!["small".into()]
                })),
            ));
        }
        socket
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    };
    let (reply, _) = tokio::join!(pending, serve);
    assert_eq!(reply.unwrap().message.answers.len(), 18);
    resolver.query_log().flush().await.unwrap();
    assert_eq!(
        resolver
            .query_log()
            .list(ListOptions::default())
            .await
            .unwrap()
            .total,
        0
    );
    // Cache hit after clearing is a genuinely new request, and details stay bounded.
    resolver
        .resolve(&q, "127.0.0.1".parse().unwrap())
        .await
        .unwrap();
    resolver.query_log().flush().await.unwrap();
    let page = resolver
        .query_log()
        .list(ListOptions::default())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].answer.len(), 16);
    assert!(page.entries[0].answer_truncated);
    assert!(page.entries[0].answer.iter().all(|r| r.data.len() <= 512));
}

#[tokio::test]
async fn stale_fallback_records_actual_failed_exchange_and_cached_answer() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (n, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..n]).unwrap();
        socket
            .send_to(
                &protocol::error_response(&q, ResponseCode::ServFail)
                    .to_vec()
                    .unwrap(),
                peer,
            )
            .await
            .unwrap();
    });
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![address.to_string()];
    config.query_log.enabled = true;
    config.ecs.enabled = false;
    config.cache.stale.enabled = true;
    let resolver = Resolver::from_config(&config);
    let q = query("stale.test.");
    let mut response = protocol::error_response(&q, ResponseCode::NoError);
    response.add_answer(Record::from_rdata(
        q.queries[0].name().clone(),
        1,
        RData::A(A::new(192, 0, 2, 42)),
    ));
    resolver.cache().insert(
        &q,
        &response,
        ecs::Scope::NoEcs,
        Instant::now() - Duration::from_secs(2),
    );
    let reply = resolver
        .resolve(&q.to_vec().unwrap(), "127.0.0.1".parse().unwrap())
        .await
        .unwrap();
    upstream.await.unwrap();
    assert_eq!(reply.message.response_code, ResponseCode::NoError);
    resolver.query_log().flush().await.unwrap();
    let page = resolver
        .query_log()
        .list(ListOptions::default())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    let entry = &page.entries[0];
    assert_eq!(entry.cache, "stale");
    assert_eq!(entry.status, "success");
    assert!(
        entry
            .upstream
            .as_ref()
            .unwrap()
            .contains(&address.to_string())
    );
    assert_eq!(entry.answer[0].data, "192.0.2.42");
    assert!(
        entry.failure_stage.is_none() && entry.failure_reason.is_none(),
        "DNS SERVFAIL is not a network failure"
    );
}

#[tokio::test]
async fn servfail_and_timeout_keep_trace_factual() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![address.to_string()];
    config.query_log.enabled = true;
    config.ecs.enabled = true;
    config.query_timeout_ms = 30;
    let resolver = Resolver::from_config(&config);
    let q = query("error.test.").to_vec().unwrap();
    let serve = async {
        let mut buffer = [0; 4096];
        let (n, peer) = socket.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..n]).unwrap();
        socket
            .send_to(
                &protocol::error_response(&q, ResponseCode::ServFail)
                    .to_vec()
                    .unwrap(),
                peer,
            )
            .await
            .unwrap();
    };
    tokio::join!(resolver.resolve(&q, "192.0.2.10".parse().unwrap()), serve);
    // Keep the UDP socket open without replying: no endpoint won this exchange.
    resolver
        .resolve(&q, "192.0.2.10".parse().unwrap())
        .await
        .unwrap();
    resolver.query_log().flush().await.unwrap();
    let page = resolver
        .query_log()
        .list(ListOptions::default())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    for entry in &page.entries {
        assert_eq!(entry.status, "error");
        assert_eq!(entry.rcode.as_deref(), Some("SERVFAIL"));
        assert_eq!(
            entry.outgoing_ecs.as_deref(),
            Some("192.0.2.0/24 (scope /0)")
        );
    }
    assert!(page.entries[0].upstream.is_none());
    let timeout = &page.entries[0];
    assert_eq!(timeout.failure_stage, Some(Stage::ResponseRead));
    assert_eq!(timeout.failure_reason, Some(Reason::Deadline));
    let attempts = timeout.upstream_attempts.as_ref().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Outcome::Failed);
    assert_eq!(attempts[0].reason, Some(Reason::Deadline));
    let servfail = &page.entries[1];
    assert!(servfail.failure_stage.is_none() && servfail.failure_reason.is_none());
    let attempts = servfail.upstream_attempts.as_ref().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Outcome::Succeeded);
    assert_eq!(attempts[0].reason, None);
    assert_eq!(servfail.upstream_attempts_omitted, Some(0));
    assert!(
        page.entries[1]
            .upstream
            .as_ref()
            .unwrap()
            .contains(&address.to_string())
    );
}

#[tokio::test]
async fn shared_exchange_logs_leader_and_follower_but_counts_actual_attempt_once() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.query_log.enabled = true;
    config.ecs.enabled = true;
    let resolver = Resolver::from_config(&config);
    let wire = query("shared.test.").to_vec().unwrap();
    let mut leader =
        Box::pin(resolver.resolve_with_transport(&wire, "192.0.2.10".parse().unwrap(), "dot"));
    assert!(futures_util::poll!(&mut leader).is_pending());
    let mut buffer = [0; 4096];
    let (length, peer) = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)) => result.unwrap().unwrap(),
        _ = &mut leader => panic!("leader returned before receiving upstream query"),
    };
    let outgoing = protocol::decode(&buffer[..length]).unwrap();
    let mut follower =
        Box::pin(resolver.resolve_with_transport(&wire, "192.0.2.11".parse().unwrap(), "doh"));
    assert!(futures_util::poll!(&mut follower).is_pending());
    assert_eq!(resolver.metrics().snapshot().counters["flight_joined"], 1);
    let response = protocol::error_response(&outgoing, ResponseCode::NoError);
    socket
        .send_to(&response.to_vec().unwrap(), peer)
        .await
        .unwrap();
    let (leader, follower) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(leader, follower)
    })
    .await
    .unwrap();
    assert!(leader.is_some() && follower.is_some());
    resolver.query_log().flush().await.unwrap();
    let page = resolver
        .query_log()
        .list(ListOptions::default())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    let leader = page
        .entries
        .iter()
        .find(|entry| entry.client == "192.0.2.10")
        .unwrap();
    let follower = page
        .entries
        .iter()
        .find(|entry| entry.client == "192.0.2.11")
        .unwrap();
    assert!(matches!(
        leader.upstream_relation,
        Some(UpstreamRelation::Leader)
    ));
    assert!(matches!(
        follower.upstream_relation,
        Some(UpstreamRelation::Follower)
    ));
    assert_eq!(
        serde_json::to_value(&leader.upstream_attempts).unwrap(),
        serde_json::to_value(&follower.upstream_attempts).unwrap()
    );
    for entry in &page.entries {
        let attempts = entry.upstream_attempts.as_ref().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].protocol, Some(ActualProtocol::Udp));
        assert_eq!(attempts[0].outcome, Outcome::Succeeded);
        assert_eq!(entry.upstream_attempts_omitted, Some(0));
        assert!(entry.failure_stage.is_none() && entry.failure_reason.is_none());
        let json = serde_json::to_value(entry).unwrap();
        assert!(
            json.get("upstream_trace").is_none(),
            "no superseded public trace contract"
        );
    }
    let counters = resolver.metrics().snapshot().counters;
    assert_eq!(counters["upstream_operations"], 1);
    assert_eq!(counters["upstream_attempt_succeeded"], 1);
    assert_eq!(counters["upstream_attempt_failed"], 0);
    assert_eq!(counters["completed"], 2);
}

#[tokio::test]
async fn cancelled_foreground_is_logged_once_unless_cleared_while_pending() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![socket.local_addr().unwrap().to_string()];
    config.query_log.enabled = true;
    let resolver = std::sync::Arc::new(Resolver::from_config(&config));
    for clear in [false, true] {
        let resolver_task = resolver.clone();
        let task = tokio::spawn(async move {
            resolver_task
                .resolve_with_transport(
                    &query("pending.test.").to_vec().unwrap(),
                    "192.0.2.10".parse().unwrap(),
                    "doh",
                )
                .await
        });
        let mut bytes = [0; 4096];
        tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        if clear {
            resolver
                .query_log()
                .clear(resolver.query_log().epoch())
                .await
                .unwrap();
        }
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        resolver.query_log().flush().await.unwrap();
        let page = resolver
            .query_log()
            .list(ListOptions::default())
            .await
            .unwrap();
        if clear {
            assert_eq!(page.total, 0);
        } else {
            assert_eq!(page.total, 1);
            let entry = &page.entries[0];
            assert_eq!(entry.cache, "cancelled");
            assert_eq!(entry.status, "dropped");
            assert_eq!(entry.name, "pending.test.");
            assert_eq!(entry.transport, "doh");
            assert!(entry.upstream.is_none() && entry.rcode.is_none() && entry.answer.is_empty());
        }
    }
}
