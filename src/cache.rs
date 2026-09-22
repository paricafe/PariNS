//! Concurrent bounded DNS cache. Shards own variant-level LRU partitions; immutable
//! wire entries are decoded after releasing locks. No network IO belongs here.

use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Edns, Message, ResponseCode},
    rr::{
        DNSClass, Name, RData, RecordType,
        rdata::opt::{ClientSubnet, EdnsCode},
    },
};
use lru::LruCache;
use serde_json::{Value, json};

use crate::{
    config::CacheConfig,
    ecs::Scope,
    protocol::{self, MAX_UDP_PAYLOAD},
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    name: Box<str>,
    kind: RecordType,
    class: DNSClass,
    dnssec_ok: bool,
    checking_disabled: bool,
    recursion_desired: bool,
    edns: bool,
}

impl Key {
    fn of(query: &Message) -> Option<Self> {
        let question = query.queries.first()?;
        if query.queries.len() != 1 || question.query_class() != DNSClass::IN || !plain_edns(query)
        {
            return None;
        }
        let mut name = question.name().to_lowercase();
        name.set_fqdn(true);
        Some(Self {
            name: name.to_ascii().into_boxed_str(),
            kind: question.query_type(),
            class: question.query_class(),
            dnssec_ok: query
                .edns
                .as_ref()
                .is_some_and(|edns| edns.flags().dnssec_ok),
            checking_disabled: query.checking_disabled,
            recursion_desired: query.recursion_desired,
            edns: query.edns.is_some(),
        })
    }
}

fn plain_edns(message: &Message) -> bool {
    message.edns.as_ref().is_none_or(|edns| {
        edns.version() == 0
            && edns
                .options()
                .options
                .iter()
                .all(|(code, _)| *code == EdnsCode::Subnet)
    })
}

fn superseding_edns(message: &Message) -> bool {
    message.edns.as_ref().is_none_or(|edns| {
        edns.version() == 0
            && edns.options().options.iter().all(|(code, _)| {
                // EDE (RFC 8914) is diagnostic information, not a transaction-
                // specific option. It can supersede old knowledge without being
                // admitted for later replay by plain_edns / prepare_response.
                matches!(code, EdnsCode::Subnet | EdnsCode::Unknown(15))
            })
    })
}

struct Entry {
    id: u64,
    key: Arc<Key>,
    wire: Box<[u8]>,
    scope: Scope,
    inserted: Instant,
    lifetime: Duration,
    retention: Duration,
    negative: bool,
    prefetch: bool,
    hits: AtomicU64,
    accessed: AtomicU64,
    charge: usize,
    charged: Arc<AtomicUsize>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.charged.fetch_sub(self.charge, Ordering::Relaxed);
    }
}

struct Partition {
    lru: LruCache<u64, Arc<Entry>>,
    charged: Arc<AtomicUsize>,
    max_entries: usize,
    max_bytes: usize,
}

impl Partition {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            // Admission below enforces both caps. Avoid eagerly allocating a hash
            // table from max_entries when the byte budget only fits a few entries.
            lru: LruCache::unbounded(),
            charged: Arc::new(AtomicUsize::new(0)),
            max_entries,
            max_bytes,
        }
    }
}

#[derive(Default)]
struct Counts {
    hits: u64,
    stale_hits: u64,
    misses: u64,
    evictions: u64,
    rejections: u64,
}

struct Shard {
    buckets: HashMap<Arc<Key>, Vec<Arc<Entry>>>,
    partitions: [Partition; 2],
    clock: u64,
    counts: Counts,
}

impl Shard {
    fn remove(&mut self, part: usize, id: u64) {
        if let Some(entry) = self.partitions[part].lru.pop(&id) {
            let empty = if let Some(ids) = self.buckets.get_mut(&entry.key) {
                ids.retain(|entry| entry.id != id);
                ids.is_empty()
            } else {
                false
            };
            if empty {
                self.buckets.remove(&entry.key);
            }
        }
    }

    fn prune(&mut self, key: &Key, now: Instant) {
        let expired: Vec<_> = self
            .buckets
            .get(key)
            .into_iter()
            .flatten()
            .filter(|e| now.saturating_duration_since(e.inserted) >= e.retention)
            .map(|e| (usize::from(e.negative), e.id))
            .collect();
        for (part, id) in expired {
            self.remove(part, id);
        }
    }

    fn select(
        &self,
        key: &Key,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> Option<(usize, u64)> {
        self.buckets
            .get(key)?
            .iter()
            .filter_map(|entry| {
                let age = now.saturating_duration_since(entry.inserted);
                let fresh = age < entry.lifetime;
                (entry.scope.matches(outgoing) && age < entry.retention && (fresh || allow_stale))
                    .then_some((
                        (fresh, entry.scope.prefix_len()),
                        (usize::from(entry.negative), entry.id),
                    ))
            })
            .max_by_key(|(rank, _)| *rank)
            .map(|(_, id)| id)
    }
}

struct Rule {
    name: Name,
    qtype: Option<RecordType>,
    index: usize,
}

#[derive(serde::Serialize)]
struct Policy {
    rule_index: Option<usize>,
    bypass: bool,
    max_ttl_secs: u32,
    negative_ttl_cap_secs: u32,
    prefetch: bool,
    stale: bool,
}

pub struct Hit {
    pub message: Message,
    pub scope: Scope,
    pub stale: bool,
    pub refresh: bool,
}

pub struct Cache {
    config: CacheConfig,
    shards: Vec<Mutex<Shard>>,
    hasher: RandomState,
    rules: Vec<Rule>,
    epoch: AtomicU64,
    bypasses: AtomicU64,
}

impl Cache {
    /// Config values are validated by Config before the server is bound.
    pub fn new(config: CacheConfig) -> Self {
        let negative_entries = config.max_entries * usize::from(config.negative_percent) / 100;
        let positive_entries = config.max_entries - negative_entries;
        let negative_bytes = if negative_entries == 0 {
            0
        } else {
            config.max_bytes * usize::from(config.negative_percent) / 100
        };
        let positive_bytes = config.max_bytes - negative_bytes;
        let count = config
            .shards
            .min(positive_entries.max(1))
            .min(if negative_entries == 0 {
                usize::MAX
            } else {
                negative_entries
            })
            .min((positive_bytes / 1024).max(1))
            .min(if negative_entries == 0 {
                usize::MAX
            } else {
                (negative_bytes / 1024).max(1)
            })
            .max(1);
        let portion = |total: usize, i: usize| total / count + usize::from(i < total % count);
        let shards = (0..count)
            .map(|i| {
                Mutex::new(Shard {
                    buckets: HashMap::new(),
                    partitions: [
                        Partition::new(portion(positive_entries, i), portion(positive_bytes, i)),
                        Partition::new(portion(negative_entries, i), portion(negative_bytes, i)),
                    ],
                    clock: 0,
                    counts: Counts::default(),
                })
            })
            .collect();
        let mut rules: Vec<_> = config
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| Rule {
                name: Name::from_ascii(&rule.name)
                    .expect("validated cache rule name")
                    .to_lowercase(),
                qtype: rule.qtype.as_ref().and_then(|q| q.parse().ok()),
                index,
            })
            .collect();
        rules.sort_by_key(|r| {
            (
                config.rules[r.index].suffix,
                std::cmp::Reverse(r.name.iter().count()),
                r.qtype.is_none(),
                r.index,
            )
        });
        Self {
            config,
            shards,
            hasher: RandomState::new(),
            rules,
            epoch: AtomicU64::new(0),
            bypasses: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn shard(&self, key: &Key) -> usize {
        self.hasher.hash_one(key) as usize % self.shards.len()
    }

    fn policy(&self, key: &Key, name: &Name) -> Policy {
        let matched = self.rules.iter().find(|r| {
            r.qtype.is_none_or(|kind| kind == key.kind)
                && r.name.zone_of(name)
                && (self.config.rules[r.index].suffix
                    || r.name.iter().count() == name.iter().count())
        });
        let rule = matched.map(|r| &self.config.rules[r.index]);
        Policy {
            rule_index: matched.map(|r| r.index),
            bypass: !self.config.enabled || rule.is_some_and(|r| r.bypass),
            max_ttl_secs: rule
                .and_then(|r| r.max_ttl_secs)
                .unwrap_or(self.config.max_ttl_secs),
            negative_ttl_cap_secs: rule
                .and_then(|r| r.negative_ttl_cap_secs)
                .unwrap_or(self.config.negative_ttl_cap_secs),
            prefetch: rule
                .and_then(|r| r.prefetch)
                .unwrap_or(self.config.prefetch.enabled),
            stale: rule
                .and_then(|r| r.stale)
                .unwrap_or(self.config.stale.enabled),
        }
    }

    pub fn get(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
    ) -> Option<(Message, Scope)> {
        self.lookup(query, outgoing, now, false)
            .map(|hit| (hit.message, hit.scope))
    }

    pub fn lookup(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> Option<Hit> {
        self.lookup_inner(query, outgoing, now, allow_stale, true)
    }

    /// Race-closing checks do not represent another client request or refresh demand.
    pub fn peek(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> Option<Hit> {
        self.lookup_inner(query, outgoing, now, allow_stale, false)
    }

    fn lookup_inner(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
        observe: bool,
    ) -> Option<Hit> {
        let Some(key) = Key::of(query) else {
            if observe && !allow_stale {
                self.bypasses.fetch_add(1, Ordering::Relaxed);
            }
            return None;
        };
        if self.policy(&key, query.queries[0].name()).bypass {
            if observe && !allow_stale {
                self.bypasses.fetch_add(1, Ordering::Relaxed);
            }
            return None;
        }
        let entry = {
            let mut shard = self.shards[self.shard(&key)]
                .lock()
                .expect("cache shard poisoned");
            let Some((part, id)) = shard.select(&key, outgoing, now, allow_stale) else {
                shard.prune(&key, now);
                if observe && !allow_stale {
                    shard.counts.misses += 1;
                }
                return None;
            };
            if observe {
                shard.clock += 1;
                let clock = shard.clock;
                let entry = Arc::clone(shard.partitions[part].lru.get(&id).unwrap());
                entry.accessed.store(clock, Ordering::Relaxed);
                if now.saturating_duration_since(entry.inserted) < entry.lifetime {
                    shard.counts.hits += 1;
                } else {
                    shard.counts.stale_hits += 1;
                }
                entry
            } else {
                Arc::clone(shard.partitions[part].lru.peek(&id).unwrap())
            }
        };
        let age = now.saturating_duration_since(entry.inserted);
        let stale = age >= entry.lifetime;
        let hits = if observe {
            entry.hits.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            entry.hits.load(Ordering::Relaxed)
        };
        let refresh = !stale
            && !entry.negative
            && entry.prefetch
            && hits >= self.config.prefetch.min_hits
            && entry.lifetime.saturating_sub(age).as_millis() * 100
                <= entry.lifetime.as_millis() * u128::from(self.config.prefetch.remaining_percent);
        let elapsed = age.as_secs().min(u64::from(u32::MAX)) as u32;
        let mut message = protocol::decode(&entry.wire).ok()?;
        for rr in message
            .answers
            .iter_mut()
            .chain(&mut message.authorities)
            .chain(&mut message.additionals)
        {
            rr.ttl = if stale {
                self.config.stale.reply_ttl_secs
            } else {
                rr.ttl.saturating_sub(elapsed)
            };
        }
        message.metadata.id = query.id;
        message.metadata.authentic_data = false;
        message.queries = query.queries.clone();
        if let Some(edns) = &query.edns {
            let mut response_edns = Edns::new();
            response_edns
                .set_max_payload(MAX_UDP_PAYLOAD)
                .set_dnssec_ok(edns.flags().dnssec_ok);
            message.edns = Some(response_edns);
        }
        Some(Hit {
            message,
            scope: entry.scope,
            stale,
            refresh,
        })
    }

    pub fn insert(&self, query: &Message, response: &Message, scope: Scope, now: Instant) {
        self.insert_if_epoch(query, response, scope, now, self.epoch());
    }

    pub fn insert_if_epoch(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
    ) -> bool {
        let Some(key) = Key::of(query) else {
            return false;
        };
        let policy = self.policy(&key, query.queries[0].name());
        if policy.bypass {
            return false;
        }
        // Only a complete, transaction-independent successful answer supersedes
        // earlier knowledge. Transient errors and client-specific extensions do not.
        if response.truncation
            || response.signature.is_some()
            || !superseding_edns(response)
            || !matches!(
                response.response_code,
                ResponseCode::NoError | ResponseCode::NXDomain
            )
        {
            return false;
        }
        // Expensive response preparation remains outside the shard lock. Admission
        // failure is distinct from supersession: TTL=0 is still newer knowledge.
        let prepared = prepare_response(query, response, &policy).and_then(|(stored, lifetime)| {
            let negative =
                stored.response_code == ResponseCode::NXDomain || stored.answers.is_empty();
            stored.to_vec().ok().map(|wire| (wire, lifetime, negative))
        });
        let mut shard = self.shards[self.shard(&key)]
            .lock()
            .expect("cache shard poisoned");
        if epoch != self.epoch() {
            shard.counts.rejections += 1;
            return false;
        }
        shard.prune(&key, now);
        // A narrower new scope can invalidate part of an older broad answer.
        // Removing the whole overlapping entry is conservative; retaining it could
        // resurrect known obsolete data after this answer expires or is uncacheable.
        let replaced: Vec<_> = shard
            .buckets
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|e| scopes_overlap(e.scope, scope))
            .map(|e| (usize::from(e.negative), e.id))
            .collect();
        for (p, id) in replaced {
            shard.remove(p, id);
        }
        let Some((wire, lifetime, negative)) = prepared else {
            return false;
        };
        // Conservative per-variant estimate includes duplicated indices, Arc controls,
        // HashMap slack and LRU allocation. Live Arc readers retain this charge after eviction.
        let charge = wire.len() + key.name.len() + size_of::<Entry>() + size_of::<Key>() + 192;
        let part = usize::from(negative);
        if charge > shard.partitions[part].max_bytes || shard.partitions[part].max_entries == 0 {
            shard.counts.rejections += 1;
            return false;
        }
        if let Some(ids) = shard.buckets.get(&key)
            && ids.len() >= self.config.max_variants
        {
            let Some(entry) = ids
                .iter()
                .filter(|e| e.negative == negative)
                .min_by_key(|e| e.accessed.load(Ordering::Relaxed))
            else {
                // A variant ceiling must not let negative churn consume positive slots.
                shard.counts.rejections += 1;
                return false;
            };
            let (p, id) = (usize::from(entry.negative), entry.id);
            shard.remove(p, id);
            shard.counts.evictions += 1;
        }
        loop {
            let p = &shard.partitions[part];
            if p.lru.len() < p.max_entries
                && p.charged.load(Ordering::Relaxed) + charge <= p.max_bytes
            {
                break;
            }
            let Some((&id, _)) = p.lru.peek_lru() else {
                shard.counts.rejections += 1;
                return false;
            };
            shard.remove(part, id);
            shard.counts.evictions += 1;
        }
        shard.clock += 1;
        let id = shard.clock;
        let key = Arc::new(key);
        let lifetime = Duration::from_secs(u64::from(lifetime));
        let retention = if policy.stale && !negative {
            lifetime + Duration::from_secs(self.config.stale.retention_secs)
        } else {
            lifetime
        };
        let charged = Arc::clone(&shard.partitions[part].charged);
        charged.fetch_add(charge, Ordering::Relaxed);
        let entry = Arc::new(Entry {
            id,
            key: Arc::clone(&key),
            wire: wire.into_boxed_slice(),
            scope,
            inserted: now,
            lifetime,
            retention,
            negative,
            prefetch: policy.prefetch,
            hits: AtomicU64::new(0),
            accessed: AtomicU64::new(id),
            charge,
            charged,
        });
        shard
            .buckets
            .entry(key)
            .or_default()
            .push(Arc::clone(&entry));
        shard.partitions[part].lru.put(id, entry);
        true
    }

    /// Acquiring every shard before advancing the epoch linearizes invalidation
    /// with admission, including work whose encoding started before this call.
    pub fn invalidate(
        &self,
        name: Option<&str>,
        kind: Option<RecordType>,
        scope: Option<Scope>,
    ) -> usize {
        let name = name.map(canonical);
        let mut shards: Vec<_> = self
            .shards
            .iter()
            .map(|s| s.lock().expect("cache shard poisoned"))
            .collect();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut removed = 0;
        for shard in &mut shards {
            let matches: Vec<_> = shard
                .partitions
                .iter()
                .enumerate()
                .flat_map(|(part, p)| {
                    p.lru
                        .iter()
                        .filter(|(_, e)| {
                            name.as_ref().is_none_or(|n| e.key.name.as_ref() == n)
                                && kind.is_none_or(|k| e.key.kind == k)
                                && scope.is_none_or(|s| e.scope == s)
                        })
                        .map(move |(&id, _)| (part, id))
                })
                .collect();
            removed += matches.len();
            for (part, id) in matches {
                shard.remove(part, id);
            }
        }
        removed
    }

    /// Charge includes entry allocations still held by readers; it is not RSS.
    pub fn snapshot(&self) -> Value {
        let mut counts = Counts::default();
        let (mut positive, mut negative, mut bytes) = (0, 0, 0);
        for shard in &self.shards {
            let shard = shard.lock().expect("cache shard poisoned");
            positive += shard.partitions[0].lru.len();
            negative += shard.partitions[1].lru.len();
            bytes += shard
                .partitions
                .iter()
                .map(|p| p.charged.load(Ordering::Relaxed))
                .sum::<usize>();
            counts.hits += shard.counts.hits;
            counts.stale_hits += shard.counts.stale_hits;
            counts.misses += shard.counts.misses;
            counts.evictions += shard.counts.evictions;
            counts.rejections += shard.counts.rejections;
        }
        json!({"entries":positive+negative,"bytes":bytes,"positive_entries":positive,
            "negative_entries":negative,"hits":counts.hits,"stale_hits":counts.stale_hits,
            "misses":counts.misses,"bypasses":self.bypasses.load(Ordering::Relaxed),
            "evictions":counts.evictions,"rejections":counts.rejections,
            "max_entries":self.config.max_entries,"max_bytes":self.config.max_bytes,
            "shards":self.shards.len(),"epoch":self.epoch()})
    }

    pub fn inspect(&self, name: &str, kind: Option<RecordType>, now: Instant) -> Value {
        let name = canonical(name);
        // Copy at most 256 Arc entries under locks, then build JSON outside them.
        let mut entries = Vec::new();
        let mut truncated = false;
        for shard in &self.shards {
            let shard = shard.lock().expect("cache shard poisoned");
            for p in &shard.partitions {
                for (_, e) in &p.lru {
                    if e.key.name.as_ref() == name && kind.is_none_or(|k| e.key.kind == k) {
                        if entries.len() == 256 {
                            truncated = true;
                            break;
                        }
                        entries.push(Arc::clone(e));
                    }
                }
            }
        }
        let variants: Vec<_> = entries.iter().map(|e| {
            let age = now.saturating_duration_since(e.inserted);
            json!({"name":e.key.name,"qtype":e.key.kind.to_string(),
                "dnssec_ok":e.key.dnssec_ok,"checking_disabled":e.key.checking_disabled,
                "recursion_desired":e.key.recursion_desired,"edns":e.key.edns,
                "scope":scope_text(e.scope),"negative":e.negative,
                "fresh_remaining_secs":e.lifetime.saturating_sub(age).as_secs(),
                "retention_remaining_secs":e.retention.saturating_sub(age).as_secs(),
                "hits":e.hits.load(Ordering::Relaxed),"bytes":e.charge,
                "state":if age < e.lifetime {"fresh"} else if age < e.retention {"stale"} else {"expired"}})
        }).collect();
        json!({"name":name,"variants":variants,"truncated":truncated})
    }

    pub fn explain(&self, query: &Message, outgoing: Option<ClientSubnet>, now: Instant) -> Value {
        let Some(key) = Key::of(query) else {
            return json!({"eligible":false,"reason":"unsupported_query_semantics","policy":null,
                "state":"bypass","scope":null});
        };
        let policy = self.policy(&key, query.queries[0].name());
        if policy.bypass {
            return json!({"eligible":false,"reason":if self.config.enabled {"rule_bypass"} else {"cache_disabled"},
                "policy":policy,"state":"bypass","scope":null});
        }
        let shard = self.shards[self.shard(&key)]
            .lock()
            .expect("cache shard poisoned");
        let selected = shard.select(&key, outgoing, now, true).map(|(part, id)| {
            let e = shard.partitions[part].lru.peek(&id).unwrap();
            (
                now.saturating_duration_since(e.inserted) < e.lifetime,
                e.scope,
            )
        });
        drop(shard);
        json!({"eligible":true,"reason":match selected {
            Some((true,_))=>"matching_fresh_scope",Some((false,_))=>"failure_only_stale",None=>"no_matching_retained_entry"},
            "policy":policy,"state":match selected {Some((true,_))=>"fresh",Some((false,_))=>"stale",None=>"miss"},
            "scope":selected.map(|(_,scope)|scope_text(scope))})
    }
}

fn scope_text(scope: Scope) -> String {
    match scope {
        Scope::NoEcs => "no_ecs".into(),
        Scope::Privacy { ipv4: true } => "privacy_v4".into(),
        Scope::Privacy { ipv4: false } => "privacy_v6".into(),
        Scope::Network(network) => network.to_string(),
    }
}

fn canonical(name: &str) -> String {
    let Ok(mut name) = Name::from_ascii(name) else {
        return name.to_ascii_lowercase();
    };
    name.set_fqdn(true);
    name.to_lowercase().to_ascii()
}

fn scopes_overlap(a: Scope, b: Scope) -> bool {
    match (a, b) {
        (Scope::Network(a), Scope::Network(b)) => a.contains(&b.addr()) || b.contains(&a.addr()),
        _ => a == b,
    }
}

fn prepare_response(
    query: &Message,
    response: &Message,
    config: &Policy,
) -> Option<(Message, u32)> {
    if response.truncation
        || response.signature.is_some()
        || !plain_edns(response)
        || !matches!(
            response.response_code,
            ResponseCode::NoError | ResponseCode::NXDomain
        )
    {
        return None;
    }
    let question = query.queries.first()?;
    let negative = response.response_code == ResponseCode::NXDomain || response.answers.is_empty();
    let mut stored = response.clone();
    let mut lifetime = config.max_ttl_secs;
    if negative {
        // CNAME+negative answers need a canonical-target proof; defer that case.
        if !response.answers.is_empty() {
            return None;
        }
        let negative_ttl = response
            .authorities
            .iter()
            .filter_map(|rr| match &rr.data {
                RData::SOA(soa)
                    if rr.dns_class == question.query_class()
                        && rr.name.zone_of(question.name()) =>
                {
                    Some(rr.ttl.min(soa.minimum))
                }
                _ => None,
            })
            .min()?;
        lifetime = lifetime.min(config.negative_ttl_cap_secs).min(negative_ttl);
    } else if !response
        .answers
        .iter()
        .any(|rr| rr.record_type() == question.query_type())
    {
        return None;
    }
    for rr in stored
        .answers
        .iter_mut()
        .chain(&mut stored.authorities)
        .chain(&mut stored.additionals)
    {
        if matches!(rr.record_type(), RecordType::SIG | RecordType::TSIG) {
            return None;
        }
        // RFC 2181 TTL high-bit values must be treated as zero, not multi-year TTLs.
        if rr.ttl > i32::MAX as u32 {
            return None;
        }
        rr.ttl = rr.ttl.min(config.max_ttl_secs);
        if negative && rr.record_type() == RecordType::SOA {
            rr.ttl = rr.ttl.min(lifetime);
        }
        lifetime = lifetime.min(rr.ttl);
    }
    if lifetime == 0 {
        return None;
    }
    stored.metadata.id = 0;
    stored.metadata.authentic_data = false;
    stored.edns = None;
    Some((stored, lifetime))
}

#[cfg(test)]
mod tests;
