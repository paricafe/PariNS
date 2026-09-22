use super::*;
use hickory_proto::{
    op::{MessageType, OpCode, Query},
    rr::{
        Name, Record,
        rdata::opt::EdnsOption,
        rdata::{A, SOA},
    },
};

fn query(name: &str) -> Message {
    let mut q = Message::new(10, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    q
}

fn answer(q: &Message, address: u8, ttl: u32) -> Message {
    let mut r = protocol::error_response(q, ResponseCode::NoError);
    r.add_answer(Record::from_rdata(
        q.queries[0].name().clone(),
        ttl,
        RData::A(A::new(192, 0, 2, address)),
    ));
    r
}

fn network(text: &str) -> Scope {
    Scope::Network(text.parse().unwrap())
}
fn ecs(text: &str) -> Option<ClientSubnet> {
    Some(text.parse().unwrap())
}

#[test]
fn subnets_are_independent_and_new_overlapping_scope_supersedes_old_answer() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("Example.Test.");
    let now = Instant::now();
    cache.insert(&q, &answer(&q, 1, 60), network("192.0.0.0/16"), now);
    cache.insert(&q, &answer(&q, 2, 60), network("192.0.2.0/24"), now);
    cache.insert(&q, &answer(&q, 3, 60), network("198.51.100.0/24"), now);
    for (subnet, expected) in [("192.0.2.0/24", 2), ("198.51.100.0/24", 3)] {
        let (r, _) = cache.get(&q, ecs(subnet), now).unwrap();
        assert_eq!(r.answers[0].data, RData::A(A::new(192, 0, 2, expected)));
    }
    assert!(cache.get(&q, ecs("203.0.113.0/24"), now).is_none());
    assert!(cache.get(&q, ecs("192.0.3.0/24"), now).is_none());
    assert_eq!(cache.snapshot()["entries"], 2);
}

#[test]
fn no_ecs_family_and_privacy_namespaces_are_isolated() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("example.test.");
    let now = Instant::now();
    cache.insert(&q, &answer(&q, 1, 60), network("0.0.0.0/0"), now);
    assert!(cache.get(&q, ecs("192.0.2.0/24"), now).is_some());
    assert!(cache.get(&q, ecs("::/0"), now).is_none());
    assert!(cache.get(&q, ecs("0.0.0.0/0"), now).is_none());
    assert!(cache.get(&q, None, now).is_none());
    cache.insert(&q, &answer(&q, 2, 60), Scope::Privacy { ipv4: true }, now);
    cache.insert(&q, &answer(&q, 3, 60), Scope::NoEcs, now);
    cache.insert(&q, &answer(&q, 4, 60), network("2001:db8::/32"), now);
    for (subnet, value) in [
        (ecs("0.0.0.0/0"), 2),
        (None, 3),
        (ecs("2001:db8:1234::/56"), 4),
    ] {
        assert_eq!(
            cache.get(&q, subnet, now).unwrap().0.answers[0].data,
            RData::A(A::new(192, 0, 2, value))
        );
    }
}

#[test]
fn ttl_ages_all_sections_and_restores_current_question() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("Example.Test.");
    let now = Instant::now();
    let mut response = answer(&q, 1, 60);
    response.add_additional(Record::from_rdata(
        Name::from_ascii("extra.test.").unwrap(),
        10,
        RData::A(A::new(192, 0, 2, 3)),
    ));
    cache.insert(&q, &response, Scope::NoEcs, now);
    let mut current = query("EXAMPLE.test.");
    current.metadata.id = 200;
    let r = cache
        .get(&current, None, now + Duration::from_secs(3))
        .unwrap()
        .0;
    assert_eq!(r.id, 200);
    assert_eq!(r.queries, current.queries);
    assert_eq!((r.answers[0].ttl, r.additionals[0].ttl), (57, 7));
    assert!(cache.get(&q, None, now + Duration::from_secs(10)).is_none());
    assert_eq!(cache.snapshot()["entries"], 0);
    assert_eq!(cache.snapshot()["bytes"], 0);
}

#[test]
fn soa_controls_nxdomain_and_nodata_lifetime() {
    for code in [ResponseCode::NXDomain, ResponseCode::NoError] {
        let cache = Cache::new(CacheConfig::default());
        let q = query("example.test.");
        let now = Instant::now();
        let mut response = protocol::error_response(&q, code);
        cache.insert(&q, &response, Scope::NoEcs, now);
        assert_eq!(cache.snapshot()["entries"], 0);
        let soa = SOA::new(
            Name::from_ascii("ns.test.").unwrap(),
            Name::from_ascii("hostmaster.test.").unwrap(),
            1,
            60,
            60,
            3600,
            30,
        );
        response.add_authority(Record::from_rdata(
            Name::from_ascii("test.").unwrap(),
            120,
            RData::SOA(soa),
        ));
        cache.insert(&q, &response, Scope::NoEcs, now);
        let r = cache.get(&q, None, now + Duration::from_secs(5)).unwrap().0;
        assert_eq!(r.response_code, code);
        assert_eq!(r.authorities[0].ttl, 25);
        assert!(cache.get(&q, None, now + Duration::from_secs(30)).is_none());
        response.authorities[0].name = Name::from_ascii("other.test.").unwrap();
        cache.insert(&q, &response, Scope::NoEcs, now);
        assert_eq!(cache.snapshot()["entries"], 0);
    }
}

#[test]
fn error_zero_ttl_and_client_specific_edns_are_not_cached() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("example.test.");
    let now = Instant::now();
    for code in [ResponseCode::ServFail, ResponseCode::Refused] {
        let r = protocol::error_response(&q, code);
        cache.insert(&q, &r, Scope::NoEcs, now);
    }
    cache.insert(&q, &answer(&q, 1, 0), Scope::NoEcs, now);
    cache.insert(&q, &answer(&q, 1, u32::MAX), Scope::NoEcs, now);
    let mut q2 = q.clone();
    let mut edns = Edns::new();
    edns.options_mut()
        .insert(EdnsOption::Unknown(10, vec![1; 8])); // DNS Cookie
    q2.edns = Some(edns.clone());
    cache.insert(&q2, &answer(&q2, 1, 60), Scope::NoEcs, now);
    let mut r = answer(&q, 1, 60);
    r.edns = Some(edns);
    cache.insert(&q, &r, Scope::NoEcs, now);
    assert_eq!(cache.snapshot()["entries"], 0);
    assert_eq!(cache.snapshot()["bytes"], 0);
}

#[test]
fn semantic_flags_and_edns_presence_do_not_share_entries() {
    let cache = Cache::new(CacheConfig::default());
    let mut q = query("example.test.");
    let now = Instant::now();
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    q.metadata.checking_disabled = true;
    assert!(cache.get(&q, None, now).is_none());
    q.metadata.checking_disabled = false;
    q.metadata.recursion_desired = true;
    assert!(cache.get(&q, None, now).is_none());
    q.metadata.recursion_desired = false;
    q.edns = Some(Edns::new());
    assert!(cache.get(&q, None, now).is_none());
    cache.insert(&q, &answer(&q, 2, 60), Scope::NoEcs, now);
    q.edns.as_mut().unwrap().set_dnssec_ok(true);
    assert!(cache.get(&q, None, now).is_none());
}

#[test]
fn capacity_lru_variants_and_bytes_remain_bounded() {
    let cfg = CacheConfig {
        shards: 1,
        max_entries: 2,
        max_variants: 2,
        max_bytes: 4096,
        ..Default::default()
    };
    let cache = Cache::new(cfg);
    let now = Instant::now();
    let a = query("a.test.");
    let b = query("b.test.");
    let c = query("c.test.");
    for q in [&a, &b] {
        cache.insert(q, &answer(q, 1, 60), Scope::NoEcs, now);
    }
    cache.get(&a, None, now).unwrap();
    cache.insert(&c, &answer(&c, 1, 60), Scope::NoEcs, now);
    assert!(cache.get(&b, None, now).is_none());
    assert!(cache.get(&a, None, now).is_some());
    for prefix in ["192.0.1.0/24", "192.0.2.0/24", "192.0.3.0/24"] {
        cache.insert(&a, &answer(&a, 1, 60), network(prefix), now);
        assert!(
            cache.snapshot()["entries"].as_u64().unwrap() <= 2
                && cache.snapshot()["bytes"].as_u64().unwrap() <= 4096
        );
    }
    assert!(cache.get(&a, ecs("192.0.1.0/24"), now).is_none());
    assert!(cache.get(&a, ecs("192.0.3.0/24"), now).is_some());
    let mut large = answer(&a, 1, 60);
    large.answers = vec![large.answers[0].clone(); 500];
    cache.insert(&a, &large, Scope::NoEcs, now);
    assert!(cache.snapshot()["bytes"].as_u64().unwrap() <= 4096);
    let actual: usize = cache
        .shards
        .iter()
        .map(|s| {
            let s = s.lock().unwrap();
            s.partitions
                .iter()
                .flat_map(|p| p.lru.iter())
                .map(|(_, e)| e.charge)
                .sum::<usize>()
        })
        .sum();
    assert_eq!(cache.snapshot()["bytes"], actual);
}

fn negative_answer(q: &Message) -> Message {
    let mut response = protocol::error_response(q, ResponseCode::NXDomain);
    response.add_authority(Record::from_rdata(
        Name::from_ascii("test.").unwrap(),
        30,
        RData::SOA(SOA::new(
            Name::from_ascii("ns.test.").unwrap(),
            Name::from_ascii("hostmaster.test.").unwrap(),
            1,
            60,
            60,
            3600,
            30,
        )),
    ));
    response
}

#[test]
fn eviction_removes_one_variant_not_the_entire_domain() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 3,
        negative_percent: 0,
        ..Default::default()
    });
    let now = Instant::now();
    let a = query("a.test.");
    let b = query("b.test.");
    let c = query("c.test.");
    cache.insert(&a, &answer(&a, 1, 60), network("192.0.1.0/24"), now);
    cache.insert(&a, &answer(&a, 2, 60), network("192.0.2.0/24"), now);
    cache.insert(&b, &answer(&b, 3, 60), Scope::NoEcs, now);
    assert!(cache.get(&a, ecs("192.0.1.0/24"), now).is_some());
    cache.insert(&c, &answer(&c, 4, 60), Scope::NoEcs, now);
    assert!(cache.get(&a, ecs("192.0.1.0/24"), now).is_some());
    assert!(cache.get(&a, ecs("192.0.2.0/24"), now).is_none());
    assert!(cache.get(&b, None, now).is_some());
    assert_eq!(cache.snapshot()["evictions"], 1);
}

#[test]
fn negative_churn_cannot_evict_positive_partition() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 4,
        negative_percent: 50,
        ..Default::default()
    });
    let now = Instant::now();
    let q = query("positive.test.");
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    for i in 0..20 {
        let q = query(&format!("missing{i}.test."));
        cache.insert(&q, &negative_answer(&q), Scope::NoEcs, now);
    }
    assert!(cache.get(&q, None, now).is_some());
    assert_eq!(cache.snapshot()["positive_entries"], 1);
    assert_eq!(cache.snapshot()["negative_entries"], 2);
}

#[test]
fn stale_is_positive_only_bounded_and_never_a_normal_hit() {
    let mut cfg = CacheConfig::default();
    cfg.stale.enabled = true;
    cfg.stale.retention_secs = 40;
    cfg.stale.reply_ttl_secs = 20;
    let cache = Cache::new(cfg);
    let now = Instant::now();
    let q = query("stale.test.");
    cache.insert(&q, &answer(&q, 1, 10), Scope::NoEcs, now);
    assert!(cache.get(&q, None, now + Duration::from_secs(11)).is_none());
    let hit = cache
        .lookup(&q, None, now + Duration::from_secs(11), true)
        .unwrap();
    assert!(hit.stale);
    assert!(!hit.refresh);
    assert_eq!(hit.message.answers[0].ttl, 20);
    assert!(
        cache
            .lookup(&q, None, now + Duration::from_secs(50), true)
            .is_none()
    );
    let q = query("negative.test.");
    cache.insert(&q, &negative_answer(&q), Scope::NoEcs, now);
    assert!(
        cache
            .lookup(&q, None, now + Duration::from_secs(30), true)
            .is_none()
    );
    assert_eq!(cache.snapshot()["stale_hits"], 1);
    assert_eq!(cache.snapshot()["misses"], 1);
}

#[test]
fn short_specific_answer_does_not_resurrect_superseded_broad_scope() {
    let mut cfg = CacheConfig::default();
    cfg.stale.enabled = true;
    let cache = Cache::new(cfg);
    let now = Instant::now();
    let q = query("fresh.test.");
    cache.insert(&q, &answer(&q, 1, 60), network("192.0.0.0/16"), now);
    cache.insert(&q, &answer(&q, 2, 1), network("192.0.2.0/24"), now);
    let hit = cache
        .lookup(&q, ecs("192.0.2.0/24"), now + Duration::from_secs(2), true)
        .unwrap();
    assert!(hit.stale);
    assert_eq!(hit.scope, network("192.0.2.0/24"));
}

#[test]
fn deterministic_rule_precedence_and_label_boundaries() {
    use crate::config::CacheRule;
    let cache = Cache::new(CacheConfig {
        rules: vec![
            CacheRule {
                name: "test".into(),
                suffix: true,
                max_ttl_secs: Some(60),
                ..Default::default()
            },
            CacheRule {
                name: "example.test".into(),
                suffix: true,
                max_ttl_secs: Some(40),
                ..Default::default()
            },
            CacheRule {
                name: "example.test".into(),
                max_ttl_secs: Some(30),
                ..Default::default()
            },
            CacheRule {
                name: "example.test".into(),
                qtype: Some("A".into()),
                max_ttl_secs: Some(20),
                ..Default::default()
            },
            CacheRule {
                name: "example.test".into(),
                qtype: Some("A".into()),
                max_ttl_secs: Some(10),
                ..Default::default()
            },
            CacheRule {
                name: "skip.test".into(),
                bypass: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let now = Instant::now();
    for (name, ttl) in [
        ("example.test.", 20),
        ("sub.example.test.", 40),
        ("notexample.test.", 60),
    ] {
        let q = query(name);
        cache.insert(&q, &answer(&q, 1, 300), Scope::NoEcs, now);
        assert_eq!(cache.get(&q, None, now).unwrap().0.answers[0].ttl, ttl);
    }
    let q = query("skip.test.");
    cache.insert(&q, &answer(&q, 1, 300), Scope::NoEcs, now);
    assert!(cache.get(&q, None, now).is_none());
    assert_eq!(cache.explain(&q, None, now)["reason"], "rule_bypass");
}

#[test]
fn targeted_invalidation_blocks_old_epoch_and_keeps_unrelated_scopes() {
    let cache = Cache::new(CacheConfig::default());
    let now = Instant::now();
    let q = query("clear.test.");
    let epoch = cache.epoch();
    cache.insert(&q, &answer(&q, 1, 60), network("192.0.2.0/24"), now);
    cache.insert(&q, &answer(&q, 2, 60), Scope::NoEcs, now);
    assert_eq!(
        cache.invalidate(Some("CLEAR.test"), Some(RecordType::A), Some(Scope::NoEcs)),
        1
    );
    assert!(!cache.insert_if_epoch(&q, &answer(&q, 2, 60), Scope::NoEcs, now, epoch));
    assert!(cache.get(&q, None, now).is_none());
    assert!(cache.get(&q, ecs("192.0.2.0/24"), now).is_some());
    assert!(cache.insert_if_epoch(&q, &answer(&q, 2, 60), Scope::NoEcs, now, cache.epoch()));
}

#[test]
fn outstanding_readers_remain_charged_after_invalidation() {
    let cache = Cache::new(CacheConfig::default());
    let now = Instant::now();
    let q = query("held.test.");
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    let key = Key::of(&q).unwrap();
    let held = {
        let shard = cache.shards[cache.shard(&key)].lock().unwrap();
        Arc::clone(&shard.buckets.get(&key).unwrap()[0])
    };
    let charge = held.charge;
    cache.invalidate(None, None, None);
    assert_eq!(cache.snapshot()["entries"], 0);
    assert_eq!(cache.snapshot()["bytes"], charge);
    drop(held);
    assert_eq!(cache.snapshot()["bytes"], 0);
}

#[test]
fn inspection_and_race_checks_do_not_make_an_entry_popular() {
    let mut cfg = CacheConfig::default();
    cfg.prefetch.enabled = true;
    let cache = Cache::new(cfg);
    let now = Instant::now();
    let q = query("prefetch.test.");
    cache.insert(&q, &answer(&q, 1, 100), Scope::NoEcs, now);
    let later = now + Duration::from_secs(95);
    for _ in 0..5 {
        assert!(!cache.peek(&q, None, later, false).unwrap().refresh);
        assert_eq!(cache.explain(&q, None, later)["state"], "fresh");
        assert_eq!(
            cache.inspect("PREFETCH.test", None, later)["variants"][0]["hits"],
            0
        );
    }
    assert_eq!(cache.snapshot()["hits"], 0);
    assert!(!cache.lookup(&q, None, later, false).unwrap().refresh);
    assert!(!cache.lookup(&q, None, later, false).unwrap().refresh);
    assert!(cache.lookup(&q, None, later, false).unwrap().refresh);
}

#[test]
fn smallest_valid_cache_still_admits_a_small_positive_answer() {
    let cache = Cache::new(CacheConfig {
        max_entries: 1,
        max_variants: 1,
        max_bytes: 512,
        ..Default::default()
    });
    let now = Instant::now();
    let q = query("a.test.");
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    assert!(cache.get(&q, None, now).is_some());
    assert_eq!(cache.snapshot()["shards"], 1);
}

#[test]
fn variant_ceiling_cannot_cross_partition_isolation() {
    let cache = Cache::new(CacheConfig {
        max_variants: 1,
        ..Default::default()
    });
    let now = Instant::now();
    let q = query("variant.test.");
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    assert!(!cache.insert_if_epoch(
        &q,
        &negative_answer(&q),
        network("192.0.2.0/24"),
        now,
        cache.epoch()
    ));
    assert!(cache.get(&q, None, now).is_some());
}

#[test]
fn cache_rules_use_dns_labels_and_equivalent_escaped_names() {
    use crate::config::CacheRule;
    let cache = Cache::new(CacheConfig {
        rules: vec![
            CacheRule {
                name: "example.test".into(),
                suffix: true,
                bypass: true,
                ..Default::default()
            },
            CacheRule {
                name: r"\145xample.test".into(),
                max_ttl_secs: Some(17),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let now = Instant::now();
    let escaped_label = query(r"foo\.example.test.");
    assert_eq!(cache.explain(&escaped_label, None, now)["eligible"], true);
    let actual_child = query("foo.example.test.");
    assert_eq!(cache.explain(&actual_child, None, now)["eligible"], false);
    let exact = query("example.test.");
    cache.insert(&exact, &answer(&exact, 1, 300), Scope::NoEcs, now);
    assert_eq!(cache.get(&exact, None, now).unwrap().0.answers[0].ttl, 17);
    assert_eq!(
        cache.inspect(r"\145xample.test", None, now)["variants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(cache.invalidate(Some(r"\145xample.test"), None, None), 1);
    let literal_star = query("*.example.test.");
    assert_eq!(cache.explain(&literal_star, None, now)["eligible"], false);
}

#[test]
fn uncacheable_success_supersedes_old_fresh_and_stale_answers() {
    for ttl in [1, 60] {
        for replacement in 0..3 {
            let mut cfg = CacheConfig {
                shards: 1,
                max_bytes: 4096,
                ..Default::default()
            };
            cfg.stale.enabled = true;
            let cache = Cache::new(cfg);
            let q = query("superseded.test.");
            let now = Instant::now();
            cache.insert(&q, &answer(&q, 1, ttl), Scope::NoEcs, now);
            let later = now + Duration::from_secs(2);
            assert!(cache.lookup(&q, None, later, true).is_some());
            let response = match replacement {
                0 => answer(&q, 2, 0),
                1 => protocol::error_response(&q, ResponseCode::NXDomain),
                _ => {
                    let mut response = answer(&q, 2, 60);
                    response.answers = vec![response.answers[0].clone(); 500];
                    response
                }
            };
            assert!(!cache.insert_if_epoch(&q, &response, Scope::NoEcs, later, cache.epoch()));
            assert!(
                cache.lookup(&q, None, later, true).is_none(),
                "ttl={ttl}, replacement={replacement}"
            );
            assert_eq!(cache.snapshot()["entries"], 0);
        }
    }
}

#[test]
fn success_supersedes_overlaps_but_keeps_disjoint_and_private_namespaces() {
    for (old, new) in [
        ("192.0.0.0/16", "192.0.2.0/24"),
        ("192.0.2.0/24", "192.0.0.0/16"),
    ] {
        let mut cfg = CacheConfig::default();
        cfg.stale.enabled = true;
        let cache = Cache::new(cfg);
        let q = query("overlap.test.");
        let now = Instant::now();
        for scope in [
            network(old),
            network("198.51.100.0/24"),
            network("2001:db8::/32"),
            Scope::NoEcs,
            Scope::Privacy { ipv4: true },
            Scope::Privacy { ipv4: false },
        ] {
            cache.insert(&q, &answer(&q, 1, 1), scope, now);
        }
        let later = now + Duration::from_secs(2);
        assert!(!cache.insert_if_epoch(&q, &answer(&q, 2, 0), network(new), later, cache.epoch()));
        assert!(cache.lookup(&q, ecs("192.0.2.0/24"), later, true).is_none());
        for subnet in [
            ecs("198.51.100.0/24"),
            ecs("2001:db8:1::/56"),
            None,
            ecs("0.0.0.0/0"),
            ecs("::/0"),
        ] {
            assert!(cache.lookup(&q, subnet, later, true).is_some());
        }
        assert_eq!(cache.snapshot()["entries"], 5);
    }
}

#[test]
fn failed_truncated_or_client_specific_updates_do_not_supersede() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("retain.test.");
    let now = Instant::now();
    cache.insert(&q, &answer(&q, 1, 60), Scope::NoEcs, now);
    let mut truncated = answer(&q, 2, 0);
    truncated.metadata.truncation = true;
    let mut client_specific = answer(&q, 2, 0);
    let mut edns = Edns::new();
    edns.options_mut()
        .insert(EdnsOption::Unknown(10, vec![1; 8]));
    client_specific.edns = Some(edns);
    for response in [
        protocol::error_response(&q, ResponseCode::ServFail),
        protocol::error_response(&q, ResponseCode::Refused),
        truncated,
        client_specific,
    ] {
        assert!(!cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
        assert_eq!(
            cache.get(&q, None, now).unwrap().0.answers[0].data,
            RData::A(A::new(192, 0, 2, 1))
        );
    }
    let old_epoch = cache.epoch();
    cache.invalidate(Some("unrelated.test"), None, None);
    assert!(!cache.insert_if_epoch(&q, &answer(&q, 2, 0), Scope::NoEcs, now, old_epoch));
    assert!(cache.get(&q, None, now).is_some());
}
