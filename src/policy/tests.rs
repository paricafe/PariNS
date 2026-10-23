use super::*;
use hickory_proto::{
    op::{MessageType, OpCode, Query, ResponseCode},
    rr::{DNSClass, Record, RecordType, rdata::CNAME},
};

fn query() -> Message {
    let mut q = Message::new(7, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(
        Name::from_ascii("alias.test").unwrap(),
        RecordType::A,
    ));
    q
}

fn cname(owner: &str, target: &str) -> Record {
    Record::from_rdata(
        Name::from_ascii(owner).unwrap(),
        60,
        RData::CNAME(CNAME(Name::from_ascii(target).unwrap())),
    )
}

fn policy(rules: &str) -> Policy {
    toml::from_str(&format!("enabled = true\n{rules}")).unwrap()
}

fn blocked(policy: &Policy, name: &str) -> bool {
    policy.blocks(&Name::from_ascii(name).unwrap())
}

#[test]
fn exact_and_suffix_respect_label_boundaries_and_case() {
    let p = policy("block_exact = ['Exact.Test.']\nblock_suffix = ['Ads.Test']");
    for name in ["exact.test", "EXACT.TEST.", "ads.test", "a.b.ads.test."] {
        assert!(blocked(&p, name), "{name}");
    }
    for name in ["sub.exact.test", "badads.test", "ads.test.other", "."] {
        assert!(!blocked(&p, name), "{name}");
    }
}

#[test]
fn any_matching_allow_wins_for_that_name() {
    let p = policy(
        "block_suffix = ['test']\nblock_exact = ['blocked.safe.test']\nallow_exact = ['only.test']\nallow_suffix = ['safe.test']",
    );
    for name in [
        "only.test",
        "safe.test",
        "blocked.safe.test",
        "other.safe.test",
    ] {
        assert!(!blocked(&p, name), "{name}");
    }
    for name in ["sub.only.test", "other.test"] {
        assert!(blocked(&p, name), "{name}");
    }
}

#[test]
fn disabled_policy_is_noop_but_still_validates_rules() {
    let p: Policy = toml::from_str("block_suffix = ['test']").unwrap();
    assert!(!blocked(&p, "ads.test"));
    assert!(toml::from_str::<Policy>("block_exact = ['*.test']").is_err());
    assert!(!blocked(&Policy::default(), "anything.test"));
}

#[test]
fn rejects_unsupported_syntax_and_invalid_dns_lengths() {
    for rule in [
        "",
        ".",
        "a..test",
        "*.test",
        "||ads.test^",
        "https://ads.test",
        "0.0.0.0 ads.test",
        "a\\.test",
        "é.test",
        " ads.test",
        "ads.test..",
    ] {
        let rules = Rules {
            block_exact: vec![rule.into()],
            ..Rules::default()
        };
        assert!(Policy::try_from(rules).is_err(), "{rule}");
    }
    for rule in [
        format!("{}.test", "a".repeat(64)),
        vec!["a".repeat(63); 4].join("."),
    ] {
        assert!(
            Policy::try_from(Rules {
                block_exact: vec![rule],
                ..Rules::default()
            })
            .is_err()
        );
    }
    assert!(toml::from_str::<Policy>("block_regex = []").is_err());
    let p = policy("block_exact = ['xn--bcher-kva.test', '_service._tcp.test']");
    assert!(blocked(&p, "XN--BCHER-KVA.test"));
    assert!(blocked(&p, "_service._tcp.test"));
}

#[test]
fn rule_count_and_text_budgets_fail_before_compilation() {
    assert!(
        Policy::try_from(Rules {
            block_exact: vec!["a.test".into(); 100_001],
            ..Rules::default()
        })
        .is_err()
    );
    assert!(
        Policy::try_from(Rules {
            block_exact: vec!["a".repeat(100); 84_000],
            ..Rules::default()
        })
        .is_err()
    );
}

#[test]
fn cname_chain_is_order_independent_and_allow_does_not_exempt_other_targets() {
    let p = policy("block_suffix = ['test']\nallow_exact = ['alias.test', 'middle.test']");
    let q = query();
    let mut reply = crate::protocol::error_response(&q, ResponseCode::NoError);
    reply.answers = vec![
        cname("MIDDLE.test", "ads.test"),
        cname("alias.TEST", "middle.test"),
    ];
    reply.authorities.push(cname("test", "ads.test"));
    reply.additionals.push(cname("test", "ads.test"));
    reply.metadata.authentic_data = true;
    reply.metadata.authoritative = true;
    reply.metadata.truncation = true;
    assert_eq!(
        p.apply_response(&q, &mut reply).map(|block| block.rule),
        Some("test".into())
    );
    // The default method answers NODATA with the blocking rule's SOA.
    assert!(reply.answers.is_empty() && reply.additionals.is_empty());
    assert_eq!(reply.authorities.len(), 1);
    assert_eq!(reply.authorities[0].record_type(), RecordType::SOA);
    assert!(!reply.authentic_data && !reply.authoritative && !reply.truncation);
    assert_eq!(reply.queries, q.queries);
}

#[test]
fn unrelated_names_wrong_class_and_cycles_do_not_cause_false_blocks() {
    let p = policy("block_suffix = ['ads.test']");
    let q = query();
    let mut reply = crate::protocol::error_response(&q, ResponseCode::NoError);
    let mut wrong_class = cname("alias.test", "ads.test");
    wrong_class.dns_class = DNSClass::CH;
    reply.answers = vec![
        cname("alias.test", "loop.test"),
        cname("loop.test", "alias.test"),
        cname("unrelated.test", "ads.test"),
        wrong_class,
    ];
    reply.additionals.push(cname("alias.test", "ads.test"));
    let before = reply.to_vec().unwrap();
    p.apply_response(&q, &mut reply);
    assert_eq!(reply.to_vec().unwrap(), before);
    // An additional branch to a blocked target must still be checked, even in a loop.
    reply.answers.push(cname("loop.test", "ads.test"));
    p.apply_response(&q, &mut reply);
    assert!(reply.answers.is_empty());
}

#[test]
fn local_inputs_are_retained_and_charged_without_changing_effective_digest() {
    let local = policy(
        "block_suffix=['test','TEST.']\nblock_exact=['ads.test']\nallow_exact=['safe.test']",
    );
    let effective = policy("block_suffix=['test']\nallow_exact=['safe.test']");
    assert_eq!(local.semantic_digest(), effective.semantic_digest());
    assert_eq!(local.publication_digest(), effective.publication_digest());
    assert_eq!(local.input_rules(), 4);
    assert_eq!(local.index_rules(), 2);
    assert_eq!(local.input_bytes(), 26);
    assert!(local.local_bytes() >= 26 + 4 * std::mem::size_of::<String>());
    assert!(local.owned_bytes() > local.local_bytes() + local.index_bytes());
    assert!(Arc::ptr_eq(&local.data, &local.clone().data));
    let mut builder = Builder::new(Limits::default()).unwrap();
    local.append_to(&mut builder).unwrap();
    assert_eq!(builder.prepare().unwrap().input_rules, 4);
    let disabled: Policy = toml::from_str("block_suffix=['test']").unwrap();
    assert_eq!(disabled.input_rules(), 0);
    assert_eq!(disabled.input_bytes(), 0);
    assert!(disabled.local_bytes() > 0);
    assert_eq!(
        disabled.semantic_digest(),
        Policy::default().semantic_digest()
    );
}

#[test]
fn aggregate_witnesses_are_bounded_and_local_allow_wins() {
    let local = policy("allow_exact=['safe.test']");
    let mut builder = Builder::new(Limits::default()).unwrap();
    local.append_to(&mut builder).unwrap();
    builder.add("test", Group::BlockSuffix, 2).unwrap();
    builder.add("test", Group::BlockSuffix, 1).unwrap();
    let aggregate = local.aggregate(
        builder.prepare().unwrap().finish_radix().unwrap(),
        vec![String::new(), "local".into(), "z-list".into()],
        [None, None],
    );
    let check = |name| aggregate.explain(&Name::from_ascii(name).unwrap());
    assert_eq!(
        check("safe.test"),
        Explanation {
            decision: Decision::Allowed,
            witness: Some(Witness {
                source_id: None,
                rule: "safe.test".into(),
                scope: Scope::Exact,
            })
        }
    );
    assert_eq!(
        check("a.test"),
        Explanation {
            decision: Decision::Blocked,
            witness: Some(Witness {
                source_id: Some("local".into()),
                rule: "test".into(),
                scope: Scope::Suffix,
            })
        }
    );
    assert_eq!(check("other.example").decision, Decision::Unmatched);
    assert_eq!(aggregate.local_bytes(), 0);
    assert!(
        aggregate
            .append_to(&mut Builder::new(Limits::default()).unwrap())
            .is_err()
    );
}

#[test]
fn publication_digest_ignores_unused_slots_but_tracks_actual_witness_identity() {
    let make = |slot, ids: &[&str]| {
        let mut builder = Builder::new(Limits::default()).unwrap();
        builder.add("test", Group::BlockSuffix, slot).unwrap();
        Policy::default().aggregate(
            builder.prepare().unwrap().finish_radix().unwrap(),
            ids.iter().map(|s| s.to_string()).collect(),
            vec![None; ids.len() - 1],
        )
    };
    let original = make(1, &["", "b-list"]);
    let unused = make(2, &["", "a-list", "b-list"]);
    let renamed = make(1, &["", "c-list"]);
    assert_eq!(original.publication_digest(), unused.publication_digest());
    assert_eq!(original.semantic_digest(), renamed.semantic_digest());
    assert_ne!(original.publication_digest(), renamed.publication_digest());
}

#[test]
fn local_source_is_raw_until_budgeted_compilation_and_can_reuse_owned_policy() {
    let raw: LocalRules = toml::from_str("enabled=true\nblock_suffix=['test']").unwrap();
    let source = LocalPolicySource::Inline(raw.clone());
    let rejected = source
        .compile(
            Limits {
                max_memory_bytes: raw.owned_bytes(),
                ..Limits::default()
            },
            || Ok(()),
        )
        .unwrap_err();
    assert_eq!(
        rejected.downcast_ref::<canonical::Error>().unwrap().kind,
        canonical::ErrorKind::MemoryLimit
    );
    let compiled = source.compile(Limits::default(), || Ok(())).unwrap();
    assert!(
        source
            .reuse_policy(&compiled)
            .unwrap()
            .same_allocation(&compiled)
    );
    let changed =
        LocalPolicySource::Inline(toml::from_str("enabled=false\nblock_suffix=['test']").unwrap());
    assert!(changed.reuse_policy(&compiled).is_none());
}

#[test]
fn local_file_buffer_is_charged_before_reading_or_compiling_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rules.toml");
    std::fs::write(
        &path,
        format!("#{}\nenabled=true\nblock_exact=['test']", "x".repeat(8192)),
    )
    .unwrap();
    let source = LocalPolicySource::File(path);
    let error = source
        .compile(
            Limits {
                max_memory_bytes: 4096,
                ..Limits::default()
            },
            || Ok(()),
        )
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<canonical::Error>().unwrap().kind,
        canonical::ErrorKind::MemoryLimit
    );
    assert!(
        source
            .compile(Limits::default(), || Ok(()))
            .unwrap()
            .blocks(&Name::from_ascii("test").unwrap())
    );
}

fn question(name: &str, kind: RecordType, edns: bool) -> Message {
    let mut q = Message::new(9, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(Name::from_ascii(name).unwrap(), kind));
    if edns {
        q.edns = Some(hickory_proto::op::Edns::new());
    }
    q
}

/// Code, answer data, SOA owner and whether EDE 15 is present.
fn answer(
    p: &Policy,
    name: &str,
    kind: RecordType,
) -> (ResponseCode, Vec<String>, Option<String>, bool) {
    let (reply, _) = p.block_query(&question(name, kind, true)).expect("blocked");
    assert!(!reply.metadata.authentic_data);
    let ttls: Vec<_> = reply
        .answers
        .iter()
        .chain(&reply.authorities)
        .map(|r| r.ttl)
        .collect();
    assert!(ttls.iter().all(|ttl| *ttl == 10), "{ttls:?}");
    let soa = reply.authorities.first().map(|r| {
        let RData::SOA(soa) = &r.data else {
            panic!("authority is not a SOA")
        };
        assert_eq!(soa.minimum, 10);
        r.name.to_string()
    });
    let ede = reply.edns.as_ref().is_some_and(|edns| {
        edns.option(hickory_proto::rr::rdata::opt::EdnsCode::Unknown(15))
            .is_some_and(|option| {
                matches!(option, hickory_proto::rr::rdata::opt::EdnsOption::Unknown(_, data) if data == &[0, 15])
            })
    });
    (
        reply.metadata.response_code,
        reply.answers.iter().map(|r| r.data.to_string()).collect(),
        soa,
        ede,
    )
}

#[test]
fn every_method_answers_a_aaaa_and_other_types() {
    use ResponseCode::{NXDomain, NoError, Refused};
    let p = policy(
        "block_suffix = ['nodata.test', 'null.test null_ip', 'nx.test nxdomain', 'ref.test refused', \
         'v4.test 192.0.2.10', 'both.test 192.0.2.11 2001:db8::11']",
    );
    let none: Vec<String> = Vec::new();
    let soa = |rule: &str| Some(format!("{rule}."));
    for (name, kind, expected) in [
        (
            "a.nodata.test",
            RecordType::A,
            (NoError, none.clone(), soa("nodata.test")),
        ),
        (
            "a.null.test",
            RecordType::A,
            (NoError, vec!["0.0.0.0".into()], None),
        ),
        (
            "a.null.test",
            RecordType::AAAA,
            (NoError, vec!["::".into()], None),
        ),
        (
            "a.null.test",
            RecordType::HTTPS,
            (NoError, none.clone(), soa("null.test")),
        ),
        (
            "a.nx.test",
            RecordType::A,
            (NXDomain, none.clone(), soa("nx.test")),
        ),
        (
            "a.nx.test",
            RecordType::TXT,
            (NXDomain, none.clone(), soa("nx.test")),
        ),
        ("a.ref.test", RecordType::A, (Refused, none.clone(), None)),
        (
            "a.v4.test",
            RecordType::A,
            (NoError, vec!["192.0.2.10".into()], None),
        ),
        // A custom IP without the asked family answers like nodata.
        (
            "a.v4.test",
            RecordType::AAAA,
            (NoError, none.clone(), soa("v4.test")),
        ),
        (
            "a.both.test",
            RecordType::AAAA,
            (NoError, vec!["2001:db8::11".into()], None),
        ),
        (
            "a.both.test",
            RecordType::MX,
            (NoError, none.clone(), soa("both.test")),
        ),
    ] {
        let (code, data, owner, ede) = answer(&p, name, kind);
        assert_eq!((code, data, owner), expected, "{name} {kind}");
        assert!(ede, "{name} {kind}");
    }
    // Clients without EDNS get no OPT record.
    let (reply, trace) = p
        .block_query(&question("a.nx.test", RecordType::A, false))
        .unwrap();
    assert!(reply.edns.is_none());
    assert_eq!(
        trace,
        BlockTrace {
            rule: "nx.test".into(),
            source_id: None,
            response: "nxdomain".into()
        }
    );
}

#[test]
fn global_method_and_ttl_apply_to_rules_without_their_own() {
    let p = policy(
        "block_response = 'custom_ip'\nblock_ipv6 = '2001:db8::1'\nblocked_ttl_secs = 60\n\
         block_exact = ['global.test', 'own.test nodata']",
    );
    let (reply, trace) = p
        .block_query(&question("global.test", RecordType::AAAA, false))
        .unwrap();
    assert_eq!(reply.answers[0].data.to_string(), "2001:db8::1");
    assert_eq!(reply.answers[0].ttl, 60);
    assert_eq!(trace.response, "custom_ip");
    let (reply, _) = p
        .block_query(&question("global.test", RecordType::A, false))
        .unwrap();
    assert!(reply.answers.is_empty() && reply.authorities[0].ttl == 60);
    let (reply, trace) = p
        .block_query(&question("own.test", RecordType::AAAA, false))
        .unwrap();
    assert!(reply.answers.is_empty());
    assert_eq!(trace.response, "nodata");
}

#[test]
fn a_blocked_chain_target_answers_with_its_rule_method() {
    let p = policy("block_suffix = ['ads.test nxdomain']");
    let q = query();
    let mut reply = crate::protocol::error_response(&q, ResponseCode::NoError);
    reply.answers = vec![cname("alias.test", "x.ads.test")];
    let trace = p.apply_response(&q, &mut reply).unwrap();
    assert_eq!(
        (trace.rule.as_str(), trace.response.as_str()),
        ("ads.test", "nxdomain")
    );
    assert_eq!(reply.metadata.response_code, ResponseCode::NXDomain);
    assert!(reply.answers.is_empty());
    assert_eq!(
        reply.authorities[0].name,
        Name::from_ascii("ads.test.").unwrap()
    );
}

#[test]
fn rule_lines_and_block_settings_are_validated_with_their_position() {
    for rules in [
        "block_exact = ['a.test null_ip', 'b.test 192.0.2.1 2001:db8::1', 'c.test refused', 'd.test 2001:db8::2']",
        "block_response = 'custom_ip'\nblock_ipv4 = '192.0.2.1'",
        "block_response = 'refused'\nblocked_ttl_secs = 86400",
    ] {
        assert!(toml::from_str::<LocalRules>(rules).is_ok(), "{rules}");
    }
    for (rules, message) in [
        (
            "block_exact = ['ok.test', 'a.test drop']",
            "filter.block_exact[1]",
        ),
        (
            "block_suffix = ['a.test custom_ip']",
            "filter.block_suffix[0]",
        ),
        (
            "block_suffix = ['a.test 192.0.2.1 192.0.2.2']",
            "at most one IPv4",
        ),
        ("block_suffix = ['a.test 224.0.0.1']", "not a unicast"),
        ("block_suffix = ['a.test 0.0.0.0']", "not a unicast"),
        (
            "block_suffix = ['a.test nodata extra']",
            "filter.block_suffix[0]",
        ),
        (
            "allow_exact = ['a.test nodata']",
            "allow rules take no block method",
        ),
        ("block_response = 'custom_ip'", "needs block_ipv4"),
        ("block_ipv4 = '192.0.2.1'", "only with block_response"),
        (
            "block_response = 'custom_ip'\nblock_ipv6 = 'ff02::1'",
            "not a unicast",
        ),
        ("block_response = 'drop'", "unknown variant"),
        ("blocked_ttl_secs = 0", "blocked_ttl_secs"),
        ("blocked_ttl_secs = 86401", "blocked_ttl_secs"),
    ] {
        let error = format!("{:#}", toml::from_str::<LocalRules>(rules).unwrap_err());
        assert!(error.contains(message), "{rules}: {error}");
    }
}

/// Local slots: 0 global, 1 nxdomain. Subscription slots 2 (refused) and 3
/// (follows the global nodata), in configured order.
fn layered(local: &str, sources: &[(&str, Group, u16)]) -> Policy {
    let marker = if local.contains("nxdomain") {
        ""
    } else {
        "\nblock_exact = ['marker.invalid nxdomain']"
    };
    let local = policy(&format!("{local}{marker}"));
    let mut builder = Builder::new(Limits::default()).unwrap();
    local.append_to(&mut builder).unwrap();
    let responses = [Some(BlockResponse::Refused), None];
    builder.set_classes(local.aggregate_classes(responses));
    for (rule, group, slot) in sources {
        builder.add(rule, *group, *slot).unwrap();
    }
    local.aggregate(
        builder.prepare().unwrap().finish_radix().unwrap(),
        local.aggregate_ids(["first".into(), "second".into()]),
        responses,
    )
}

#[test]
fn the_most_specific_rule_chooses_the_method() {
    let method = |p: &Policy, name: &str| {
        p.block_query(&question(name, RecordType::A, false))
            .map(|(_, trace)| (trace.rule, trace.source_id, trace.response))
    };
    let local = |rule: &str, response: &str| Some((rule.to_string(), None, response.to_string()));
    let source = |rule: &str, id: &str, response: &str| {
        Some((rule.to_string(), Some(id.to_string()), response.to_string()))
    };
    // Local exact beats local suffix, which beats subscription exact.
    let p = layered(
        "block_suffix = ['test']\nblock_exact = ['a.b.test nxdomain']",
        &[
            ("a.b.test", Group::BlockExact, 2),
            ("b.test", Group::BlockSuffix, 2),
        ],
    );
    assert_eq!(method(&p, "a.b.test"), local("a.b.test", "nxdomain"));
    assert_eq!(method(&p, "x.b.test"), local("test", "nodata"));
    // The longest local suffix wins over a shorter one.
    let p = layered("block_suffix = ['test', 'b.test nxdomain']", &[]);
    assert_eq!(method(&p, "x.b.test"), local("b.test", "nxdomain"));
    assert_eq!(method(&p, "x.c.test"), local("test", "nodata"));
    // Subscription exact beats subscription suffix; the longest suffix wins;
    // the same rule in two sources answers with the first configured source.
    let p = layered(
        "",
        &[
            ("test", Group::BlockSuffix, 2),
            ("b.test", Group::BlockSuffix, 3),
            ("a.b.test", Group::BlockExact, 2),
            ("c.test", Group::BlockSuffix, 3),
            ("c.test", Group::BlockSuffix, 2),
        ],
    );
    assert_eq!(
        method(&p, "a.b.test"),
        source("a.b.test", "first", "refused")
    );
    assert_eq!(method(&p, "x.b.test"), source("b.test", "second", "nodata"));
    assert_eq!(method(&p, "x.test"), source("test", "first", "refused"));
    // c.test in the first source answers like its covering `test` rule there,
    // so the index keeps only `test`; the second source's copy never wins.
    assert_eq!(method(&p, "x.c.test"), source("test", "first", "refused"));
    // A local allow still wins over every block rule.
    let p = layered(
        "allow_exact = ['a.b.test']",
        &[("b.test", Group::BlockSuffix, 2)],
    );
    assert_eq!(method(&p, "a.b.test"), None);
}

#[test]
fn method_changes_publish_and_rename_only_when_answers_change() {
    let make = |global: &str, source: Option<BlockResponse>| {
        let local = policy(&format!("block_response = '{global}'"));
        let mut builder = Builder::new(Limits::default()).unwrap();
        local.append_to(&mut builder).unwrap();
        builder.set_classes(local.aggregate_classes([source]));
        builder.add("test", Group::BlockSuffix, 1).unwrap();
        local.aggregate(
            builder.prepare().unwrap().finish_radix().unwrap(),
            local.aggregate_ids(["list".into()]),
            [source],
        )
    };
    let follows = make("nodata", None);
    let explicit = make("nodata", Some(BlockResponse::Nodata));
    let refused = make("nodata", Some(BlockResponse::Refused));
    let global = make("refused", None);
    // The same answers publish the same generation, whichever setting gives them.
    assert_eq!(follows.publication_digest(), explicit.publication_digest());
    assert_ne!(follows.publication_digest(), refused.publication_digest());
    assert_eq!(refused.publication_digest(), global.publication_digest());
    assert_eq!(follows.semantic_digest(), refused.semantic_digest());
}

#[test]
fn derived_index_with_kept_covered_rules_round_trips() {
    let local = policy("block_suffix = ['test', 'b.test nxdomain']");
    let mut builder = Builder::new(Limits::default()).unwrap();
    local.append_to(&mut builder).unwrap();
    let responses = [Some(BlockResponse::Refused), None];
    builder.set_classes(local.aggregate_classes(responses));
    builder.add("a.b.test", Group::BlockExact, 3).unwrap();
    builder.add("c.test", Group::BlockSuffix, 2).unwrap();
    // Configured order, not sorted by ID.
    let ids = local.aggregate_ids(["zeta".into(), "alpha".into()]);
    let policy = local.aggregate(
        builder.prepare().unwrap().finish_radix().unwrap(),
        ids.clone(),
        responses,
    );
    let mut bytes = Vec::new();
    policy.write_index(&mut bytes, [7; 32]).unwrap();
    let index =
        radix::Index::read_from(bytes.as_slice(), [7; 32], &ids, Limits::default()).unwrap();
    let restored = local.aggregate(index, ids, responses);
    assert_eq!(restored.publication_digest(), policy.publication_digest());
    for name in ["a.b.test", "x.b.test", "x.c.test", "x.test"] {
        let query = question(name, RecordType::A, false);
        assert_eq!(
            restored.block_query(&query).map(|(_, trace)| trace),
            policy.block_query(&query).map(|(_, trace)| trace),
            "{name}"
        );
    }
    // Local suffixes outrank every subscription rule.
    let (_, trace) = restored
        .block_query(&question("x.c.test", RecordType::A, false))
        .unwrap();
    assert_eq!(
        (trace.source_id.as_deref(), trace.response.as_str()),
        (None, "nodata")
    );
    let (_, trace) = restored
        .block_query(&question("a.b.test", RecordType::A, false))
        .unwrap();
    assert_eq!(
        (trace.rule.as_str(), trace.response.as_str()),
        ("b.test", "nxdomain")
    );
}
