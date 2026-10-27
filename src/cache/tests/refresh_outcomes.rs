use super::*;

fn refreshed(cache: &Cache, query: &Message, scope: Scope, now: Instant) {
    let result =
        cache.insert_refresh_if_epoch(query, &answer(query, 1, 2), scope, now, cache.epoch());
    assert!(matches!(
        result.outcome,
        StoreOutcome::Admitted | StoreOutcome::Replaced
    ));
}

#[test]
fn the_first_fresh_lookup_settles_a_refresh_once_but_inspection_does_not() {
    let cache = Cache::new(CacheConfig::default());
    let query = query("fresh.test.");
    let now = Instant::now();
    refreshed(&cache, &query, Scope::NoEcs, now);
    assert!(cache.peek(&query, None, now, false).is_some());
    assert_eq!(cache.explain(&query, None, now)["state"], "fresh");
    assert_eq!(
        cache.inspect(query.queries[0].name(), None, now)["variants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(cache.snapshot()["refresh_useful"], 0);
    assert_eq!(cache.refresh_outcomes(), (0, 0));
    for _ in 0..3 {
        assert!(get(&cache, &query, None, now).is_some());
        assert_eq!(cache.refresh_outcomes(), (1, 0));
    }
    cache.invalidate(None, None, None);
    assert_eq!(cache.refresh_outcomes(), (1, 0));
}

#[test]
fn a_refresh_first_used_stale_is_unused_even_when_returned_repeatedly() {
    let mut config = CacheConfig::default();
    config.stale.enabled = true;
    config.stale.retention_secs = 10;
    let cache = Cache::new(config);
    let query = query("stale.test.");
    let now = Instant::now();
    refreshed(&cache, &query, Scope::NoEcs, now);
    let stale = now + Duration::from_secs(2);
    assert_eq!(cache.explain(&query, None, stale)["state"], "stale");
    assert_eq!(cache.refresh_outcomes(), (0, 0));
    for _ in 0..2 {
        assert!(cache.lookup(&query, None, stale, true).unwrap().stale);
        assert_eq!(cache.refresh_outcomes(), (0, 1));
    }
    cache.invalidate(None, None, None);
    assert_eq!(cache.refresh_outcomes(), (0, 1));
}

#[test]
fn removal_settles_unseen_refreshes_but_ordinary_answers_do_not_count() {
    let cache = Cache::new(CacheConfig {
        max_entries: 1,
        negative_percent: 0,
        shards: 1,
        ..CacheConfig::default()
    });
    let first = query("first.test.");
    let other = query("other.test.");
    let now = Instant::now();
    refreshed(&cache, &first, Scope::NoEcs, now);
    refreshed(&cache, &first, Scope::NoEcs, now);
    assert_eq!(cache.refresh_outcomes(), (0, 1), "replacement");
    cache.invalidate(Some(other.queries[0].name()), None, None);
    assert_eq!(cache.refresh_outcomes(), (0, 1));
    cache.invalidate(Some(first.queries[0].name()), None, None);
    assert_eq!(cache.refresh_outcomes(), (0, 2), "invalidation");
    refreshed(&cache, &first, Scope::NoEcs, now);
    insert(&cache, &other, &answer(&other, 2, 60), Scope::NoEcs, now);
    assert_eq!(cache.refresh_outcomes(), (0, 3), "eviction");
    assert!(get(&cache, &other, None, now).is_some());
    cache.invalidate(None, None, None);
    assert_eq!(cache.refresh_outcomes(), (0, 3));
}

#[test]
fn an_expired_refresh_is_unused_when_reaped_and_an_old_epoch_creates_no_entry() {
    let mut config = CacheConfig::default();
    config.stale.enabled = false;
    let cache = Cache::new(config);
    let query = query("expired.test.");
    let now = Instant::now();
    refreshed(&cache, &query, Scope::NoEcs, now);
    let expired = now + Duration::from_secs(3);
    assert_eq!(
        cache.inspect(query.queries[0].name(), None, expired)["variants"][0]["state"],
        "expired"
    );
    assert_eq!(cache.refresh_outcomes(), (0, 0));
    assert!(cache.peek(&query, None, expired, true).is_none());
    assert_eq!(cache.refresh_outcomes(), (0, 0));
    assert!(get(&cache, &query, None, expired).is_none());
    assert!(get(&cache, &query, None, expired).is_none());
    assert_eq!(cache.refresh_outcomes(), (0, 1));
    let old_epoch = cache.epoch();
    cache.invalidate(None, None, None);
    cache.insert_refresh_if_epoch(&query, &answer(&query, 1, 60), Scope::NoEcs, now, old_epoch);
    assert!(get(&cache, &query, None, now).is_none());
    assert_eq!(cache.refresh_outcomes(), (0, 1));
}

#[test]
fn a_shared_refresh_is_useful_once_across_its_client_networks() {
    let cache = Cache::new(CacheConfig::default());
    let query = query("networks.test.");
    let now = Instant::now();
    refreshed(&cache, &query, network("192.0.0.0/16"), now);
    for client in ["192.0.2.0/24", "192.0.3.0/24"] {
        assert!(get(&cache, &query, ecs(client), now).is_some());
        assert_eq!(cache.refresh_outcomes(), (1, 0));
    }
}
