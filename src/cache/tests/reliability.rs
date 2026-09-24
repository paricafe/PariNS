use super::*;

fn exact(cidr: &str) -> Scope {
    Scope::ExactSource(cidr.parse().unwrap())
}

fn padded(mut message: Message, padding: Vec<u8>) -> Message {
    message
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(12, padding));
    message
}

#[test]
fn exact_source_matches_only_identical_family_network_and_prefix() {
    for (scope, same, different) in [
        ("192.0.2.0/24", "192.0.2.99/24", "192.0.2.0/25"),
        ("2001:db8::/32", "2001:db8::1/32", "2001:db8::/48"),
    ] {
        let cache = Cache::new(CacheConfig::default());
        let query = query("exact.test.");
        let now = Instant::now();
        cache.insert(&query, &answer(&query, 1, 60), exact(scope), now);
        assert!(cache.peek(&query, ecs(same), now, false).is_some());
        for outgoing in [ecs(different), ecs("0.0.0.0/0"), ecs("::/0"), None] {
            assert!(cache.peek(&query, outgoing, now, false).is_none());
        }
        assert_eq!(
            cache.inspect("exact.test.", None, now)["variants"][0]["scope"],
            format!("exact_ecs:{scope}")
        );
        assert_eq!(cache.invalidate(None, None, Some(network(scope))), 0);
        assert_eq!(cache.invalidate(None, None, Some(exact(scope))), 1);
    }
}

#[test]
fn network_exact_supersession_is_bidirectional_for_every_uncacheable_success() {
    for (old, new) in [
        (network("192.0.0.0/16"), exact("192.0.2.0/24")),
        (exact("192.0.2.0/24"), network("192.0.0.0/16")),
        (exact("192.0.2.0/24"), exact("192.0.2.0/25")),
    ] {
        for cause in [
            "ttl_zero",
            "ede",
            "capacity",
            "negative_without_soa",
            "short_ttl",
        ] {
            let config = CacheConfig {
                shards: 1,
                max_bytes: 4096,
                negative_percent: 0,
                stale: crate::config::StaleConfig {
                    enabled: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            let cache = Cache::new(config);
            let query = query("supersede.test.");
            let now = Instant::now();
            cache.insert(&query, &answer(&query, 1, 60), old, now);
            cache.insert(
                &query,
                &answer(&query, 3, 60),
                exact("198.51.100.0/24"),
                now,
            );
            let mut response = answer(&query, 2, 60);
            match cause {
                "ttl_zero" => response.answers[0].ttl = 0,
                "ede" => response = padded(with_ede(response), vec![9; 16]),
                "negative_without_soa" => {
                    response = protocol::error_response(&query, ResponseCode::NXDomain)
                }
                "capacity" => {
                    for i in 0..400 {
                        response.add_additional(Record::from_rdata(
                            Name::from_ascii(format!("extra{i}.test.")).unwrap(),
                            60,
                            RData::A(A::new(192, 0, 2, 2)),
                        ));
                    }
                }
                "short_ttl" => response.answers[0].ttl = 1,
                _ => unreachable!(),
            }
            let decision =
                cache.insert_decision_if_epoch(&query, &response, new, now, cache.epoch());
            assert_eq!(
                decision.outcome,
                if cause == "short_ttl" {
                    StoreOutcome::Replaced
                } else {
                    StoreOutcome::SupersededOnly
                },
                "{cause}"
            );
            for outgoing in [
                ecs("192.0.2.0/24"),
                ecs("192.0.2.0/25"),
                ecs("192.0.3.0/24"),
            ] {
                if let Some(hit) = cache.peek(&query, outgoing, now + Duration::from_secs(2), true)
                {
                    assert_ne!(
                        hit.message.answers[0].data,
                        RData::A(A::new(192, 0, 2, 1)),
                        "{cause}"
                    );
                }
            }
            assert!(
                cache
                    .peek(&query, ecs("198.51.100.0/24"), now, false)
                    .is_some()
            );
        }
    }
}

#[test]
fn padding_is_not_a_cache_key_or_stored_wire_option_and_unknowns_still_bypass() {
    let cache = Cache::new(CacheConfig::default());
    let plain = query("padded.test.");
    let query = padded(plain, vec![7; 500]);
    let now = Instant::now();
    let response = padded(answer(&query, 1, 60), vec![9; 900]);
    let decision =
        cache.insert_decision_if_epoch(&query, &response, Scope::NoEcs, now, cache.epoch());
    assert_eq!(decision.outcome, StoreOutcome::Admitted);
    for padding in [Some(vec![]), Some(vec![2; 1000]), None] {
        let mut current = query.clone();
        current
            .edns
            .as_mut()
            .unwrap()
            .options_mut()
            .remove(EdnsCode::Padding);
        if let Some(bytes) = padding {
            current
                .edns
                .as_mut()
                .unwrap()
                .options_mut()
                .insert(EdnsOption::Unknown(12, bytes));
        }
        let hit = cache.peek(&current, None, now, false).unwrap();
        assert!(
            hit.message
                .edns
                .as_ref()
                .unwrap()
                .option(EdnsCode::Padding)
                .is_none()
        );
    }
    let key = Key::of(&query).unwrap();
    let shard = cache.shards[cache.shard(&key)].lock().unwrap();
    assert!(
        protocol::decode(&shard.buckets[&key][0].wire)
            .unwrap()
            .edns
            .is_none()
    );
    drop(shard);
    for option in [10, 65001] {
        let mut response = response.clone();
        response
            .edns
            .as_mut()
            .unwrap()
            .options_mut()
            .insert(EdnsOption::Unknown(option, vec![1; 8]));
        let decision =
            cache.insert_decision_if_epoch(&query, &response, Scope::NoEcs, now, cache.epoch());
        assert_eq!(
            decision,
            StoreDecision::skipped(DecisionReason::UnsupportedEdns)
        );
        assert!(cache.peek(&query, None, now, false).is_some());
    }
    assert_eq!(cache.snapshot()["rejections"], 0);
}

#[test]
fn diagnostic_decisions_report_actual_branches_without_budget_misclassification() {
    let cache = Cache::new(CacheConfig::default());
    let query = query("decisions.test.");
    let now = Instant::now();
    assert_eq!(
        cache
            .lookup_with_decision(&query, None, now, false)
            .decision
            .outcome,
        LookupOutcome::Miss
    );
    let zero = cache.insert_decision_if_epoch(
        &query,
        &answer(&query, 1, 0),
        Scope::NoEcs,
        now,
        cache.epoch(),
    );
    assert_eq!(zero, StoreDecision::skipped(DecisionReason::TtlZero));
    let negative = cache.insert_decision_if_epoch(
        &query,
        &protocol::error_response(&query, ResponseCode::NXDomain),
        Scope::NoEcs,
        now,
        cache.epoch(),
    );
    assert_eq!(
        negative,
        StoreDecision::skipped(DecisionReason::NegativeWithoutSoa)
    );
    cache.insert(&query, &answer(&query, 1, 60), Scope::NoEcs, now);
    assert_eq!(
        cache
            .lookup_with_decision(&query, None, now, false)
            .decision
            .outcome,
        LookupOutcome::Fresh
    );
    let before = cache.diagnostics_snapshot();
    cache.peek(&query, None, now, false);
    cache.inspect("decisions.test.", None, now);
    assert_eq!(before, cache.diagnostics_snapshot());
    assert_eq!(before["store"]["reasons"]["ttl_zero"], 1);
    assert_eq!(before["store"]["reasons"]["negative_without_soa"], 1);
    assert_eq!(cache.snapshot()["rejections"], 0);
}
