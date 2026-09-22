use super::*;

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
