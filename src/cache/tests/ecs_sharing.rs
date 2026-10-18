use super::*;

fn exact(cidr: &str) -> Scope {
    Scope::ExactSource(cidr.parse().unwrap())
}

fn sharing_config(min_networks: u8) -> CacheConfig {
    let mut config = CacheConfig::default();
    config.ecs_sharing.enabled = true;
    config.ecs_sharing.min_networks = min_networks;
    config.ecs_sharing.revoke_cooldown_secs = 600;
    config
}

fn store(cache: &Cache, q: &Message, response: &Message, scope: Scope, now: Instant) {
    cache.insert_decision_if_epoch(q, response, scope, now, cache.epoch());
}

fn state(cache: &Cache, q: &Message, now: Instant) -> Value {
    cache.inspect(q.queries[0].name(), Some(q.queries[0].query_type()), now)["ecs_sharing"].clone()
}

fn scopes(cache: &Cache, q: &Message, now: Instant) -> Vec<String> {
    let mut scopes: Vec<_> = cache.inspect(q.queries[0].name(), None, now)["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| variant["scope"].as_str().unwrap().to_owned())
        .collect();
    scopes.sort();
    scopes
}

/// Three networks over two /16 prefixes: the third answer promotes the record.
const LEARNING: [&str; 3] = ["192.0.2.0/24", "192.0.3.0/24", "198.51.100.0/24"];

fn learn(cache: &Cache, q: &Message, response: &Message, now: Instant) {
    for network in LEARNING {
        store(cache, q, response, exact(network), now);
    }
}

#[test]
fn fingerprint_ignores_ttl_and_order_but_keeps_data_chain_and_negative_identity() {
    let q = query("print.test.");
    let question = &q.queries[0];
    let print = |response: &Message| sharing::fingerprint(question, response).unwrap();
    let mut two = answer(&q, 1, 60);
    two.add_answer(Record::from_rdata(
        name("print.test."),
        30,
        RData::A(A::new(192, 0, 2, 2)),
    ));
    let mut reordered = two.clone();
    reordered.answers.reverse();
    reordered.answers[0].ttl = 5;
    reordered.metadata.id = 999;
    assert_eq!(print(&two), print(&reordered));
    // Duplicate identical records describe the same RRset.
    let mut duplicated = two.clone();
    duplicated.add_answer(two.answers[0].clone());
    assert_eq!(print(&two), print(&duplicated));
    // Case-insensitive owner names, but RDATA and rcode are significant.
    let mut upper = answer(&q, 1, 60);
    upper.answers[0].name = name("PRINT.test.");
    assert_eq!(print(&answer(&q, 1, 60)), print(&upper));
    assert_ne!(print(&answer(&q, 1, 60)), print(&answer(&q, 2, 60)));
    assert_ne!(print(&two), print(&answer(&q, 1, 60)));

    // CNAME chains are part of the answer even when the final address agrees.
    let chained = |target: &str| {
        let mut response = protocol::error_response(&q, ResponseCode::NoError);
        response.add_answer(cname("print.test.", target, 60));
        response.add_answer(Record::from_rdata(
            name(target),
            60,
            RData::A(A::new(192, 0, 2, 9)),
        ));
        response
    };
    assert_ne!(
        print(&chained("a.cdn.test.")),
        print(&chained("b.cdn.test."))
    );
    let mut chain_reordered = chained("a.cdn.test.");
    chain_reordered.answers.reverse();
    assert_eq!(print(&chained("a.cdn.test.")), print(&chain_reordered));

    // Negative answers: rcode, SOA owner and negative classification, not SOA TTL.
    let negative = negative_answer(&q);
    let mut aged = negative.clone();
    aged.authorities[0].ttl = 3;
    assert_eq!(print(&negative), print(&aged));
    let mut other_zone = negative.clone();
    other_zone.authorities[0].name = name("print.test.");
    assert_ne!(print(&negative), print(&other_zone));
    let mut nodata = negative.clone();
    nodata.metadata.response_code = ResponseCode::NoError;
    assert_ne!(print(&negative), print(&nodata));
    assert_ne!(
        print(&cname_negative_answer(&q, ResponseCode::NXDomain)),
        print(&negative)
    );
}

#[test]
fn promotion_needs_k_distinct_networks_across_two_wide_prefixes() {
    let cache = Cache::new(sharing_config(3));
    let q = query("Shared.Test.");
    let response = answer(&q, 7, 60);
    let now = Instant::now();
    assert_eq!(state(&cache, &q, now), Value::Null);
    // Repeating one network and many networks inside one /16 never promotes.
    store(&cache, &q, &response, exact("10.1.1.0/24"), now);
    assert_eq!(state(&cache, &q, now), "observing");
    store(&cache, &q, &response, exact("10.1.1.0/24"), now);
    for network in ["10.1.2.0/24", "10.1.3.0/24", "10.1.4.0/24"] {
        store(&cache, &q, &response, exact(network), now);
    }
    assert_eq!(state(&cache, &q, now), "observing");
    assert!(cache.peek(&q, ecs("203.0.113.0/24"), now, false).is_none());
    assert_eq!(cache.snapshot()["ecs_shared_entries"], 0);
    // A sample from a second /16 completes K networks with the required span.
    store(&cache, &q, &response, exact("10.2.0.0/24"), now);
    assert_eq!(state(&cache, &q, now), "shared");
    // One learned entry replaces the per-network copies of the same answer.
    assert_eq!(scopes(&cache, &q, now), ["learned_v4"]);
    let hit = cache.lookup(&q, ecs("203.0.113.0/24"), now, false).unwrap();
    assert_eq!(hit.scope, Scope::Learned { ipv4: true });
    assert_eq!(hit.scope.reply_scope(ecs("203.0.113.0/24")), Some(24));
    assert_eq!(hit.message.answers[0].data, response.answers[0].data);
    let stats = cache.snapshot();
    assert_eq!(stats["ecs_shared_hits"], 1);
    assert_eq!(stats["ecs_shared_entries"], 1);
    assert_eq!(stats["ecs_sharing_revocations"], 0);
    assert_eq!(cache.diagnostics_snapshot()["ecs_shared_entries"], 1);
    // Later identical answers keep one learned copy.
    store(&cache, &q, &response, exact("203.0.113.0/24"), now);
    assert_eq!(scopes(&cache, &q, now), ["learned_v4"]);

    // IPv6 spans use /32 and K counts distinct networks.
    let cache = Cache::new(sharing_config(2));
    store(&cache, &q, &response, exact("2001:db8:1::/56"), now);
    store(&cache, &q, &response, exact("2001:db8:2::/56"), now);
    assert_eq!(state(&cache, &q, now), "observing");
    store(&cache, &q, &response, exact("2001:db9::/56"), now);
    assert_eq!(scopes(&cache, &q, now), ["learned_v6"]);
}

#[test]
fn different_answer_revokes_at_once_and_cooldown_blocks_relearning() {
    let cache = Cache::new(sharing_config(3));
    let q = query("revoke.test.");
    let (same, different) = (answer(&q, 1, 60), answer(&q, 2, 60));
    let now = Instant::now();
    learn(&cache, &q, &same, now);
    assert_eq!(scopes(&cache, &q, now), ["learned_v4"]);
    // A refresh from any network with a different answer removes the shared copy.
    store(&cache, &q, &different, exact("203.0.113.0/24"), now);
    assert_eq!(state(&cache, &q, now), "revoked");
    assert_eq!(scopes(&cache, &q, now), ["exact_ecs:203.0.113.0/24"]);
    assert!(cache.peek(&q, ecs("192.0.2.0/24"), now, false).is_none());
    assert_eq!(cache.snapshot()["ecs_sharing_revocations"], 1);
    assert_eq!(cache.snapshot()["ecs_shared_entries"], 0);
    // Within the cooldown identical answers stay per network.
    let later = now + Duration::from_secs(599);
    learn(&cache, &q, &same, later);
    assert_eq!(state(&cache, &q, later), "revoked");
    assert_eq!(scopes(&cache, &q, later).len(), 3);
    assert!(cache.peek(&q, ecs("10.0.0.0/24"), later, false).is_none());
    // After it, learning starts over.
    let after = now + Duration::from_secs(601);
    assert_eq!(state(&cache, &q, after), Value::Null);
    learn(&cache, &q, &same, after);
    assert_eq!(state(&cache, &q, after), "shared");
    assert_eq!(scopes(&cache, &q, after), ["learned_v4"]);

    // A contradiction while observing excludes the name the same way.
    let q = query("rotating.test.");
    store(&cache, &q, &answer(&q, 1, 60), exact("192.0.2.0/24"), now);
    store(
        &cache,
        &q,
        &answer(&q, 2, 60),
        exact("198.51.100.0/24"),
        now,
    );
    assert_eq!(state(&cache, &q, now), "revoked");
    learn(&cache, &q, &answer(&q, 1, 60), now);
    assert_eq!(cache.snapshot()["ecs_shared_entries"], 1);
    assert_eq!(cache.snapshot()["ecs_sharing_revocations"], 2);
}

#[test]
fn expired_evidence_is_not_contradicted_by_a_later_answer() {
    let cache = Cache::new(sharing_config(3));
    let q = query("moved.test.");
    let now = Instant::now();
    store(&cache, &q, &answer(&q, 1, 10), exact("192.0.2.0/24"), now);
    store(
        &cache,
        &q,
        &answer(&q, 1, 10),
        exact("198.51.100.0/24"),
        now,
    );
    // Both samples expired: a changed address starts a new observation.
    let later = now + Duration::from_secs(11);
    store(
        &cache,
        &q,
        &answer(&q, 2, 60),
        exact("203.0.113.0/24"),
        later,
    );
    assert_eq!(state(&cache, &q, later), "observing");
    store(&cache, &q, &answer(&q, 2, 60), exact("192.0.2.0/24"), later);
    assert_eq!(state(&cache, &q, later), "observing");
    store(
        &cache,
        &q,
        &answer(&q, 2, 60),
        exact("198.51.100.0/24"),
        later,
    );
    assert_eq!(state(&cache, &q, later), "shared");
}

#[test]
fn sharing_never_crosses_family_no_ecs_privacy_or_query_flags() {
    let cache = Cache::new(sharing_config(3));
    let q = query("family.test.");
    let now = Instant::now();
    learn(&cache, &q, &answer(&q, 1, 60), now);
    assert!(cache.peek(&q, ecs("203.0.113.0/24"), now, false).is_some());
    for outgoing in [ecs("2001:db8::/56"), ecs("0.0.0.0/0"), ecs("::/0"), None] {
        assert!(cache.peek(&q, outgoing, now, false).is_none());
    }
    let mut flagged = q.clone();
    flagged.metadata.checking_disabled = true;
    assert!(
        cache
            .peek(&flagged, ecs("203.0.113.0/24"), now, false)
            .is_none()
    );
    // Other namespaces of the same key are neither replaced nor taught.
    store(&cache, &q, &answer(&q, 2, 60), Scope::NoEcs, now);
    store(
        &cache,
        &q,
        &answer(&q, 3, 60),
        Scope::Privacy { ipv4: true },
        now,
    );
    store(&cache, &q, &answer(&q, 4, 60), exact("2001:db8::/56"), now);
    assert_eq!(state(&cache, &q, now), "shared");
    assert_eq!(
        scopes(&cache, &q, now),
        [
            "exact_ecs:2001:db8::/56",
            "learned_v4",
            "no_ecs",
            "privacy_v4"
        ]
    );
    let read = |outgoing| {
        cache
            .peek(&q, outgoing, now, false)
            .unwrap()
            .message
            .answers[0]
            .data
            .clone()
    };
    assert_eq!(read(ecs("203.0.113.0/24")), RData::A(A::new(192, 0, 2, 1)));
    assert_eq!(read(None), RData::A(A::new(192, 0, 2, 2)));
    assert_eq!(read(ecs("0.0.0.0/0")), RData::A(A::new(192, 0, 2, 3)));
    // A different-flag key learns independently.
    learn(&cache, &flagged, &answer(&flagged, 5, 60), now);
    assert_eq!(
        cache
            .peek(&flagged, ecs("203.0.113.0/24"), now, false)
            .unwrap()
            .message
            .answers[0]
            .data,
        RData::A(A::new(192, 0, 2, 5))
    );
}

#[test]
fn explicit_response_scope_revokes_but_scope_zero_does_not() {
    let cache = Cache::new(sharing_config(3));
    let q = query("scoped.test.");
    let now = Instant::now();
    learn(&cache, &q, &answer(&q, 1, 60), now);
    store(&cache, &q, &answer(&q, 1, 60), network("0.0.0.0/0"), now);
    assert_eq!(state(&cache, &q, now), "shared");
    assert_eq!(scopes(&cache, &q, now), ["0.0.0.0/0"]);
    learn(&cache, &q, &answer(&q, 1, 60), now);
    store(&cache, &q, &answer(&q, 1, 60), network("192.0.2.0/24"), now);
    assert_eq!(state(&cache, &q, now), "revoked");
    assert_eq!(scopes(&cache, &q, now), ["192.0.2.0/24"]);
}

#[test]
fn disabled_sharing_keeps_exact_source_reuse_and_reports_null() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("disabled.test.");
    let now = Instant::now();
    for network in [
        "192.0.2.0/24",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "10.0.0.0/24",
    ] {
        store(&cache, &q, &answer(&q, 1, 60), exact(network), now);
    }
    assert_eq!(scopes(&cache, &q, now).len(), 4);
    assert!(cache.peek(&q, ecs("172.16.0.0/24"), now, false).is_none());
    assert_eq!(state(&cache, &q, now), Value::Null);
}

#[test]
fn classification_table_is_bounded_lru_and_inspection_is_read_only() {
    let mut config = sharing_config(3);
    config.shards = 1;
    config.max_entries = 256;
    let cache = Cache::new(config);
    let now = Instant::now();
    let names: Vec<_> = (0..=1024)
        .map(|i| query(&format!("n{i}.bound.test.")))
        .collect();
    for q in &names[..1024] {
        store(&cache, q, &answer(q, 1, 60), exact("192.0.2.0/24"), now);
    }
    // Inspecting the oldest record must not refresh its LRU position.
    assert_eq!(state(&cache, &names[0], now), "observing");
    store(
        &cache,
        &names[1024],
        &answer(&names[1024], 1, 60),
        exact("192.0.2.0/24"),
        now,
    );
    assert_eq!(state(&cache, &names[0], now), Value::Null);
    assert_eq!(state(&cache, &names[1], now), "observing");
    assert_eq!(state(&cache, &names[1024], now), "observing");
    assert_eq!(cache.shards[0].lock().unwrap().sharing.len(), 1024);
}

#[test]
fn older_work_cannot_teach_or_refill_after_invalidation_or_revocation() {
    let cache = Cache::new(sharing_config(3));
    let q = query("race.test.");
    let same = answer(&q, 1, 60);
    let now = Instant::now();
    learn(&cache, &q, &same, now);
    let started = cache.epoch();
    // Clearing the name removes the shared entry and forgets its classification.
    assert_eq!(cache.invalidate(Some(&name("race.test.")), None, None), 1);
    assert_eq!(state(&cache, &q, now), Value::Null);
    let decision = cache.insert_decision_if_epoch(&q, &same, exact("203.0.113.0/24"), now, started);
    assert_eq!(decision.reason, Some(DecisionReason::EpochChanged));
    assert_eq!(state(&cache, &q, now), Value::Null);
    assert!(scopes(&cache, &q, now).is_empty());

    // Work begun while the name was shared and finishing after a revocation is
    // stored for its own network only, even though its answer matches.
    learn(&cache, &q, &same, now);
    let started = cache.epoch();
    store(&cache, &q, &answer(&q, 2, 60), exact("10.0.0.0/24"), now);
    let decision = cache.insert_decision_if_epoch(&q, &same, exact("203.0.113.0/24"), now, started);
    assert!(decision.admitted());
    assert_eq!(
        scopes(&cache, &q, now),
        ["exact_ecs:10.0.0.0/24", "exact_ecs:203.0.113.0/24"]
    );
    assert!(cache.peek(&q, ecs("192.0.2.0/24"), now, false).is_none());

    // Scope-filtered clearing of learned entries also resets classification;
    // clearing another namespace does not.
    let q = query("scoped-clear.test.");
    learn(&cache, &q, &same, now);
    cache.invalidate(Some(&name("scoped-clear.test.")), None, Some(Scope::NoEcs));
    assert_eq!(state(&cache, &q, now), "shared");
    let removed = cache.invalidate(
        Some(&name("scoped-clear.test.")),
        None,
        Some(Scope::Learned { ipv4: true }),
    );
    assert_eq!(removed, 1);
    assert_eq!(state(&cache, &q, now), Value::Null);
}

#[test]
fn learned_negative_answers_keep_negative_budget_ttl_and_no_stale_or_prefetch() {
    let mut config = sharing_config(3);
    config.stale.enabled = true;
    config.prefetch.enabled = true;
    let cache = Cache::new(config);
    let q = query("missing.test.");
    let now = Instant::now();
    learn(&cache, &q, &negative_answer(&q), now);
    let inspection = cache.inspect(&name("missing.test."), None, now);
    let variant = &inspection["variants"][0];
    assert_eq!(variant["scope"], "learned_v4");
    assert_eq!(variant["negative"], true);
    assert_eq!(variant["fresh_remaining_secs"], 30);
    assert_eq!(variant["retention_remaining_secs"], 30);
    assert_eq!(cache.snapshot()["negative_entries"], 1);
    let hit = cache.lookup(&q, ecs("203.0.113.0/24"), now, false).unwrap();
    assert!(!hit.refresh && !hit.stale);
    let expired = now + Duration::from_secs(31);
    assert!(
        cache
            .lookup(&q, ecs("203.0.113.0/24"), expired, true)
            .is_none()
    );

    // Positive learned entries follow the ordinary stale retention rule.
    let q = query("present.test.");
    learn(&cache, &q, &answer(&q, 1, 10), now);
    let stale = now + Duration::from_secs(11);
    let hit = cache
        .lookup(&q, ecs("203.0.113.0/24"), stale, true)
        .unwrap();
    assert!(hit.stale);
    assert!(
        cache
            .lookup(&q, ecs("203.0.113.0/24"), stale, false)
            .is_none()
    );
}

/// A synthetic replay of the pattern observed in production: invariant names
/// queried from many networks, geo-dependent names, and per-query rotation.
/// It shows the qualitative hit-rate gain and that no client ever receives an
/// answer differing from what its own network would have been given.
#[test]
fn synthetic_replay_gains_hits_without_serving_another_networks_answer() {
    let networks: Vec<String> = (0..24)
        .map(|i| format!("10.{}.{}.0/24", i % 6, i))
        .collect();
    // Deterministic upstream: invariant names, geo names (answer per /16), and
    // rotating names (answer changes each second).
    let upstream = |kind: usize, network: usize, second: u64| -> u8 {
        match kind {
            0 => 1,
            1 => 10 + (network % 6) as u8,
            _ => 100 + (second % 5) as u8,
        }
    };
    let run = |enabled: bool| {
        let mut config = sharing_config(3);
        config.ecs_sharing.enabled = enabled;
        let cache = Cache::new(config);
        let start = Instant::now();
        let (mut hits, mut total) = (0, 0);
        let mut state = 0x2545_f491_u64;
        for step in 0..6000u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let name_index = (state % 60) as usize;
            let kind = name_index % 3;
            let network = ((state >> 8) % networks.len() as u64) as usize;
            let second = step / 20;
            let now = start + Duration::from_secs(second);
            let q = query(&format!("r{name_index}.replay.test."));
            let outgoing = ecs(&networks[network]);
            let expected = RData::A(A::new(192, 0, 2, upstream(kind, network, second)));
            total += 1;
            if let Some(hit) = cache.lookup(&q, outgoing, now, false) {
                hits += 1;
                // A geo name is never served across /16s; rotating names may
                // serve their own network's cached answer within its TTL.
                if kind != 2 {
                    assert_eq!(hit.message.answers[0].data, expected, "{name_index}");
                }
            } else {
                let response = answer(&q, upstream(kind, network, second), 30);
                store(&cache, &q, &response, exact(&networks[network]), now);
            }
        }
        (hits as f64 / total as f64, cache.snapshot())
    };
    let (baseline, _) = run(false);
    let (learned, stats) = run(true);
    println!("synthetic replay hit rate: exact_source {baseline:.3}, learned {learned:.3}");
    assert!(learned > baseline + 0.05, "{baseline} {learned}");
    assert!(stats["ecs_shared_hits"].as_u64().unwrap() > 0);
    // Rotating names are excluded once their answers disagree.
    assert!(stats["ecs_sharing_revocations"].as_u64().unwrap() > 0);
}

#[test]
fn nested_prefixes_from_one_client_count_as_one_network() {
    let cache = Cache::new(sharing_config(3));
    let q = query("nested.test.");
    let response = answer(&q, 1, 60);
    let now = Instant::now();
    // All of these contain 10.1.2.3, so a single client could send each one.
    for network in ["10.1.2.0/24", "10.1.0.0/16", "10.0.0.0/8", "10.1.2.0/28"] {
        store(&cache, &q, &response, exact(network), now);
    }
    assert_eq!(state(&cache, &q, now), "observing");
    assert_eq!(cache.snapshot()["ecs_shared_entries"], 0);
    store(&cache, &q, &response, exact("192.0.2.0/24"), now);
    assert_eq!(state(&cache, &q, now), "observing");
    store(&cache, &q, &response, exact("198.51.100.0/24"), now);
    assert_eq!(state(&cache, &q, now), "shared");
}

#[test]
fn a_covering_prefix_replaces_every_sample_it_overlaps() {
    let cache = Cache::new(sharing_config(3));
    let q = query("covering.test.");
    let response = answer(&q, 1, 60);
    let now = Instant::now();
    store(&cache, &q, &response, exact("10.1.2.0/24"), now);
    store(&cache, &q, &response, exact("10.3.0.0/24"), now);
    // The /8 overlaps both samples, so only it remains alongside new evidence.
    store(&cache, &q, &response, exact("10.0.0.0/8"), now);
    store(&cache, &q, &response, exact("192.0.2.0/24"), now);
    assert_eq!(state(&cache, &q, now), "observing");
    store(&cache, &q, &response, exact("198.51.100.0/24"), now);
    assert_eq!(state(&cache, &q, now), "shared");
}

#[test]
fn shared_evidence_expires_and_must_be_proven_again() {
    let cache = Cache::new(sharing_config(3));
    let q = query("renew.test.");
    let response = answer(&q, 7, 60);
    let now = Instant::now();
    learn(&cache, &q, &response, now);
    assert_eq!(state(&cache, &q, now), "shared");
    // Within the evidence age, one network's matching answer keeps sharing.
    let later = now + Duration::from_secs(1800);
    store(&cache, &q, &response, exact("203.0.113.0/24"), later);
    assert_eq!(scopes(&cache, &q, later), ["learned_v4"]);
    // After an hour the same answer only starts a new observation and is stored
    // for its own network: one network refreshing first cannot keep it shared.
    let renewed = now + Duration::from_secs(3600);
    store(&cache, &q, &response, exact("203.0.113.0/24"), renewed);
    assert_eq!(state(&cache, &q, renewed), "observing");
    assert_eq!(scopes(&cache, &q, renewed), ["exact_ecs:203.0.113.0/24"]);
    learn(&cache, &q, &response, renewed);
    assert_eq!(state(&cache, &q, renewed), "shared");
}

#[test]
fn clearing_one_network_also_clears_the_learned_entry_that_serves_it() {
    let cache = Cache::new(sharing_config(3));
    let q = query("clear.test.");
    let response = answer(&q, 7, 60);
    let now = Instant::now();
    learn(&cache, &q, &response, now);
    assert_eq!(scopes(&cache, &q, now), ["learned_v4"]);
    // An IPv6 network filter does not touch the IPv4 learned entry.
    cache.invalidate(
        Some(q.queries[0].name()),
        None,
        Some(exact("2001:db8::/56")),
    );
    assert_eq!(scopes(&cache, &q, now), ["learned_v4"]);
    let removed = cache.invalidate(
        Some(q.queries[0].name()),
        None,
        Some(exact("203.0.113.0/24")),
    );
    assert_eq!(removed, 1);
    assert!(scopes(&cache, &q, now).is_empty());
    assert!(cache.peek(&q, ecs("203.0.113.0/24"), now, false).is_none());
    assert_eq!(state(&cache, &q, now), Value::Null);
}
