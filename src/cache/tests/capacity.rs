use super::*;

fn partition(cache: &Cache, name: &str) -> serde_json::Value {
    cache.diagnostics_snapshot()["capacity"]["shards"][0][name].clone()
}

fn eviction(partition: &serde_json::Value, reason: &str, state: &str) -> u64 {
    partition["evictions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["reason"] == reason)
        .unwrap()[state]
        .as_u64()
        .unwrap()
}

#[test]
fn partition_budgets_sum_across_shards() {
    let cache = Cache::new(CacheConfig {
        shards: 4,
        max_entries: 11,
        max_bytes: 32_001,
        negative_percent: 31,
        ..Default::default()
    });
    let diagnostic = cache.diagnostics_snapshot();
    let shards = diagnostic["capacity"]["shards"].as_array().unwrap();
    assert_eq!(shards.len(), 3);
    for (index, shard) in shards.iter().enumerate() {
        assert_eq!(shard["shard"], index);
        for part in ["positive", "negative"] {
            assert_eq!(shard[part]["entries"], 0);
            assert_eq!(shard[part]["bytes"], 0);
            assert_eq!(shard[part]["evictions"].as_array().unwrap().len(), 4);
        }
    }
    let sum = |part: &str, key: &str| -> u64 {
        shards.iter().map(|s| s[part][key].as_u64().unwrap()).sum()
    };
    assert_eq!(sum("positive", "max_entries"), 8);
    assert_eq!(sum("negative", "max_entries"), 3);
    assert_eq!(sum("negative", "max_bytes"), 9_920);
    assert_eq!(sum("positive", "max_bytes"), 22_081);
}

#[test]
fn fresh_evictions_belong_to_the_correct_partition() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 2,
        negative_percent: 50,
        ..Default::default()
    });
    let now = Instant::now();
    for label in ["a", "b"] {
        let positive = query(&format!("{label}.test."));
        let negative = query(&format!("missing-{label}.test."));
        insert(
            &cache,
            &positive,
            &answer(&positive, 1, 60),
            Scope::NoEcs,
            now,
        );
        insert(
            &cache,
            &negative,
            &negative_answer(&negative),
            Scope::NoEcs,
            now,
        );
    }
    for name in ["positive", "negative"] {
        let part = partition(&cache, name);
        assert_eq!(part["entries"], 1);
        assert_eq!(eviction(&part, "entry_limit", "fresh"), 1);
        assert_eq!(eviction(&part, "entry_limit", "retained"), 0);
        assert_eq!(eviction(&part, "entry_limit", "expired"), 0);
    }
}

#[test]
fn entry_byte_and_combined_limits_are_distinguished() {
    let now = Instant::now();
    let a = query("a.test.");
    let b = query("b.test.");
    let measure = Cache::new(CacheConfig {
        shards: 1,
        negative_percent: 0,
        ..Default::default()
    });
    insert(&measure, &a, &answer(&a, 1, 60), Scope::NoEcs, now);
    let charge = partition(&measure, "positive")["bytes"].as_u64().unwrap() as usize;
    for (max_entries, max_bytes, reason) in [
        (1, charge * 3, "entry_limit"),
        (3, charge * 2 - 1, "byte_limit"),
        (1, charge * 2 - 1, "entry_and_byte_limit"),
    ] {
        let cache = Cache::new(CacheConfig {
            shards: 1,
            max_entries,
            max_bytes,
            negative_percent: 0,
            ..Default::default()
        });
        for q in [&a, &b] {
            insert(&cache, q, &answer(q, 1, 60), Scope::NoEcs, now);
        }
        let part = partition(&cache, "positive");
        assert_eq!(part["entries"], 1, "{reason}");
        assert_eq!(eviction(&part, reason, "fresh"), 1, "{reason}");
        assert_eq!(
            part["evictions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["fresh"].as_u64().unwrap())
                .sum::<u64>(),
            1
        );
    }
}

#[test]
fn variant_limit_records_the_victim_without_capacity_pressure() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 8,
        max_variants: 1,
        negative_percent: 0,
        ..Default::default()
    });
    let q = query("variant.test.");
    let now = Instant::now();
    for subnet in ["192.0.1.0/24", "192.0.2.0/24"] {
        insert(&cache, &q, &answer(&q, 1, 60), network(subnet), now);
    }
    let part = partition(&cache, "positive");
    assert_eq!(part["entries"], 1);
    assert_eq!(eviction(&part, "variant_limit", "fresh"), 1);
    assert_eq!(eviction(&part, "entry_limit", "fresh"), 0);
}

#[test]
fn victim_age_distinguishes_retained_and_expired_entries() {
    for (seconds, state) in [(10, "retained"), (30, "expired")] {
        let mut config = CacheConfig {
            shards: 1,
            max_entries: 1,
            negative_percent: 0,
            ..Default::default()
        };
        config.stale.enabled = true;
        config.stale.retention_secs = 20;
        let cache = Cache::new(config);
        let a = query("a.test.");
        let b = query("b.test.");
        let now = Instant::now();
        insert(&cache, &a, &answer(&a, 1, 10), Scope::NoEcs, now);
        insert(
            &cache,
            &b,
            &answer(&b, 1, 60),
            Scope::NoEcs,
            now + Duration::from_secs(seconds),
        );
        let part = partition(&cache, "positive");
        assert_eq!(eviction(&part, "entry_limit", state), 1, "{state}");
    }
}

#[test]
fn capacity_snapshots_do_not_touch_recency_hits_or_refresh_settlement() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 2,
        negative_percent: 0,
        ..Default::default()
    });
    let now = Instant::now();
    let a = query("a.test.");
    let b = query("b.test.");
    let c = query("c.test.");
    assert!(
        cache
            .insert_refresh_if_epoch(&a, &answer(&a, 1, 60), Scope::NoEcs, now, cache.epoch())
            .admitted()
    );
    insert(&cache, &b, &answer(&b, 1, 60), Scope::NoEcs, now);
    let stats = cache.snapshot();
    let before = cache.diagnostics_snapshot();
    for _ in 0..3 {
        assert_eq!(cache.diagnostics_snapshot(), before);
    }
    assert_eq!(cache.snapshot(), stats);
    assert_eq!(cache.refresh_outcomes(), (0, 0));
    insert(&cache, &c, &answer(&c, 1, 60), Scope::NoEcs, now);
    assert_eq!(cache.refresh_outcomes(), (0, 1));
    assert!(cache.peek(&a, None, now, false).is_none());
    assert!(cache.peek(&b, None, now, false).is_some());
}

#[test]
fn resident_entries_and_held_reader_bytes_are_reported_separately() {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        negative_percent: 0,
        ..Default::default()
    });
    let now = Instant::now();
    let q = query("held.test.");
    insert(&cache, &q, &answer(&q, 1, 60), Scope::NoEcs, now);
    let held = {
        let shard = cache.shards[0].lock().unwrap();
        Arc::clone(&shard.buckets.get(&Key::of(&q).unwrap()).unwrap()[0])
    };
    cache.invalidate(None, None, None);
    let part = partition(&cache, "positive");
    assert_eq!(part["entries"], 0);
    assert_eq!(part["bytes"], held.charge);
    drop(held);
    assert_eq!(partition(&cache, "positive")["bytes"], 0);
}
