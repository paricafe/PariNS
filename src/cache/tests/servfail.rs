//! Cached upstream SERVFAIL (RFC 9520).
use super::*;

fn servfail(q: &Message) -> Message {
    protocol::error_response(q, ResponseCode::ServFail)
}

fn fail(cache: &Cache, q: &Message, scope: Scope, now: Instant, failure: Failure) -> StoreDecision {
    cache.insert_failure_if_epoch(q, &servfail(q), scope, now, cache.epoch(), failure)
}

fn exact(text: &str) -> Scope {
    Scope::ExactSource(text.parse().unwrap())
}

fn lookup(cache: &Cache, q: &Message, outgoing: Option<ClientSubnet>, now: Instant) -> Option<Hit> {
    cache.lookup_with_decision(q, outgoing, now, false).hit
}

#[test]
fn explanation_follows_failure_and_stale_selection_without_observing_a_hit() {
    let now = Instant::now();
    for failure in [Failure::Final, Failure::StaleFallback] {
        for with_stale in [false, true] {
            let mut config = CacheConfig::default();
            config.stale.enabled = true;
            let cache = Cache::new(config);
            let q = query("explain-failure.test.");
            if with_stale {
                insert(&cache, &q, &answer(&q, 1, 10), Scope::NoEcs, now);
            }
            let later = now + Duration::from_secs(20);
            assert!(fail(&cache, &q, Scope::NoEcs, later, failure).admitted());
            let before = cache.inspect(q.queries[0].name(), None, later);
            let counters = cache.diagnostics_snapshot();
            let explanation = cache.explain(&q, None, later);
            assert_eq!(
                explanation["state"],
                if with_stale && failure == Failure::StaleFallback {
                    "stale"
                } else {
                    "failure"
                }
            );
            assert_eq!(
                explanation["reason"],
                match (failure, with_stale) {
                    (Failure::Final, _) => "cached_dnssec_failure",
                    (Failure::StaleFallback, true) => "cached_failure_stale_fallback",
                    (Failure::StaleFallback, false) => "cached_upstream_failure",
                }
            );
            assert_eq!(cache.inspect(q.queries[0].name(), None, later), before);
            assert_eq!(cache.diagnostics_snapshot(), counters);
            assert_eq!(cache.refresh_outcomes(), (0, 0));
        }
    }
}

#[test]
fn a_failure_is_cached_for_its_ttl_in_its_own_network_only() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("failing.test.");
    let now = Instant::now();
    assert!(
        fail(
            &cache,
            &q,
            exact("192.0.2.0/24"),
            now,
            Failure::StaleFallback
        )
        .admitted()
    );
    let hit = lookup(&cache, &q, ecs("192.0.2.0/24"), now).unwrap();
    assert_eq!(hit.failure, Some(Failure::StaleFallback));
    assert_eq!(hit.message.response_code, ResponseCode::ServFail);
    assert!(!hit.refresh);
    // Other networks, the other family and no-ECS traffic still miss.
    for outgoing in [ecs("198.51.100.0/24"), ecs("2001:db8::/56"), None] {
        assert!(lookup(&cache, &q, outgoing, now).is_none());
    }
    assert!(
        lookup(
            &cache,
            &q,
            ecs("192.0.2.0/24"),
            now + Duration::from_secs(29)
        )
        .is_some()
    );
    assert!(
        lookup(
            &cache,
            &q,
            ecs("192.0.2.0/24"),
            now + Duration::from_secs(30)
        )
        .is_none()
    );
    let status = cache.snapshot();
    assert_eq!(status["servfail_hits"], 2);
    assert_eq!(status["negative_entries"], 0, "expired entries are pruned");
}

#[test]
fn a_failure_never_replaces_an_answer_and_an_answer_replaces_it() {
    let mut config = CacheConfig::default();
    config.stale.enabled = true;
    let cache = Cache::new(config);
    let q = query("kept.test.");
    let now = Instant::now();
    // A fresh positive or negative answer refuses the failure.
    for response in [answer(&q, 1, 60), negative_answer(&q)] {
        insert(&cache, &q, &response, Scope::NoEcs, now);
        assert!(!fail(&cache, &q, Scope::NoEcs, now, Failure::StaleFallback).admitted());
        let hit = lookup(&cache, &q, None, now).unwrap();
        assert!(hit.failure.is_none());
        cache.invalidate(None, None, None);
    }
    // A stale answer stays next to the failure for fallback.
    insert(&cache, &q, &answer(&q, 2, 10), Scope::NoEcs, now);
    let later = now + Duration::from_secs(20);
    assert!(fail(&cache, &q, Scope::NoEcs, later, Failure::StaleFallback).admitted());
    assert!(lookup(&cache, &q, None, later).unwrap().failure.is_some());
    let stale = cache
        .lookup_with_decision(&q, None, later, true)
        .hit
        .unwrap();
    assert!(stale.stale && stale.failure.is_none());
    assert_eq!(
        stale.message.answers[0].data,
        RData::A(A::new(192, 0, 2, 2))
    );
    // A new answer supersedes the failure at once.
    insert(&cache, &q, &answer(&q, 3, 60), Scope::NoEcs, later);
    let hit = lookup(&cache, &q, None, later).unwrap();
    assert!(hit.failure.is_none());
    assert_eq!(hit.message.answers[0].data, RData::A(A::new(192, 0, 2, 3)));
    assert_eq!(cache.snapshot()["servfail_entries"], 0);
}

#[test]
fn a_disabled_or_bypassed_failure_cache_stores_nothing() {
    let cache = Cache::new(CacheConfig {
        servfail_ttl_secs: 0,
        ..CacheConfig::default()
    });
    let q = query("off.test.");
    let now = Instant::now();
    assert!(!fail(&cache, &q, Scope::NoEcs, now, Failure::StaleFallback).admitted());
    assert!(lookup(&cache, &q, None, now).is_none());
    // Only a SERVFAIL is stored as a failure.
    let cache = Cache::new(CacheConfig::default());
    let refused = protocol::error_response(&q, ResponseCode::Refused);
    assert!(
        !cache
            .insert_failure_if_epoch(
                &q,
                &refused,
                Scope::NoEcs,
                now,
                cache.epoch(),
                Failure::Final
            )
            .admitted()
    );
    // Work older than an invalidation does not store a failure either.
    let epoch = cache.epoch();
    cache.invalidate(None, None, None);
    assert!(
        !cache
            .insert_failure_if_epoch(&q, &servfail(&q), Scope::NoEcs, now, epoch, Failure::Final)
            .admitted()
    );
}

#[test]
fn a_failure_keeps_its_extended_errors_and_is_never_saved() {
    let cache = Cache::new(CacheConfig::default());
    let mut q = query("bogus.test.");
    q.edns = Some(Edns::new());
    let now = Instant::now();
    let mut response = servfail(&q);
    response
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(15, vec![0, 6]));
    assert!(
        cache
            .insert_failure_if_epoch(
                &q,
                &response,
                Scope::NoEcs,
                now,
                cache.epoch(),
                Failure::Final
            )
            .admitted()
    );
    let hit = lookup(&cache, &q, None, now).unwrap();
    assert_eq!(hit.failure, Some(Failure::Final));
    assert_eq!(
        hit.message.edns.unwrap().option(EdnsCode::Unknown(15)),
        Some(&EdnsOption::Unknown(15, vec![0, 6]))
    );
    let mut file = std::io::Cursor::new(Vec::new());
    let report = cache
        .write_clean_snapshot(
            &mut file,
            "fingerprint",
            std::time::SystemTime::now(),
            now,
            usize::MAX,
            now + Duration::from_secs(10),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((report.saved, report.skipped), (0, 0));
}
