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
fn subnets_are_independent_and_longest_matching_scope_wins() {
    let mut cache = Cache::new(CacheConfig::default());
    let q = query("Example.Test.");
    let now = Instant::now();
    cache.insert(&q, &answer(&q, 1, 60), network("192.0.0.0/16"), now);
    cache.insert(&q, &answer(&q, 2, 60), network("192.0.2.0/24"), now);
    cache.insert(&q, &answer(&q, 3, 60), network("198.51.100.0/24"), now);
    for (subnet, expected) in [
        ("192.0.2.0/24", 2),
        ("192.0.3.0/24", 1),
        ("198.51.100.0/24", 3),
    ] {
        let (r, _) = cache.get(&q, ecs(subnet), now).unwrap();
        assert_eq!(r.answers[0].data, RData::A(A::new(192, 0, 2, expected)));
    }
    assert!(cache.get(&q, ecs("203.0.113.0/24"), now).is_none());
    assert_eq!(cache.entries, 3);
}

#[test]
fn no_ecs_family_and_privacy_namespaces_are_isolated() {
    let mut cache = Cache::new(CacheConfig::default());
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
    let mut cache = Cache::new(CacheConfig::default());
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
    assert_eq!((cache.entries, cache.bytes), (0, 0));
}

#[test]
fn soa_controls_nxdomain_and_nodata_lifetime() {
    for code in [ResponseCode::NXDomain, ResponseCode::NoError] {
        let mut cache = Cache::new(CacheConfig::default());
        let q = query("example.test.");
        let now = Instant::now();
        let mut response = protocol::error_response(&q, code);
        cache.insert(&q, &response, Scope::NoEcs, now);
        assert_eq!(cache.entries, 0);
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
        assert_eq!(cache.entries, 0);
    }
}

#[test]
fn error_zero_ttl_and_client_specific_edns_are_not_cached() {
    let mut cache = Cache::new(CacheConfig::default());
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
    assert_eq!((cache.entries, cache.bytes), (0, 0));
}

#[test]
fn semantic_flags_and_edns_presence_do_not_share_entries() {
    let mut cache = Cache::new(CacheConfig::default());
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
        max_entries: 2,
        max_variants: 2,
        max_bytes: 512,
        ..Default::default()
    };
    let mut cache = Cache::new(cfg);
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
        assert!(cache.entries <= 2 && cache.bytes <= 512);
    }
    assert!(cache.get(&a, ecs("192.0.1.0/24"), now).is_none());
    assert!(cache.get(&a, ecs("192.0.3.0/24"), now).is_some());
    let mut large = answer(&a, 1, 60);
    large.answers = vec![large.answers[0].clone(); 100];
    cache.insert(&a, &large, Scope::NoEcs, now);
    assert!(cache.bytes <= 512);
    let actual: usize = cache
        .buckets
        .iter()
        .flat_map(|(_, bucket)| bucket)
        .map(|entry| entry.charge)
        .sum();
    assert_eq!(cache.bytes, actual);
}
