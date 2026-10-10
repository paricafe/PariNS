use super::*;
mod reliability;
use hickory_proto::{
    op::{MessageType, OpCode, Query},
    rr::{
        Name, Record,
        rdata::opt::EdnsOption,
        rdata::{A, AAAA, CNAME, NS, NULL, SOA},
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

fn cname(owner: &str, target: &str, ttl: u32) -> Record {
    Record::from_rdata(
        Name::from_ascii(owner).unwrap(),
        ttl,
        RData::CNAME(CNAME(Name::from_ascii(target).unwrap())),
    )
}

fn cname_negative_answer(q: &Message, code: ResponseCode) -> Message {
    let mut response = negative_answer(q);
    response.metadata.response_code = code;
    response.add_answer(cname(
        &q.queries[0].name().to_ascii(),
        "missing.target.test.",
        60,
    ));
    response.authorities[0].name = Name::from_ascii("target.test.").unwrap();
    response
}

#[test]
fn cname_negative_proves_unordered_chains_using_dns_names() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        for (name, links, zone) in [
            (
                "alias.target.test.",
                vec![("alias.target.test.", "missing.target.test.")],
                "target.test.",
            ),
            (
                "alias.source.test.",
                vec![("alias.source.test.", "missing.target.test.")],
                "target.test.",
            ),
            (
                "alias.source.test.",
                vec![
                    ("alias.source.test.", "middle.source.test."),
                    ("middle.source.test.", "missing.target.test."),
                ],
                "target.test.",
            ),
            (
                "alias.source.test.",
                vec![
                    ("middle.source.test.", "missing.target.test."),
                    ("alias.source.test.", "middle.source.test."),
                ],
                "target.test.",
            ),
            (
                "Alias.Source.Test.",
                vec![
                    ("ALIAS.source.test.", r"\155iddle.Target.Test."),
                    ("middle.target.test.", "MISSING.Target.Test."),
                ],
                "TARGET.TEST.",
            ),
            (
                r"alias\.source.test.",
                vec![(r"alias\.source.test.", r"missing\.target.test.")],
                "test.",
            ),
        ] {
            let cache = Cache::new(CacheConfig::default());
            let q = query(name);
            let now = Instant::now();
            let mut response = cname_negative_answer(&q, code);
            response.answers = links
                .iter()
                .map(|(owner, target)| cname(owner, target, 60))
                .collect();
            response.authorities[0].name = Name::from_ascii(zone).unwrap();
            let decision =
                cache.insert_decision_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch());
            assert!(
                decision.admitted(),
                "{code:?}: {name}, {links:?}: {decision:?}"
            );
            let hit = cache.get(&q, None, now).unwrap().0;
            assert_eq!(hit.response_code, code);
            assert_eq!(hit.answers, response.answers);
            assert_eq!(cache.snapshot()["negative_entries"], 1);
            assert_eq!(cache.snapshot()["positive_entries"], 0);
        }
    }
}

#[test]
fn cname_negative_allows_duplicate_links_and_soa_with_other_sections() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        let cache = Cache::new(CacheConfig::default());
        let q = query("alias.source.test.");
        let now = Instant::now();
        let mut response = cname_negative_answer(&q, code);
        response.metadata.authoritative = false;
        response.add_answer(cname("ALIAS.source.test.", "MISSING.target.test.", 20));
        let mut duplicate = response.authorities[0].clone();
        duplicate.ttl = 18;
        response.add_authority(duplicate);
        response.add_authority(Record::from_rdata(
            Name::from_ascii("target.test.").unwrap(),
            25,
            RData::NS(NS(Name::from_ascii("ns.target.test.").unwrap())),
        ));
        for code in [RecordType::RRSIG, RecordType::NSEC] {
            response.add_authority(Record::from_rdata(
                Name::from_ascii("target.test.").unwrap(),
                24,
                RData::Unknown {
                    code,
                    rdata: NULL::with(vec![1, 2, 3]),
                },
            ));
        }
        response.add_additional(Record::from_rdata(
            Name::from_ascii("ns.target.test.").unwrap(),
            22,
            RData::A(A::new(192, 0, 2, 1)),
        ));
        assert!(cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
        let hit = cache.get(&q, None, now + Duration::from_secs(5)).unwrap().0;
        assert!(!hit.authoritative);
        assert_eq!(hit.answers.len(), 2);
        assert_eq!((hit.answers[0].ttl, hit.answers[1].ttl), (55, 15));
        assert_eq!(hit.authorities.len(), 5);
        assert_eq!((hit.authorities[0].ttl, hit.authorities[1].ttl), (13, 13));
        assert_eq!(hit.authorities[2].ttl, 20);
        assert_eq!(hit.authorities[3].ttl, 19);
        assert_eq!(hit.authorities[4].ttl, 19);
        assert_eq!(hit.additionals[0].ttl, 17);
        assert!(cache.get(&q, None, now + Duration::from_secs(18)).is_none());
    }
}

#[test]
fn cname_negative_rejects_unproved_chain_or_terminal_soa() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        for case in [
            "no_soa",
            "referral",
            "alias_soa",
            "escaped_soa_boundary",
            "wrong_soa_class",
            "conflicting_soa_data",
            "conflicting_soa_owner",
            "unrelated_soa",
            "wrong_cname_class",
            "fork",
            "cycle",
            "self_cycle",
            "broken_chain",
            "unrelated_cname",
            "dname",
            "rrsig",
            "other_type",
        ] {
            let cache = Cache::new(CacheConfig::default());
            let q = query("alias.source.test.");
            let now = Instant::now();
            let mut response = cname_negative_answer(&q, code);
            match case {
                "no_soa" => response.authorities.clear(),
                "referral" => {
                    response.authorities[0].data =
                        RData::NS(NS(Name::from_ascii("ns.target.test.").unwrap()));
                }
                "alias_soa" => {
                    response.authorities[0].name = Name::from_ascii("source.test.").unwrap();
                }
                "escaped_soa_boundary" => {
                    response.answers[0] = cname("alias.source.test.", r"missing\.target.test.", 60);
                }
                "wrong_soa_class" => response.authorities[0].dns_class = DNSClass::CH,
                "conflicting_soa_data" | "conflicting_soa_owner" | "unrelated_soa" => {
                    let mut other = response.authorities[0].clone();
                    match case {
                        "conflicting_soa_data" => {
                            let RData::SOA(soa) = &mut other.data else {
                                unreachable!()
                            };
                            soa.serial += 1;
                        }
                        "conflicting_soa_owner" => {
                            other.name = Name::from_ascii("test.").unwrap();
                        }
                        _ => other.name = Name::from_ascii("source.test.").unwrap(),
                    }
                    response.add_authority(other);
                }
                "wrong_cname_class" => response.answers[0].dns_class = DNSClass::CH,
                "fork" => {
                    response.add_answer(cname("alias.source.test.", "other.target.test.", 60));
                }
                "cycle" => {
                    response.add_answer(cname("missing.target.test.", "alias.source.test.", 60));
                }
                "self_cycle" => {
                    response.answers[0] = cname("alias.source.test.", "alias.source.test.", 60);
                }
                "broken_chain" => {
                    response.answers[0].name = Name::from_ascii("middle.source.test.").unwrap();
                }
                "unrelated_cname" => {
                    response.add_answer(cname("other.source.test.", "other.target.test.", 60));
                }
                "dname" | "rrsig" => {
                    response.add_answer(Record::from_rdata(
                        q.queries[0].name().clone(),
                        60,
                        RData::Unknown {
                            code: if case == "dname" {
                                RecordType::DNAME
                            } else {
                                RecordType::RRSIG
                            },
                            rdata: NULL::with(vec![1]),
                        },
                    ));
                }
                "other_type" => {
                    response.add_answer(Record::from_rdata(
                        Name::from_ascii("missing.target.test.").unwrap(),
                        60,
                        RData::AAAA(AAAA("2001:db8::1".parse().unwrap())),
                    ));
                }
                _ => unreachable!(),
            }
            assert!(
                !cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()),
                "{code:?}: {case}"
            );
            assert!(cache.get(&q, None, now).is_none());
        }
    }
}

#[test]
fn cname_direct_and_terminal_addresses_remain_positive() {
    for kind in [RecordType::CNAME, RecordType::A, RecordType::AAAA] {
        let cache = Cache::new(CacheConfig::default());
        let mut q = query("alias.source.test.");
        q.queries[0].set_query_type(kind);
        let now = Instant::now();
        let mut response = cname_negative_answer(&q, ResponseCode::NoError);
        response.authorities.clear();
        if kind != RecordType::CNAME {
            response.add_answer(Record::from_rdata(
                Name::from_ascii("missing.target.test.").unwrap(),
                60,
                if kind == RecordType::A {
                    RData::A(A::new(192, 0, 2, 1))
                } else {
                    RData::AAAA(AAAA("2001:db8::1".parse().unwrap()))
                },
            ));
        }
        assert!(cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
        assert_eq!(cache.snapshot()["positive_entries"], 1);
        assert_eq!(cache.snapshot()["negative_entries"], 0);
        response.metadata.response_code = ResponseCode::NXDomain;
        response.authorities = cname_negative_answer(&q, ResponseCode::NXDomain).authorities;
        assert!(!cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
        assert!(cache.get(&q, None, now).is_none());
    }
}

#[test]
fn cname_negative_lifetime_obeys_every_existing_ttl_bound() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        for bound in [
            "cname",
            "soa",
            "minimum",
            "negative_cap",
            "max_ttl",
            "additional",
        ] {
            let mut cfg = CacheConfig {
                max_ttl_secs: 100,
                negative_ttl_cap_secs: 100,
                ..Default::default()
            };
            let q = query("alias.source.test.");
            let now = Instant::now();
            let mut response = cname_negative_answer(&q, code);
            response.answers[0].ttl = 100;
            response.authorities[0].ttl = 100;
            let RData::SOA(soa) = &mut response.authorities[0].data else {
                unreachable!()
            };
            soa.minimum = if bound == "minimum" { 7 } else { 100 };
            response.add_additional(Record::from_rdata(
                Name::from_ascii("ns.target.test.").unwrap(),
                100,
                RData::A(A::new(192, 0, 2, 1)),
            ));
            match bound {
                "cname" => response.answers[0].ttl = 7,
                "soa" => response.authorities[0].ttl = 7,
                "negative_cap" => cfg.negative_ttl_cap_secs = 7,
                "max_ttl" => cfg.max_ttl_secs = 7,
                "additional" => response.additionals[0].ttl = 7,
                _ => {}
            }
            let cache = Cache::new(cfg);
            assert!(cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
            assert_eq!(
                cache.inspect("alias.source.test.", None, now)["variants"][0]["fresh_remaining_secs"],
                7,
                "{code:?}: {bound}"
            );
            let hit = cache.get(&q, None, now + Duration::from_secs(5)).unwrap().0;
            assert_eq!(
                hit.answers
                    .iter()
                    .chain(&hit.authorities)
                    .chain(&hit.additionals)
                    .map(|rr| rr.ttl)
                    .min(),
                Some(2),
                "{code:?}: {bound}"
            );
            assert!(cache.get(&q, None, now + Duration::from_secs(7)).is_none());
        }
        for ttl in [0, i32::MAX as u32 + 1] {
            for section in 0..3 {
                let cache = Cache::new(CacheConfig::default());
                let q = query("alias.source.test.");
                let now = Instant::now();
                let mut response = cname_negative_answer(&q, code);
                response.add_additional(Record::from_rdata(
                    Name::from_ascii("ns.target.test.").unwrap(),
                    30,
                    RData::A(A::new(192, 0, 2, 1)),
                ));
                match section {
                    0 => response.answers[0].ttl = ttl,
                    1 => response.authorities[0].ttl = ttl,
                    _ => response.additionals[0].ttl = ttl,
                }
                assert_eq!(
                    cache.insert_decision_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()),
                    StoreDecision::skipped(DecisionReason::TtlZero),
                    "{code:?}: section {section}, ttl {ttl}"
                );
            }
        }
    }
}

#[test]
fn cname_negative_exact_source_does_not_share_namespaces_or_question_types() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        let cache = Cache::new(CacheConfig::default());
        let q = query("alias.source.test.");
        let now = Instant::now();
        cache.insert(
            &q,
            &cname_negative_answer(&q, code),
            Scope::ExactSource("192.0.2.0/24".parse().unwrap()),
            now,
        );
        assert!(cache.get(&q, ecs("192.0.2.99/24"), now).is_some());
        for outgoing in [
            ecs("192.0.3.0/24"),
            ecs("192.0.2.0/25"),
            ecs("2001:db8::/32"),
            ecs("0.0.0.0/0"),
            ecs("::/0"),
            None,
        ] {
            assert!(cache.get(&q, outgoing, now).is_none());
        }
        for kind in [RecordType::AAAA, RecordType::CNAME] {
            let mut other = q.clone();
            other.queries[0].set_query_type(kind);
            assert!(cache.get(&other, ecs("192.0.2.0/24"), now).is_none());
        }
        assert!(
            cache
                .get(&query("missing.target.test."), ecs("192.0.2.0/24"), now)
                .is_none()
        );
    }
}

#[test]
fn cname_negative_rejection_supersedes_old_positive_and_negative_but_not_old_epoch() {
    for old_negative in [false, true] {
        for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
            for cause in [DecisionReason::TtlZero, DecisionReason::Capacity] {
                let mut cfg = CacheConfig {
                    shards: 1,
                    max_bytes: 4096,
                    negative_percent: 50,
                    ..Default::default()
                };
                cfg.stale.enabled = true;
                let cache = Cache::new(cfg);
                let q = query("alias.source.test.");
                let now = Instant::now();
                let old = if old_negative {
                    negative_answer(&q)
                } else {
                    answer(&q, 1, 60)
                };
                cache.insert(&q, &old, network("192.0.0.0/16"), now);
                let mut replacement = cname_negative_answer(&q, code);
                if cause == DecisionReason::TtlZero {
                    replacement.answers[0].ttl = 0;
                } else {
                    replacement.answers = vec![replacement.answers[0].clone(); 500];
                }
                let old_epoch = cache.epoch();
                cache.invalidate(Some("unrelated.test."), None, None);
                let new_scope = Scope::ExactSource("192.0.2.0/24".parse().unwrap());
                assert_eq!(
                    cache.insert_decision_if_epoch(&q, &replacement, new_scope, now, old_epoch),
                    StoreDecision::skipped(DecisionReason::EpochChanged)
                );
                assert_eq!(
                    cache.get(&q, ecs("192.0.2.0/24"), now).unwrap().0.answers,
                    old.answers
                );
                assert_eq!(
                    cache.insert_decision_if_epoch(&q, &replacement, new_scope, now, cache.epoch()),
                    StoreDecision::rejected(cause, true),
                    "old_negative={old_negative}, code={code:?}, cause={cause:?}"
                );
                for outgoing in [ecs("192.0.2.0/24"), ecs("192.0.3.0/24")] {
                    assert!(cache.lookup(&q, outgoing, now, true).is_none());
                    assert!(
                        cache
                            .lookup(&q, outgoing, now + Duration::from_secs(60), true)
                            .is_none()
                    );
                }
            }
        }
    }
}

#[test]
fn empty_negative_keeps_existing_soa_selection() {
    let q = query("alias.source.test.");
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        let cache = Cache::new(CacheConfig::default());
        let now = Instant::now();
        let mut response = negative_answer(&q);
        response.metadata.response_code = code;
        let mut unrelated = response.authorities[0].clone();
        unrelated.name = Name::from_ascii("unrelated.test.").unwrap();
        response.add_authority(unrelated);
        assert!(cache.insert_if_epoch(&q, &response, Scope::NoEcs, now, cache.epoch()));
        assert_eq!(cache.get(&q, None, now).unwrap().0.authorities.len(), 2);
    }
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
        let response = match i % 3 {
            0 => negative_answer(&q),
            1 => cname_negative_answer(&q, ResponseCode::NoError),
            _ => cname_negative_answer(&q, ResponseCode::NXDomain),
        };
        cache.insert(&q, &response, Scope::NoEcs, now);
        assert!(cache.get(&q, None, now).is_some());
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
    cfg.prefetch.enabled = true;
    cfg.prefetch.min_hits = 1;
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
    for response in [
        negative_answer(&q),
        cname_negative_answer(&q, ResponseCode::NoError),
        cname_negative_answer(&q, ResponseCode::NXDomain),
    ] {
        cache.insert(&q, &response, Scope::NoEcs, now);
        let hit = cache
            .lookup(&q, None, now + Duration::from_secs(29), false)
            .unwrap();
        assert!(!hit.refresh);
        assert!(!hit.stale);
        assert!(
            cache
                .lookup(&q, None, now + Duration::from_secs(30), true)
                .is_none()
        );
    }
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

fn with_ede(mut response: Message) -> Message {
    response
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(15, vec![0, 0]));
    response
}

#[test]
fn ede_success_supersedes_fresh_and_stale_without_admitting_diagnostics() {
    for ttl in [1, 60] {
        let mut cfg = CacheConfig::default();
        cfg.stale.enabled = true;
        let cache = Cache::new(cfg);
        let mut q = query("ede.test.");
        q.edns = Some(Edns::new());
        let now = Instant::now();
        let later = now + Duration::from_secs(2);
        let mut negative = protocol::error_response(&q, ResponseCode::NXDomain);
        negative.add_authority(Record::from_rdata(
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
        for response in [answer(&q, 2, 0), answer(&q, 2, 60), negative] {
            cache.insert(&q, &answer(&q, 1, ttl), Scope::NoEcs, now);
            assert!(cache.lookup(&q, None, later, true).is_some());
            assert!(!cache.insert_if_epoch(
                &q,
                &with_ede(response),
                Scope::NoEcs,
                later,
                cache.epoch(),
            ));
            assert!(
                cache.lookup(&q, None, later, true).is_none(),
                "old ttl={ttl}"
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
fn ede_supersession_keeps_disjoint_scopes_privacy_and_query_semantics() {
    let mut cfg = CacheConfig::default();
    cfg.stale.enabled = true;
    let cache = Cache::new(cfg);
    let mut q = query("ede-scope.test.");
    q.edns = Some(Edns::new());
    let mut other_query = q.clone();
    other_query.metadata.checking_disabled = true;
    let now = Instant::now();
    for scope in [
        network("192.0.0.0/16"),
        network("198.51.100.0/24"),
        network("2001:db8::/32"),
        Scope::NoEcs,
        Scope::Privacy { ipv4: true },
        Scope::Privacy { ipv4: false },
    ] {
        cache.insert(&q, &answer(&q, 1, 1), scope, now);
    }
    cache.insert(
        &other_query,
        &answer(&other_query, 1, 1),
        network("192.0.0.0/16"),
        now,
    );
    let later = now + Duration::from_secs(2);
    let mut response = with_ede(answer(&q, 2, 0));
    response
        .edns
        .as_mut()
        .unwrap()
        .options_mut()
        .insert(EdnsOption::Subnet("192.0.2.0/24".parse().unwrap()));
    assert!(!cache.insert_if_epoch(&q, &response, network("192.0.2.0/24"), later, cache.epoch()));
    assert!(cache.lookup(&q, ecs("192.0.2.0/24"), later, true).is_none());
    assert!(
        cache
            .lookup(&other_query, ecs("192.0.2.0/24"), later, true)
            .is_some()
    );
    for subnet in [
        ecs("198.51.100.0/24"),
        ecs("2001:db8:1::/56"),
        None,
        ecs("0.0.0.0/0"),
        ecs("::/0"),
    ] {
        assert!(cache.lookup(&q, subnet, later, true).is_some());
    }
    assert_eq!(cache.snapshot()["entries"], 6);
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
    let mut unknown_option = with_ede(answer(&q, 2, 0));
    unknown_option
        .edns
        .as_mut()
        .unwrap()
        .options_mut()
        .insert(EdnsOption::Unknown(65001, vec![1]));
    for response in [
        protocol::error_response(&q, ResponseCode::ServFail),
        protocol::error_response(&q, ResponseCode::Refused),
        with_ede(protocol::error_response(&q, ResponseCode::ServFail)),
        with_ede(truncated),
        with_ede(client_specific),
        unknown_option,
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
    assert!(!cache.insert_if_epoch(
        &q,
        &with_ede(answer(&q, 2, 0)),
        Scope::NoEcs,
        now,
        old_epoch
    ));
    assert!(cache.get(&q, None, now).is_some());
}
