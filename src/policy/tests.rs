use super::*;
use hickory_proto::{
    op::{MessageType, OpCode, Query},
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
    p.apply_response(&q, &mut reply);
    assert!(
        reply.answers.is_empty() && reply.authorities.is_empty() && reply.additionals.is_empty()
    );
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
