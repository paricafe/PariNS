use super::*;
use crate::cache::tests::insert;
use hickory_proto::rr::{
    Record as DnsRecord,
    rdata::{A, CNAME, NS, SOA},
};
use std::io::Cursor;

/// A deadline no test reaches.
fn far() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

const FP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn query(name: &str) -> Message {
    let mut query = Message::new(1, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    query
}

fn answer(q: &Message, ttl: u32) -> Message {
    let mut response = protocol::error_response(q, ResponseCode::NoError);
    response.add_answer(DnsRecord::from_rdata(
        q.queries[0].name().clone(),
        ttl,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    response
}

fn encode(cache: &Cache, now: Instant) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    cache
        .write_clean_snapshot(
            &mut cursor,
            FP,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            now,
            1_048_576,
            far(),
            &AtomicBool::new(false),
        )
        .unwrap();
    cursor.into_inner()
}

fn restore(cache: &Cache, bytes: Vec<u8>, offline_ms: u64, now: Instant) -> Result<SnapshotReport> {
    cache.restore_clean_snapshot(
        &mut Cursor::new(bytes),
        FP,
        SystemTime::UNIX_EPOCH + Duration::from_millis(1_000_000 + offline_ms),
        now,
        1_048_576,
        far(),
        &AtomicBool::new(false),
    )
}

#[test]
fn every_section_ages_twice_and_restoring_does_not_observe_requests() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("example.test.");
    let inserted = Instant::now();
    let mut response = answer(&q, 60);
    response.add_authority(DnsRecord::from_rdata(
        Name::from_ascii("test.").unwrap(),
        50,
        RData::A(A::new(192, 0, 2, 2)),
    ));
    response.add_additional(DnsRecord::from_rdata(
        Name::from_ascii("extra.test.").unwrap(),
        40,
        RData::A(A::new(192, 0, 2, 3)),
    ));
    insert(&cache, &q, &response, Scope::NoEcs, inserted);
    let bytes = encode(&cache, inserted + Duration::from_millis(3250));
    let restored = Cache::new(CacheConfig::default());
    let now = inserted + Duration::from_secs(10);
    assert_eq!(restore(&restored, bytes, 2100, now).unwrap().restored, 1);
    let response = restored.peek(&q, None, now, false).unwrap().message;
    assert_eq!(
        (
            response.answers[0].ttl,
            response.authorities[0].ttl,
            response.additionals[0].ttl
        ),
        (53, 43, 33)
    );
    let counts = restored.snapshot();
    for field in [
        "hits",
        "stale_hits",
        "misses",
        "bypasses",
        "rejections",
        "evictions",
    ] {
        assert_eq!(counts[field], 0, "{field}");
    }
    assert!(
        restored
            .peek(&q, None, now + Duration::from_secs(33), false)
            .is_none()
    );
}

#[test]
fn negative_answers_preserve_soa_lifetime_without_stale_restore() {
    let config = CacheConfig::default();
    let cache = Cache::new(config.clone());
    let q = query("negative.test.");
    let now = Instant::now();
    let mut response = protocol::error_response(&q, ResponseCode::NXDomain);
    response.add_authority(DnsRecord::from_rdata(
        Name::from_ascii("test.").unwrap(),
        100,
        RData::SOA(SOA::new(
            Name::from_ascii("ns.test.").unwrap(),
            Name::from_ascii("admin.test.").unwrap(),
            1,
            60,
            60,
            3600,
            30,
        )),
    ));
    insert(&cache, &q, &response, Scope::NoEcs, now);
    let restored = Cache::new(config);
    assert_eq!(
        restore(
            &restored,
            encode(&cache, now + Duration::from_secs(5)),
            4000,
            now
        )
        .unwrap()
        .restored,
        1
    );
    assert_eq!(
        restored
            .peek(&q, None, now, false)
            .unwrap()
            .message
            .authorities[0]
            .ttl,
        21
    );
    assert!(
        restored
            .peek(&q, None, now + Duration::from_secs(21), true)
            .is_none()
    );
    assert_eq!(
        restore(
            &Cache::new(CacheConfig::default()),
            encode(&cache, now + Duration::from_secs(30)),
            0,
            now
        )
        .unwrap()
        .restored,
        0
    );
}

#[test]
fn cname_negative_restore_preserves_partition_lifetime_and_cold_state() {
    for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
        let mut config = CacheConfig {
            shards: 1,
            ..CacheConfig::default()
        };
        config.stale.enabled = true;
        config.prefetch.enabled = true;
        config.prefetch.min_hits = 1;
        config.prefetch.remaining_percent = 100;
        let cache = Cache::new(config.clone());
        let q = query("alias.example.");
        let positive = query("positive.example.");
        let scope = Scope::ExactSource("192.0.2.0/24".parse().unwrap());
        let outgoing = Some("192.0.2.0/24".parse().unwrap());
        let inserted = Instant::now();
        let mut response = protocol::error_response(&q, code);
        for (owner, target, ttl) in [
            ("alias.example.", "bridge.example.", 80),
            ("bridge.example.", "missing.remote.test.", 60),
        ] {
            response.add_answer(DnsRecord::from_rdata(
                Name::from_ascii(owner).unwrap(),
                ttl,
                RData::CNAME(CNAME(Name::from_ascii(target).unwrap())),
            ));
        }
        response.add_authority(DnsRecord::from_rdata(
            Name::from_ascii("remote.test.").unwrap(),
            100,
            RData::SOA(SOA::new(
                Name::from_ascii("ns.remote.test.").unwrap(),
                Name::from_ascii("admin.remote.test.").unwrap(),
                1,
                60,
                60,
                3600,
                30,
            )),
        ));
        response.add_authority(DnsRecord::from_rdata(
            Name::from_ascii("remote.test.").unwrap(),
            70,
            RData::NS(NS(Name::from_ascii("ns.remote.test.").unwrap())),
        ));
        response.add_additional(DnsRecord::from_rdata(
            Name::from_ascii("ns.remote.test.").unwrap(),
            40,
            RData::A(A::new(192, 0, 2, 53)),
        ));
        assert!(cache.insert_if_epoch(&q, &response, scope, inserted, cache.epoch()));
        insert(
            &cache,
            &positive,
            &answer(&positive, 60),
            Scope::NoEcs,
            inserted,
        );
        for _ in 0..3 {
            assert!(!cache.lookup(&q, outgoing, inserted, false).unwrap().refresh);
        }
        assert_eq!(cache.snapshot()["hits"], 3);
        // Age 3.25s online (round up to 4s), then 2.1s offline (round up to 3s).
        let bytes = encode(&cache, inserted + Duration::from_millis(3250));
        let header: Header = serde_json::from_slice(&bytes[..HEADER_BYTES]).unwrap();
        assert_eq!(header.version, 3);
        let restored = Cache::new(config.clone());
        let now = inserted + Duration::from_secs(10);
        assert_eq!(
            restore(&restored, bytes.clone(), 2100, now)
                .unwrap()
                .restored,
            2
        );
        let counts = restored.snapshot();
        assert_eq!(counts["negative_entries"], 1);
        assert_eq!(counts["positive_entries"], 1);
        for field in ["hits", "stale_hits", "misses", "rejections", "evictions"] {
            assert_eq!(counts[field], 0, "{code:?}: {field}");
        }
        assert_eq!(restored.diagnostics_snapshot()["store"]["admitted"], 0);
        {
            let shard = restored.shards[0].lock().unwrap();
            let (_, entry) = shard.partitions[1].lru.iter().next().unwrap();
            assert!(entry.negative);
            assert_eq!(entry.hits.load(Ordering::Relaxed), 0);
            assert_eq!(entry.retention, entry.lifetime);
        }
        let hit = restored.peek(&q, outgoing, now, false).unwrap();
        assert_eq!(hit.scope, scope);
        assert!(!hit.stale && !hit.refresh);
        assert_eq!(hit.message.response_code, code);
        assert_eq!(hit.message.answers[0].ttl, 73);
        assert_eq!(hit.message.answers[1].ttl, 53);
        assert_eq!(hit.message.authorities[0].ttl, 23);
        assert_eq!(hit.message.authorities[1].ttl, 63);
        assert_eq!(hit.message.additionals[0].ttl, 33);
        let almost_expired = now + Duration::from_secs(22);
        assert!(
            !restored
                .lookup(&q, outgoing, almost_expired, false)
                .unwrap()
                .refresh
        );
        assert!(
            restored
                .peek(&q, outgoing, now + Duration::from_secs(23), true)
                .is_none()
        );
        let expired = Cache::new(config.clone());
        assert_eq!(
            restore(&expired, bytes.clone(), 30_000, now)
                .unwrap()
                .restored,
            1
        );
        assert_eq!(expired.snapshot()["negative_entries"], 0);
        // Current negative capacity remains authoritative; positive capacity
        // cannot absorb these entries during restore.
        let positive_only = Cache::new(CacheConfig {
            negative_percent: 0,
            ..config
        });
        let report = restore(&positive_only, bytes, 2100, now).unwrap();
        assert_eq!((report.restored, report.skipped), (1, 1));
        assert_eq!(positive_only.snapshot()["negative_entries"], 0);
        assert_eq!(positive_only.snapshot()["positive_entries"], 1);
    }
}

#[test]
fn ecs_network_privacy_and_no_ecs_are_distinct_after_restore() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("ecs.test.");
    let now = Instant::now();
    for scope in [
        Scope::NoEcs,
        Scope::Privacy { ipv4: true },
        Scope::Privacy { ipv4: false },
        Scope::Network("192.0.2.0/24".parse().unwrap()),
        Scope::Network("2001:db8::/32".parse().unwrap()),
    ] {
        insert(&cache, &q, &answer(&q, 60), scope, now);
    }
    let restored = Cache::new(CacheConfig::default());
    assert_eq!(
        restore(&restored, encode(&cache, now), 0, now)
            .unwrap()
            .restored,
        5
    );
    for outgoing in [
        None,
        Some("0.0.0.0/0".parse().unwrap()),
        Some("::/0".parse().unwrap()),
        Some("192.0.2.0/24".parse().unwrap()),
        Some("2001:db8:1::/48".parse().unwrap()),
    ] {
        assert!(restored.peek(&q, outgoing, now, false).is_some());
    }
    assert!(
        restored
            .peek(&q, Some("198.51.100.0/24".parse().unwrap()), now, false)
            .is_none()
    );
}

#[test]
fn checksum_policy_version_clock_and_wire_fail_before_any_import() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("integrity.test.");
    let now = Instant::now();
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let bytes = encode(&cache, now);
    for change in [
        "checksum",
        "fingerprint",
        "version",
        "saved_at_ns",
        "records",
    ] {
        let mut invalid = bytes.clone();
        let mut header: serde_json::Value =
            serde_json::from_slice(&invalid[..HEADER_BYTES]).unwrap();
        header[change] = match change {
            "checksum" | "fingerprint" => serde_json::json!("b".repeat(64)),
            "version" => serde_json::json!(999),
            "saved_at_ns" => serde_json::json!(1_000_000_000_001u64),
            _ => serde_json::json!(0),
        };
        invalid[..HEADER_BYTES].fill(b' ');
        let encoded = serde_json::to_vec(&header).unwrap();
        invalid[..encoded.len()].copy_from_slice(&encoded);
        let restored = Cache::new(CacheConfig::default());
        assert!(restore(&restored, invalid, 0, now).is_err(), "{change}");
        assert_eq!(restored.snapshot()["entries"], 0);
    }
    // Even changing the timestamp backwards while retaining a syntactically
    // valid header is covered by the digest and cannot extend TTLs.
    let mut invalid = bytes;
    let mut header: Header = serde_json::from_slice(&invalid[..HEADER_BYTES]).unwrap();
    header.saved_at_ns -= 1_000_000_000;
    invalid[..HEADER_BYTES].fill(b' ');
    let encoded = serde_json::to_vec(&header).unwrap();
    invalid[..encoded.len()].copy_from_slice(&encoded);
    assert!(restore(&Cache::new(CacheConfig::default()), invalid, 0, now).is_err());
}

#[test]
fn expired_stale_entries_and_ttl_zero_supersession_never_reappear() {
    let mut config = CacheConfig::default();
    config.stale.enabled = true;
    let cache = Cache::new(config.clone());
    let now = Instant::now();
    let q = query("stale.test.");
    insert(&cache, &q, &answer(&q, 5), Scope::NoEcs, now);
    assert!(
        cache
            .peek(&q, None, now + Duration::from_secs(6), true)
            .is_some()
    );
    let restored = Cache::new(config.clone());
    assert_eq!(
        restore(
            &restored,
            encode(&cache, now + Duration::from_secs(6)),
            0,
            now
        )
        .unwrap()
        .restored,
        0
    );
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    insert(&cache, &q, &answer(&q, 0), Scope::NoEcs, now);
    assert_eq!(
        restore(&Cache::new(config), encode(&cache, now), 0, now)
            .unwrap()
            .restored,
        0
    );
}

#[test]
fn restore_respects_current_policy_capacity_variants_and_zero_heat() {
    let config = CacheConfig {
        shards: 1,
        ..CacheConfig::default()
    };
    let cache = Cache::new(config.clone());
    let now = Instant::now();
    for i in 1..=6 {
        let q = query(&format!("entry{i}.test."));
        insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    }
    let bytes = encode(&cache, now);
    let limited = Cache::new(CacheConfig {
        max_entries: 2,
        negative_percent: 0,
        ..config.clone()
    });
    let report = restore(&limited, bytes.clone(), 0, now).unwrap();
    assert_eq!((report.restored, report.skipped), (2, 4));
    assert!(
        limited
            .peek(&query("entry6.test."), None, now, false)
            .is_some()
    );
    assert_eq!(limited.snapshot()["evictions"], 0);
    let disabled = Cache::new(CacheConfig {
        enabled: false,
        ..config.clone()
    });
    assert_eq!(
        restore(&disabled, bytes.clone(), 0, now).unwrap().restored,
        0
    );
    let tiny = Cache::new(CacheConfig {
        max_bytes: 1,
        ..config
    });
    assert_eq!(restore(&tiny, bytes, 0, now).unwrap().restored, 0);
}

#[test]
fn byte_budget_cancel_and_write_failure_are_explicit() {
    let cache = Cache::new(CacheConfig::default());
    let now = Instant::now();
    let q = query("budget.test.");
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let mut bytes = Cursor::new(Vec::new());
    let report = cache
        .write_clean_snapshot(
            &mut bytes,
            FP,
            SystemTime::now(),
            now,
            HEADER_BYTES,
            far(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (report.saved, report.skipped, report.bytes),
        (0, 1, HEADER_BYTES as u64)
    );
    assert!(
        cache
            .write_clean_snapshot(
                &mut bytes,
                FP,
                SystemTime::now(),
                now,
                HEADER_BYTES,
                far(),
                &AtomicBool::new(true)
            )
            .is_err()
    );
    struct Full(Cursor<Vec<u8>>);
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Full {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(position)
        }
    }
    assert!(
        cache
            .write_clean_snapshot(
                &mut Full(Cursor::new(Vec::new())),
                FP,
                SystemTime::now(),
                now,
                1_048_576,
                far(),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .to_string()
            .contains("disk full")
    );
}

#[test]
fn semantic_fingerprint_excludes_storage_and_includes_live_dns_policy() {
    let mut config =
        crate::config::Config::parse(include_str!("../../../parins.example.toml")).unwrap();
    let original = semantic_fingerprint(&config, b"filter-a").unwrap();
    config.cache.persistence.enabled = !config.cache.persistence.enabled;
    assert_eq!(
        original,
        semantic_fingerprint(&config, b"filter-a").unwrap()
    );
    assert_ne!(
        original,
        semantic_fingerprint(&config, b"filter-b").unwrap()
    );
    config.ecs.enabled = !config.ecs.enabled;
    let ecs_changed = semantic_fingerprint(&config, b"filter-a").unwrap();
    assert_ne!(original, ecs_changed);
    // Forwarded and recursive answers never share a snapshot.
    config.resolution.mode = crate::config::ResolutionMode::Recursive;
    assert_ne!(
        ecs_changed,
        semantic_fingerprint(&config, b"filter-a").unwrap()
    );
}

#[test]
fn prior_cache_semantics_are_rejected_without_changing_snapshot_format() {
    let config =
        crate::config::Config::parse(include_str!("../../../parins.example.toml")).unwrap();
    let fingerprint = semantic_fingerprint(&config, b"filter-a").unwrap();
    // Reconstruct the previous semantic contract, with otherwise identical
    // effective configuration and a fully valid snapshot/checksum.
    let mut cache_policy = serde_json::to_value(&config.cache).unwrap();
    cache_policy.as_object_mut().unwrap().remove("persistence");
    let prior = serde_json::json!({"semantics": 3, "cache": cache_policy, "ecs": config.ecs,
        "upstreams": config.upstreams, "filter_digest": STANDARD.encode(b"filter-a")});
    let prior_fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&prior).unwrap()));
    assert_ne!(fingerprint, prior_fingerprint);
    assert_eq!(VERSION, 3);
    let cache = Cache::new(config.cache.clone());
    let q = query("prior-semantics.test.");
    let now = Instant::now();
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let mut bytes = Cursor::new(Vec::new());
    let saved_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    cache
        .write_clean_snapshot(
            &mut bytes,
            &prior_fingerprint,
            saved_at,
            now,
            1_048_576,
            far(),
            &AtomicBool::new(false),
        )
        .unwrap();
    let restored = Cache::new(config.cache);
    let error = restored
        .restore_clean_snapshot(
            &mut bytes,
            &fingerprint,
            saved_at,
            now,
            1_048_576,
            far(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("snapshot policy mismatch"));
    assert_eq!(restored.snapshot()["entries"], 0);
}

#[test]
fn names_without_presentation_round_trip_restore_with_positive_and_negative_entries() {
    let long = |n: usize| vec![b'a'; n];
    let names: Vec<Name> = [
        vec![
            b"https://ac".to_vec(),
            b"example".to_vec(),
            b"test".to_vec(),
        ],
        vec![br"back\slash".to_vec(), b"test".to_vec()],
        vec![b"with space".to_vec(), b"test".to_vec()],
        vec![b"dot.inside".to_vec(), b"test".to_vec()],
        vec![b"\xff\x00bin".to_vec(), b"test".to_vec()],
        vec![b"UPPER".to_vec(), b"Test".to_vec()],
        vec![long(63), b"test".to_vec()],
        // 3 * 64 + 57 + 5 + 1 = 255 wire bytes, the maximum.
        vec![long(63), long(63), long(63), long(56), b"test".to_vec()],
    ]
    .into_iter()
    .map(|labels| Name::from_labels(labels).unwrap())
    .collect();
    assert_eq!(names.last().unwrap().to_bytes().unwrap().len(), 255);
    let question = |name: &Name, kind| {
        let mut query = Message::new(1, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(name.clone(), kind));
        query
    };
    let cache = Cache::new(CacheConfig::default());
    let now = Instant::now();
    for name in &names {
        let positive = question(name, RecordType::A);
        insert(&cache, &positive, &answer(&positive, 60), Scope::NoEcs, now);
        let negative = question(name, RecordType::AAAA);
        let mut response = protocol::error_response(&negative, ResponseCode::NXDomain);
        response.add_authority(DnsRecord::from_rdata(
            Name::from_ascii("test.").unwrap(),
            100,
            RData::SOA(SOA::new(
                Name::from_ascii("ns.test.").unwrap(),
                Name::from_ascii("admin.test.").unwrap(),
                1,
                60,
                60,
                3600,
                30,
            )),
        ));
        insert(&cache, &negative, &response, Scope::NoEcs, now);
    }
    assert_eq!(cache.snapshot()["entries"], 2 * names.len());
    let restored = Cache::new(CacheConfig::default());
    let report = restore(&restored, encode(&cache, now), 0, now).unwrap();
    assert_eq!((report.restored, report.reason), (2 * names.len(), None));
    for name in &names {
        let positive = restored
            .peek(&question(name, RecordType::A), None, now, false)
            .unwrap()
            .message;
        assert_eq!(positive.answers[0].data, RData::A(A::new(192, 0, 2, 1)));
        let negative = restored
            .peek(&question(name, RecordType::AAAA), None, now, false)
            .unwrap()
            .message;
        assert_eq!(negative.response_code, ResponseCode::NXDomain);
        assert_eq!(
            restored.inspect(name, None, now)["variants"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    assert_eq!(restored.invalidate(Some(&names[0]), None, None), 2);
}

#[test]
fn restore_rejects_noncanonical_wire_names() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("question.test.");
    let now = Instant::now();
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let original = encode(&cache, now);
    let mut trailing = Name::from_ascii("question.test.")
        .unwrap()
        .to_bytes()
        .unwrap();
    trailing.push(0);
    for name in [
        "question.test.".to_string(),
        STANDARD.encode(
            Name::from_ascii("Question.test.")
                .unwrap()
                .to_bytes()
                .unwrap(),
        ),
        STANDARD.encode(trailing),
    ] {
        let mut header: Header = serde_json::from_slice(&original[..HEADER_BYTES]).unwrap();
        let mut record: Record = serde_json::from_slice(&original[HEADER_BYTES..]).unwrap();
        record.key.name = name;
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        let mut hash = Sha256::new();
        hash.update(&line);
        header.checksum = digest(hash, &header);
        let encoded = serde_json::to_vec(&header).unwrap();
        let mut bytes = vec![b' '; HEADER_BYTES];
        bytes[..encoded.len()].copy_from_slice(&encoded);
        bytes.extend(line);
        let restored = Cache::new(CacheConfig::default());
        assert!(restore(&restored, bytes, 0, now).is_err());
        assert_eq!(restored.snapshot()["entries"], 0);
    }
}

#[test]
fn restore_rejects_malformed_scope_and_question_even_with_recomputed_checksum() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("question.test.");
    let now = Instant::now();
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let original = encode(&cache, now);
    for bad_scope in [true, false] {
        let mut header: Header = serde_json::from_slice(&original[..HEADER_BYTES]).unwrap();
        let mut record: Record = serde_json::from_slice(&original[HEADER_BYTES..]).unwrap();
        if bad_scope {
            record.scope = SavedScope::Network("192.0.2.1/24".into());
        } else {
            record.key.name = STANDARD.encode(
                Name::from_ascii("another.test.")
                    .unwrap()
                    .to_bytes()
                    .unwrap(),
            );
        }
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        let mut hash = Sha256::new();
        hash.update(&line);
        header.checksum = digest(hash, &header);
        let encoded = serde_json::to_vec(&header).unwrap();
        let mut bytes = vec![b' '; HEADER_BYTES];
        bytes[..encoded.len()].copy_from_slice(&encoded);
        bytes.extend(line);
        let restored = Cache::new(CacheConfig::default());
        assert!(restore(&restored, bytes, 0, now).is_err());
        assert_eq!(restored.snapshot()["entries"], 0);
    }
}

#[test]
fn a_record_that_does_not_decode_unloads_what_the_restore_admitted() {
    let config = CacheConfig {
        shards: 1,
        ..CacheConfig::default()
    };
    let cache = Cache::new(config.clone());
    let now = Instant::now();
    for name in ["first.test.", "second.test."] {
        let q = query(name);
        insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    }
    let original = encode(&cache, now);
    let mut header: Header = serde_json::from_slice(&original[..HEADER_BYTES]).unwrap();
    let mut lines: Vec<Vec<u8>> = original[HEADER_BYTES..]
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    // The second record names a scope this codec never writes.
    let mut record: Record = serde_json::from_slice(&lines[1]).unwrap();
    record.scope = SavedScope::Network("192.0.2.1/24".into());
    lines[1] = serde_json::to_vec(&record).unwrap();
    lines[1].push(b'\n');
    let mut hash = Sha256::new();
    for line in &lines {
        hash.update(line);
    }
    header.checksum = digest(hash, &header);
    let encoded = serde_json::to_vec(&header).unwrap();
    let mut bytes = vec![b' '; HEADER_BYTES];
    bytes[..encoded.len()].copy_from_slice(&encoded);
    bytes.extend(lines.concat());
    let restored = Cache::new(config);
    assert!(restore(&restored, bytes, 0, now).is_err());
    assert_eq!(restored.snapshot()["entries"], 0);
    assert_eq!(restored.snapshot()["bytes"], 0);
}

#[test]
fn current_variants_negative_partition_and_admission_remain_authoritative() {
    let config = CacheConfig {
        shards: 1,
        ..CacheConfig::default()
    };
    let cache = Cache::new(config.clone());
    let now = Instant::now();
    let q = query("variants.test.");
    for scope in ["192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24"] {
        insert(
            &cache,
            &q,
            &answer(&q, 60),
            Scope::Network(scope.parse().unwrap()),
            now,
        );
    }
    let restored = Cache::new(CacheConfig {
        max_variants: 1,
        ..config
    });
    let report = restore(&restored, encode(&cache, now), 0, now).unwrap();
    assert_eq!((report.restored, report.skipped), (1, 2));
    assert_eq!(restored.snapshot()["rejections"], 0);
    assert_eq!(restored.snapshot()["evictions"], 0);
}

#[test]
fn full_stream_bounds_and_read_faults_are_enforced() {
    let cache = Cache::new(CacheConfig::default());
    let q = query("bounds.test.");
    let now = Instant::now();
    insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    let original = encode(&cache, now);
    let mut header: Header = serde_json::from_slice(&original[..HEADER_BYTES]).unwrap();
    header.records = MAX_RECORDS + 1;
    let json = serde_json::to_vec(&header).unwrap();
    let mut bytes = vec![b' '; HEADER_BYTES];
    bytes[..json.len()].copy_from_slice(&json);
    assert!(
        restore(&Cache::new(CacheConfig::default()), bytes, 0, now)
            .unwrap_err()
            .to_string()
            .contains("record limit")
    );
    struct FailingRead(Cursor<Vec<u8>>);
    impl Read for FailingRead {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() >= HEADER_BYTES as u64 {
                Err(std::io::Error::other("injected IO fault"))
            } else {
                self.0.read(output)
            }
        }
    }
    impl Seek for FailingRead {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(position)
        }
    }
    let restored = Cache::new(CacheConfig::default());
    let error = restored
        .restore_clean_snapshot(
            &mut FailingRead(Cursor::new(original)),
            FP,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            now,
            1_048_576,
            far(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("injected IO fault"));
    assert_eq!(restored.snapshot()["entries"], 0);
}

#[test]
fn exact_source_without_client_edns_round_trips_and_prior_format_is_rejected() {
    let cache = Cache::new(CacheConfig::default());
    let query = query("exact-snapshot.test.");
    assert!(query.edns.is_none());
    let now = Instant::now();
    let scope = Scope::ExactSource("192.0.2.0/24".parse().unwrap());
    insert(&cache, &query, &answer(&query, 60), scope, now);
    let bytes = encode(&cache, now);
    let restored = Cache::new(CacheConfig::default());
    let report = restore(&restored, bytes.clone(), 1000, now).unwrap();
    assert_eq!(report.restored, 1);
    let hit = restored
        .peek(&query, Some("192.0.2.0/24".parse().unwrap()), now, false)
        .unwrap();
    assert_eq!(hit.scope, scope);
    assert!(hit.message.edns.is_none());
    assert_eq!(hit.message.answers[0].ttl, 59);
    assert!(
        restored
            .peek(&query, Some("192.0.2.0/25".parse().unwrap()), now, false)
            .is_none()
    );
    assert_eq!(restored.diagnostics_snapshot()["store"]["admitted"], 0);
    assert!(
        SavedScope::ExactSource("0.0.0.0/0".into())
            .decode()
            .is_err()
    );
    assert!(
        SavedScope::ExactSource("192.0.2.1/24".into())
            .decode()
            .is_err()
    );
    let mut legacy = bytes;
    let mut header: Header = serde_json::from_slice(&legacy[..HEADER_BYTES]).unwrap();
    header.version = 2;
    let encoded = serde_json::to_vec(&header).unwrap();
    legacy[..HEADER_BYTES].fill(b' ');
    legacy[..encoded.len()].copy_from_slice(&encoded);
    assert!(
        restore(&Cache::new(CacheConfig::default()), legacy, 0, now)
            .unwrap_err()
            .to_string()
            .contains("version")
    );
}

#[test]
fn learned_entries_round_trip_without_restoring_classification() {
    let mut config = CacheConfig::default();
    config.ecs_sharing.enabled = true;
    let cache = Cache::new(config.clone());
    let q = query("learned.test.");
    let now = Instant::now();
    for network in ["192.0.2.0/24", "192.0.3.0/24", "198.51.100.0/24"] {
        insert(
            &cache,
            &q,
            &answer(&q, 60),
            Scope::ExactSource(network.parse().unwrap()),
            now,
        );
    }
    let v6 = Scope::Learned { ipv4: false };
    let q6 = query("learned6.test.");
    cache.admit(
        &q6,
        &answer(&q6, 60),
        v6,
        now,
        cache.epoch(),
        Some(Restore {
            remaining: 60,
            accessed: 1,
        }),
    );
    assert_eq!(cache.snapshot()["ecs_shared_entries"], 2);
    let bytes = encode(&cache, now);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains(r#""namespace":"learned_v4""#));
    assert!(text.contains(r#""namespace":"learned_v6""#));
    let restored = Cache::new(config);
    assert_eq!(restore(&restored, bytes, 0, now).unwrap().restored, 2);
    let outgoing = Some("203.0.113.0/24".parse().unwrap());
    assert_eq!(
        restored.peek(&q, outgoing, now, false).unwrap().scope,
        Scope::Learned { ipv4: true }
    );
    assert!(restored.peek(&q, None, now, false).is_none());
    assert_eq!(restored.snapshot()["ecs_shared_entries"], 2);
    // Classification is not persisted: the name starts unobserved, and the
    // first per-network answer supersedes the restored learned copy.
    let inspection = restored.inspect(&Name::from_ascii("learned.test.").unwrap(), None, now);
    assert_eq!(inspection["ecs_sharing"], Value::Null);
    insert(
        &restored,
        &q,
        &answer(&q, 60),
        Scope::ExactSource("203.0.113.0/24".parse().unwrap()),
        now,
    );
    assert!(restored.peek(&q, outgoing, now, false).is_some());
    assert!(
        restored
            .peek(&q, Some("192.0.2.0/24".parse().unwrap()), now, false)
            .is_none()
    );
    assert_eq!(restored.snapshot()["ecs_shared_entries"], 1);
}

/// A restore keeps the first records that fit, and a new process hashes keys
/// into other shards, so the file lists records by recency across all shards.
#[test]
fn snapshot_lists_records_most_recently_used_first_across_shards() {
    let cache = Cache::new(CacheConfig::default());
    let now = Instant::now();
    for index in 0..16 {
        let q = query(&format!("n{index}.test."));
        insert(&cache, &q, &answer(&q, 60), Scope::NoEcs, now);
    }
    // Later shards hold the more recently used entries: shard order is wrong.
    let mut used = HashMap::new();
    let mut populated = 0;
    for shard in &cache.shards {
        let shard = shard.lock().unwrap();
        populated += usize::from(!shard.partitions[0].lru.is_empty());
        for (_, entry) in shard.partitions[0].lru.iter() {
            let clock = used.len() as u64 + 1;
            entry.accessed.store(clock, Ordering::Relaxed);
            used.insert(entry.key.name.clone(), clock);
        }
    }
    assert!(populated > 1, "the keys span several shards");
    let bytes = encode(&cache, now);
    let order: Vec<u64> = bytes[HEADER_BYTES..]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (query, _, _) = serde_json::from_slice::<Record>(line)
                .unwrap()
                .decode()
                .unwrap();
            used[&key_name(query.queries[0].name())]
        })
        .collect();
    assert_eq!(order.len(), 16);
    assert!(order.windows(2).all(|pair| pair[0] > pair[1]), "{order:?}");
}

/// Sleeps on every write, so a deadline stops the save part-way.
struct SlowWriter(Cursor<Vec<u8>>);
impl Write for SlowWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(Duration::from_millis(20));
        self.0.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for SlowWriter {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(position)
    }
}

/// Fast while authenticating; once the restore seeks back to the first record
/// to admit entries, reads return 64 bytes at a time and sleep.
struct SlowAdmission {
    inner: Cursor<Vec<u8>>,
    slow: bool,
}
impl Read for SlowAdmission {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.slow {
            return self.inner.read(buffer);
        }
        std::thread::sleep(Duration::from_millis(20));
        let length = buffer.len().min(64);
        self.inner.read(&mut buffer[..length])
    }
}
impl Seek for SlowAdmission {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.slow |= position == SeekFrom::Start(HEADER_BYTES as u64);
        self.inner.seek(position)
    }
}

/// Twenty entries in one shard, where use order is exact; the last inserted is
/// the most recently used.
fn twenty(now: Instant) -> (Cache, Vec<Message>) {
    let cache = Cache::new(CacheConfig {
        shards: 1,
        ..CacheConfig::default()
    });
    let queries: Vec<_> = (0..20)
        .map(|i| query(&format!("n{i}.deadline.test.")))
        .collect();
    for q in &queries {
        insert(&cache, q, &answer(q, 60), Scope::NoEcs, now);
    }
    (cache, queries)
}

#[test]
fn restored_entries_keep_their_use_order_for_eviction_and_the_next_save() {
    let now = Instant::now();
    let (cache, queries) = twenty(now);
    let bytes = encode(&cache, now);
    let restored = Cache::new(CacheConfig {
        shards: 1,
        max_entries: 20,
        negative_percent: 0,
        ..CacheConfig::default()
    });
    assert_eq!(
        restore(&restored, bytes.clone(), 0, now).unwrap().restored,
        20
    );
    // The next save lists the restored entries in the order they were saved.
    assert!(encode(&restored, now)[HEADER_BYTES..] == bytes[HEADER_BYTES..]);
    // New answers evict the least recently used restored entries first.
    for i in 0..19 {
        let q = query(&format!("new{i}.test."));
        insert(&restored, &q, &answer(&q, 60), Scope::NoEcs, now);
    }
    assert!(restored.peek(&queries[19], None, now, false).is_some());
    assert!(restored.peek(&queries[18], None, now, false).is_none());
}

#[test]
fn a_save_that_reaches_its_deadline_publishes_the_most_recent_entries_completely() {
    let now = Instant::now();
    let (cache, queries) = twenty(now);
    let mut writer = SlowWriter(Cursor::new(Vec::new()));
    let report = cache
        .write_clean_snapshot(
            &mut writer,
            FP,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            now,
            1_048_576,
            Instant::now() + Duration::from_millis(150),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(report.reason.as_deref(), Some("deadline"));
    assert!(report.saved < 20, "{report:?}");
    assert_eq!(report.saved + report.skipped, 20);
    // The partial snapshot is complete: every written record restores.
    let restored = Cache::new(CacheConfig::default());
    let read = restore(&restored, writer.0.into_inner(), 0, now).unwrap();
    assert_eq!((read.restored, read.reason), (report.saved, None));
    if report.saved > 0 {
        assert!(restored.peek(&queries[19], None, now, false).is_some());
    }
}

#[test]
fn a_restore_that_reaches_its_deadline_keeps_the_entries_it_admitted() {
    let now = Instant::now();
    let (cache, queries) = twenty(now);
    let restored = Cache::new(CacheConfig::default());
    let mut reader = SlowAdmission {
        inner: Cursor::new(encode(&cache, now)),
        slow: false,
    };
    let report = restored
        .restore_clean_snapshot(
            &mut reader,
            FP,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            now,
            1_048_576,
            Instant::now() + Duration::from_millis(300),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(report.reason.as_deref(), Some("deadline"));
    assert!(report.restored < 20, "{report:?}");
    assert_eq!(report.restored + report.skipped, 20);
    assert_eq!(restored.snapshot()["entries"], report.restored);
    if report.restored > 0 {
        assert!(restored.peek(&queries[19], None, now, false).is_some());
    }
}

#[test]
fn a_restore_out_of_time_while_authenticating_admits_nothing() {
    let now = Instant::now();
    let (cache, _) = twenty(now);
    let restored = Cache::new(CacheConfig::default());
    let report = restored
        .restore_clean_snapshot(
            &mut Cursor::new(encode(&cache, now)),
            FP,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            now,
            1_048_576,
            Instant::now(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (report.restored, report.skipped, report.reason.as_deref()),
        (0, 20, Some("deadline"))
    );
    assert_eq!(restored.snapshot()["entries"], 0);
}
