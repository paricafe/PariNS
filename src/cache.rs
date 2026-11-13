//! Bounded DNS cache for upstream answers, failure responses, and cache diagnostics.

use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Edns, Message, Query, ResponseCode},
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

mod decision;
pub mod persistence;
mod sharing;
pub use decision::{
    DecisionReason, LookupDecision, LookupOutcome, LookupResult, StoreDecision, StoreOutcome,
};
use decision::{EvictionReason, EvictionState, Evictions, PartitionSnapshot, ShardSnapshot};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    name: Name,
    kind: RecordType,
    class: DNSClass,
    dnssec_ok: bool,
    checking_disabled: bool,
    recursion_desired: bool,
    edns: bool,
}

impl Key {
    fn of(query: &Message) -> Option<Self> {
        let question = &query.queries[0];
        if question.query_class() != DNSClass::IN || !plain_edns(query) {
            return None;
        }
        Some(Self {
            name: key_name(question.name()),
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
    protocol::edns_within(message, &[EdnsCode::Subnet, EdnsCode::Padding])
}

fn query_rejection(query: &Message) -> DecisionReason {
    if plain_edns(query) {
        DecisionReason::UnsupportedQuery
    } else {
        DecisionReason::UnsupportedEdns
    }
}

fn superseding_edns(message: &Message) -> bool {
    protocol::edns_within(
        message,
        &[EdnsCode::Subnet, EdnsCode::Padding, EdnsCode::Unknown(15)],
    )
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
    failure: Option<Failure>,
    prefetch: bool,
    refreshed: AtomicBool,
    hits: AtomicU64,
    accessed: AtomicU64,
    charge: usize,
    charged: Arc<AtomicUsize>,
}

impl Entry {
    fn settle_refresh(&self) -> bool {
        let refreshed = self.refreshed.load(Ordering::Relaxed);
        if refreshed {
            self.refreshed.store(false, Ordering::Relaxed);
        }
        refreshed
    }
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
    evictions: Evictions,
}

impl Partition {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            lru: LruCache::unbounded(),
            charged: Arc::new(AtomicUsize::new(0)),
            max_entries,
            max_bytes,
            evictions: Evictions::default(),
        }
    }

    fn snapshot(&self) -> PartitionSnapshot {
        PartitionSnapshot {
            entries: self.lru.len(),
            max_entries: self.max_entries,
            bytes: self.charged.load(Ordering::Relaxed),
            max_bytes: self.max_bytes,
            evictions: self.evictions.snapshot(),
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
    shared_hits: u64,
    revocations: u64,
    failure_hits: u64,
}

struct Shard {
    buckets: HashMap<Arc<Key>, Vec<Arc<Entry>>>,
    partitions: [Partition; 2],
    clock: u64,
    counts: Counts,
    sharing: sharing::Table,
    learned: usize,
    failures: usize,
}

impl Shard {
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
                (entry.scope.matches(outgoing)
                    && age < entry.retention
                    && (fresh || allow_stale)
                    && !(allow_stale && entry.failure.is_some()))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    StaleFallback,
    Final,
}

pub struct Hit {
    pub message: Message,
    pub scope: Scope,
    pub stale: bool,
    pub refresh: bool,
    pub authentic: bool,
    pub failure: Option<Failure>,
}

pub struct Cache {
    identity: u128,
    config: CacheConfig,
    shards: Vec<Mutex<Shard>>,
    hasher: RandomState,
    rules: Vec<Rule>,
    epoch: AtomicU64,
    bypasses: AtomicU64,
    refresh_useful: AtomicU64,
    refresh_unused: AtomicU64,
    diagnostics: decision::Diagnostics,
}

impl Cache {
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
                    sharing: sharing::Table::new(sharing::capacity(config.max_entries, count)),
                    learned: 0,
                    failures: 0,
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
                qtype: rule
                    .qtype
                    .as_ref()
                    .map(|q| q.parse().expect("validated cache rule qtype")),
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
            identity: rand::random(),
            config,
            shards,
            hasher: RandomState::new(),
            rules,
            epoch: AtomicU64::new(0),
            bypasses: AtomicU64::new(0),
            refresh_useful: AtomicU64::new(0),
            refresh_unused: AtomicU64::new(0),
            diagnostics: decision::Diagnostics::default(),
        }
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn invalidation_token(&self) -> String {
        self.token_at_epoch(self.epoch())
    }

    fn token_at_epoch(&self, epoch: u64) -> String {
        format!("{:032x}:{epoch}", self.identity)
    }

    fn shard(&self, key: &Key) -> usize {
        self.hasher.hash_one(key) as usize % self.shards.len()
    }

    fn remove(&self, shard: &mut Shard, part: usize, id: u64) {
        if let Some(entry) = shard.partitions[part].lru.pop(&id) {
            if entry.settle_refresh() {
                self.refresh_unused.fetch_add(1, Ordering::Relaxed);
            }
            shard.learned -= usize::from(matches!(entry.scope, Scope::Learned { .. }));
            shard.failures -= usize::from(entry.failure.is_some());
            let empty = if let Some(ids) = shard.buckets.get_mut(&entry.key) {
                ids.retain(|entry| entry.id != id);
                ids.is_empty()
            } else {
                false
            };
            if empty {
                shard.buckets.remove(&entry.key);
            }
        }
    }

    fn evict(&self, shard: &mut Shard, part: usize, id: u64, now: Instant, reason: EvictionReason) {
        let partition = &mut shard.partitions[part];
        let entry = partition.lru.peek(&id).expect("admission eviction target");
        let age = now.saturating_duration_since(entry.inserted);
        let state = if age < entry.lifetime {
            EvictionState::Fresh
        } else if age < entry.retention {
            EvictionState::Retained
        } else {
            EvictionState::Expired
        };
        partition.evictions.record(reason, state);
        shard.counts.evictions += 1;
        self.remove(shard, part, id);
    }

    fn prune(&self, shard: &mut Shard, key: &Key, now: Instant) {
        let expired: Vec<_> = shard
            .buckets
            .get(key)
            .into_iter()
            .flatten()
            .filter(|e| now.saturating_duration_since(e.inserted) >= e.retention)
            .map(|e| (usize::from(e.negative), e.id))
            .collect();
        for (part, id) in expired {
            self.remove(shard, part, id);
        }
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

    #[cfg(test)]
    pub fn lookup(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> Option<Hit> {
        self.lookup_with_decision(query, outgoing, now, allow_stale)
            .hit
    }

    pub fn lookup_with_decision(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> LookupResult {
        let result = self.lookup_inner(query, outgoing, now, allow_stale, true);
        self.diagnostics.lookup(result.decision);
        result
    }

    pub fn diagnostics_snapshot(&self) -> Value {
        let mut value = self.diagnostics.snapshot();
        let (hits, entries, revocations) = self.sharing_counts();
        let object = value.as_object_mut().expect("diagnostics object");
        object.insert("ecs_shared_hits".into(), json!(hits));
        object.insert("ecs_shared_entries".into(), json!(entries));
        object.insert("ecs_sharing_revocations".into(), json!(revocations));
        let capacity: Vec<_> = self
            .shards
            .iter()
            .enumerate()
            .map(|(index, shard)| {
                let shard = shard.lock().expect("cache shard poisoned");
                ShardSnapshot {
                    shard: index,
                    positive: shard.partitions[0].snapshot(),
                    negative: shard.partitions[1].snapshot(),
                }
            })
            .collect();
        object.insert("capacity".into(), json!({"shards":capacity}));
        value
    }

    pub fn refresh_outcomes(&self) -> (u64, u64) {
        (
            self.refresh_useful.load(Ordering::Relaxed),
            self.refresh_unused.load(Ordering::Relaxed),
        )
    }

    fn sharing_counts(&self) -> (u64, usize, u64) {
        self.shards
            .iter()
            .fold((0, 0, 0), |(hits, entries, revoked), shard| {
                let shard = shard.lock().expect("cache shard poisoned");
                (
                    hits + shard.counts.shared_hits,
                    entries + shard.learned,
                    revoked + shard.counts.revocations,
                )
            })
    }

    pub fn record_unusable_scope(&self) -> StoreDecision {
        let decision = StoreDecision::skipped(DecisionReason::EcsUnusable);
        self.diagnostics.store(decision);
        decision
    }

    pub fn peek(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
    ) -> Option<Hit> {
        self.lookup_inner(query, outgoing, now, allow_stale, false)
            .hit
    }

    fn lookup_inner(
        &self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
        allow_stale: bool,
        observe: bool,
    ) -> LookupResult {
        let Some(key) = Key::of(query) else {
            if observe && !allow_stale {
                self.bypasses.fetch_add(1, Ordering::Relaxed);
            }
            return LookupResult::empty(LookupOutcome::Bypass, Some(query_rejection(query)));
        };
        if self.policy(&key, query.queries[0].name()).bypass {
            if observe && !allow_stale {
                self.bypasses.fetch_add(1, Ordering::Relaxed);
            }
            return LookupResult::empty(
                if self.config.enabled {
                    LookupOutcome::Bypass
                } else {
                    LookupOutcome::Disabled
                },
                Some(DecisionReason::PolicyDisabled),
            );
        }
        let entry = {
            let mut shard = self.shards[self.shard(&key)]
                .lock()
                .expect("cache shard poisoned");
            let Some((part, id)) = shard.select(&key, outgoing, now, allow_stale) else {
                if observe {
                    self.prune(&mut shard, &key, now);
                    if !allow_stale {
                        shard.counts.misses += 1;
                    }
                }
                return LookupResult::empty(LookupOutcome::Miss, None);
            };
            if observe {
                shard.clock += 1;
                let clock = shard.clock;
                let entry = Arc::clone(shard.partitions[part].lru.get(&id).unwrap());
                entry.accessed.store(clock, Ordering::Relaxed);
                let fresh = now.saturating_duration_since(entry.inserted) < entry.lifetime;
                if fresh {
                    shard.counts.hits += 1;
                } else {
                    shard.counts.stale_hits += 1;
                }
                if entry.settle_refresh() {
                    if fresh {
                        self.refresh_useful.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.refresh_unused.fetch_add(1, Ordering::Relaxed);
                    }
                }
                shard.counts.shared_hits += u64::from(matches!(entry.scope, Scope::Learned { .. }));
                shard.counts.failure_hits += u64::from(entry.failure.is_some());
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
        let Ok(mut message) = protocol::decode(&entry.wire) else {
            return LookupResult::empty(
                LookupOutcome::Bypass,
                Some(DecisionReason::UncacheableResponse),
            );
        };
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
        let authentic = std::mem::take(&mut message.metadata.authentic_data) && !stale;
        message.queries = query.queries.clone();
        if let Some(mut response_edns) = protocol::reply_edns(query) {
            if entry.failure.is_some() {
                for option in protocol::extended_errors(&message) {
                    response_edns.options_mut().insert(option);
                }
            }
            message.edns = Some(response_edns);
        }
        LookupResult {
            hit: Some(Hit {
                message,
                scope: entry.scope,
                stale,
                refresh,
                authentic,
                failure: entry.failure,
            }),
            decision: LookupDecision {
                outcome: if stale {
                    LookupOutcome::Stale
                } else {
                    LookupOutcome::Fresh
                },
                reason: None,
            },
        }
    }

    #[cfg(test)]
    pub fn insert_if_epoch(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
    ) -> bool {
        self.insert_decision_if_epoch(query, response, scope, now, epoch)
            .admitted()
    }

    pub fn insert_decision_if_epoch(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
    ) -> StoreDecision {
        let decision = self.admit(query, response, scope, now, epoch, Origin::Answer);
        self.diagnostics.store(decision);
        decision
    }

    pub fn insert_refresh_if_epoch(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
    ) -> StoreDecision {
        let decision = self.admit(query, response, scope, now, epoch, Origin::Refresh);
        self.diagnostics.store(decision);
        decision
    }

    fn admit(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
        origin: Origin,
    ) -> StoreDecision {
        let restore = match origin {
            Origin::Restore(restore) => Some(restore),
            Origin::Answer | Origin::Refresh => None,
        };
        let restoring = restore.is_some();
        let Some(key) = Key::of(query) else {
            return StoreDecision::skipped(query_rejection(query));
        };
        let policy = self.policy(&key, query.queries[0].name());
        if policy.bypass {
            return StoreDecision::skipped(DecisionReason::PolicyDisabled);
        }
        if !superseding_edns(response) {
            return StoreDecision::skipped(DecisionReason::UnsupportedEdns);
        }
        if response.truncation
            || response.signature.is_some()
            || !matches!(
                response.response_code,
                ResponseCode::NoError | ResponseCode::NXDomain
            )
        {
            return StoreDecision::skipped(DecisionReason::UncacheableResponse);
        }
        let prepared =
            prepare_response(query, response, &policy).and_then(|(stored, lifetime, negative)| {
                let lifetime = restore.map_or(lifetime, |restore| lifetime.min(restore.remaining));
                if lifetime == 0 {
                    return Err(DecisionReason::TtlZero);
                }
                stored
                    .to_vec()
                    .map(|wire| (wire, lifetime, negative))
                    .map_err(|_| DecisionReason::UncacheableResponse)
            });
        let learning = self.config.ecs_sharing.enabled && !restoring;
        let fingerprint = (learning && matches!(scope, Scope::ExactSource(_)))
            .then(|| sharing::fingerprint(&query.queries[0], response))
            .flatten();
        let mut shard = self.shards[self.shard(&key)]
            .lock()
            .expect("cache shard poisoned");
        if epoch != self.epoch() {
            shard.counts.rejections += u64::from(!restoring);
            return StoreDecision::skipped(DecisionReason::EpochChanged);
        }
        self.prune(&mut shard, &key, now);
        let scope = if learning {
            let lifetime = prepared
                .as_ref()
                .ok()
                .map(|(_, lifetime, _)| Duration::from_secs(u64::from(*lifetime)));
            let observation = shard.sharing.observe(
                &key,
                scope,
                fingerprint,
                lifetime,
                now,
                &self.config.ecs_sharing,
            );
            shard.counts.revocations += u64::from(observation.revoked);
            observation.scope
        } else {
            scope
        };
        if restoring {
            let Ok((wire, _, negative)) = &prepared else {
                return StoreDecision::skipped(*prepared.as_ref().unwrap_err());
            };
            let charge = charge(wire, &key);
            let p = &shard.partitions[usize::from(*negative)];
            if p.lru.len() >= p.max_entries
                || p.charged.load(Ordering::Relaxed).saturating_add(charge) > p.max_bytes
                || shard.buckets.get(&key).is_some_and(|entries| {
                    entries.len() >= self.config.max_variants
                        || entries
                            .iter()
                            .any(|entry| scopes_overlap(entry.scope, scope))
                })
            {
                return StoreDecision::skipped(DecisionReason::Capacity);
            }
        }
        let replaced: Vec<_> = shard
            .buckets
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|e| scopes_overlap(e.scope, scope))
            .map(|e| (usize::from(e.negative), e.id))
            .collect();
        let superseded = !replaced.is_empty();
        for (p, id) in replaced {
            self.remove(&mut shard, p, id);
        }
        let (wire, lifetime, negative) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => return StoreDecision::rejected(reason, superseded),
        };
        let lifetime = Duration::from_secs(u64::from(lifetime));
        let retention = if policy.stale && !negative {
            lifetime + Duration::from_secs(self.config.stale.retention_secs)
        } else {
            lifetime
        };
        self.place(
            &mut shard,
            key,
            Stored {
                wire,
                scope,
                lifetime,
                retention,
                negative,
                failure: None,
                prefetch: policy.prefetch,
                refreshed: matches!(origin, Origin::Refresh),
                accessed: restore.map(|restore| restore.accessed),
            },
            now,
            superseded,
        )
    }

    pub fn insert_failure_if_epoch(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
        failure: Failure,
    ) -> StoreDecision {
        let decision = self.admit_failure(query, response, scope, now, epoch, failure);
        self.diagnostics.store(decision);
        decision
    }

    fn admit_failure(
        &self,
        query: &Message,
        response: &Message,
        scope: Scope,
        now: Instant,
        epoch: u64,
        failure: Failure,
    ) -> StoreDecision {
        let Some(key) = Key::of(query) else {
            return StoreDecision::skipped(query_rejection(query));
        };
        let policy = self.policy(&key, query.queries[0].name());
        if policy.bypass {
            return StoreDecision::skipped(DecisionReason::PolicyDisabled);
        }
        let lifetime = self
            .config
            .servfail_ttl_secs
            .min(policy.negative_ttl_cap_secs);
        if response.response_code != ResponseCode::ServFail
            || response.truncation
            || response.signature.is_some()
            || !superseding_edns(response)
            || lifetime == 0
        {
            return StoreDecision::skipped(DecisionReason::UncacheableResponse);
        }
        let mut stored = protocol::error_response(query, ResponseCode::ServFail);
        let errors = protocol::extended_errors(response);
        if !errors.is_empty() {
            let edns = stored.edns.get_or_insert_with(|| {
                let mut edns = Edns::new();
                edns.set_max_payload(MAX_UDP_PAYLOAD);
                edns
            });
            for option in errors {
                edns.options_mut().insert(option);
            }
        }
        let Ok(wire) = stored.to_vec() else {
            return StoreDecision::skipped(DecisionReason::UncacheableResponse);
        };
        let mut shard = self.shards[self.shard(&key)]
            .lock()
            .expect("cache shard poisoned");
        if epoch != self.epoch() {
            shard.counts.rejections += 1;
            return StoreDecision::skipped(DecisionReason::EpochChanged);
        }
        self.prune(&mut shard, &key, now);
        let overlapping = || {
            shard
                .buckets
                .get(&key)
                .into_iter()
                .flatten()
                .filter(|e| scopes_overlap(e.scope, scope))
        };
        if overlapping()
            .any(|e| e.failure.is_none() && now.saturating_duration_since(e.inserted) < e.lifetime)
        {
            return StoreDecision::skipped(DecisionReason::UncacheableResponse);
        }
        let replaced: Vec<_> = overlapping()
            .filter(|e| e.failure.is_some())
            .map(|e| e.id)
            .collect();
        let superseded = !replaced.is_empty();
        for id in replaced {
            self.remove(&mut shard, 1, id);
        }
        let lifetime = Duration::from_secs(u64::from(lifetime));
        self.place(
            &mut shard,
            key,
            Stored {
                wire,
                scope,
                lifetime,
                retention: lifetime,
                negative: true,
                failure: Some(failure),
                prefetch: false,
                refreshed: false,
                accessed: None,
            },
            now,
            superseded,
        )
    }

    fn place(
        &self,
        shard: &mut Shard,
        key: Key,
        stored: Stored,
        now: Instant,
        superseded: bool,
    ) -> StoreDecision {
        let Stored {
            wire,
            scope,
            lifetime,
            retention,
            negative,
            failure,
            prefetch,
            refreshed,
            accessed,
        } = stored;
        let charge = charge(&wire, &key);
        let part = usize::from(negative);
        if charge > shard.partitions[part].max_bytes || shard.partitions[part].max_entries == 0 {
            shard.counts.rejections += 1;
            return StoreDecision::rejected(DecisionReason::Capacity, superseded);
        }
        if let Some(ids) = shard.buckets.get(&key)
            && ids.len() >= self.config.max_variants
        {
            let Some(entry) = ids
                .iter()
                .filter(|e| e.negative == negative)
                .min_by_key(|e| e.accessed.load(Ordering::Relaxed))
            else {
                shard.counts.rejections += 1;
                return StoreDecision::rejected(DecisionReason::VariantLimit, superseded);
            };
            let (p, id) = (usize::from(entry.negative), entry.id);
            self.evict(shard, p, id, now, EvictionReason::Variants);
        }
        loop {
            let p = &shard.partitions[part];
            let entry_limit = p.lru.len() >= p.max_entries;
            let byte_limit = p.charged.load(Ordering::Relaxed) + charge > p.max_bytes;
            if !entry_limit && !byte_limit {
                break;
            }
            let Some((&id, _)) = p.lru.peek_lru() else {
                shard.counts.rejections += 1;
                return StoreDecision::rejected(DecisionReason::Capacity, superseded);
            };
            let reason = match (entry_limit, byte_limit) {
                (true, true) => EvictionReason::EntriesAndBytes,
                (true, false) => EvictionReason::Entries,
                _ => EvictionReason::Bytes,
            };
            self.evict(shard, part, id, now, reason);
        }
        shard.clock += 1;
        let id = shard.clock;
        let key = Arc::new(key);
        let charged = Arc::clone(&shard.partitions[part].charged);
        charged.fetch_add(charge, Ordering::Relaxed);
        shard.learned += usize::from(matches!(scope, Scope::Learned { .. }));
        shard.failures += usize::from(failure.is_some());
        let entry = Arc::new(Entry {
            id,
            key: Arc::clone(&key),
            wire: wire.into_boxed_slice(),
            scope,
            inserted: now,
            lifetime,
            retention,
            negative,
            failure,
            prefetch,
            refreshed: AtomicBool::new(refreshed),
            hits: AtomicU64::new(0),
            accessed: AtomicU64::new(accessed.unwrap_or(id)),
            charge,
            charged,
        });
        shard
            .buckets
            .entry(key)
            .or_default()
            .push(Arc::clone(&entry));
        let lru = &mut shard.partitions[part].lru;
        lru.put(id, entry);
        if accessed.is_some() {
            lru.demote(&id);
        }
        StoreDecision {
            outcome: if superseded {
                StoreOutcome::Replaced
            } else {
                StoreOutcome::Admitted
            },
            reason: None,
        }
    }

    pub fn invalidate(
        &self,
        name: Option<&Name>,
        kind: Option<RecordType>,
        scope: Option<Scope>,
    ) -> usize {
        let name = name.map(key_name);
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
                            name.as_ref().is_none_or(|n| &e.key.name == n)
                                && kind.is_none_or(|k| e.key.kind == k)
                                && scope.is_none_or(|s| e.scope == s || learned_serves(e.scope, s))
                        })
                        .map(move |(&id, _)| (part, id))
                })
                .collect();
            removed += matches.len();
            for (part, id) in matches {
                self.remove(shard, part, id);
            }
            shard.sharing.invalidate(name.as_ref(), kind, scope);
        }
        removed
    }

    pub fn snapshot(&self) -> Value {
        let epoch = self.epoch();
        let cache_token = self.token_at_epoch(epoch);
        let mut counts = Counts::default();
        let (mut positive, mut negative, mut bytes, mut learned, mut failures) = (0, 0, 0, 0, 0);
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
            counts.shared_hits += shard.counts.shared_hits;
            counts.revocations += shard.counts.revocations;
            counts.failure_hits += shard.counts.failure_hits;
            learned += shard.learned;
            failures += shard.failures;
        }
        json!({"entries":positive+negative,"bytes":bytes,"positive_entries":positive,
            "negative_entries":negative,"hits":counts.hits,"stale_hits":counts.stale_hits,
            "misses":counts.misses,"bypasses":self.bypasses.load(Ordering::Relaxed),
            "evictions":counts.evictions,"rejections":counts.rejections,
            "max_entries":self.config.max_entries,"max_bytes":self.config.max_bytes,
            "shards":self.shards.len(),"epoch":epoch,"cache_token":cache_token,
            "ecs_shared_hits":counts.shared_hits,"ecs_shared_entries":learned,
            "ecs_sharing_revocations":counts.revocations,
            "servfail_entries":failures,"servfail_hits":counts.failure_hits,
            "refresh_useful":self.refresh_useful.load(Ordering::Relaxed),"refresh_unused":self.refresh_unused.load(Ordering::Relaxed)})
    }

    pub fn inspect(&self, name: &Name, kind: Option<RecordType>, now: Instant) -> Value {
        let name = key_name(name);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut sharing = None;
        for shard in &self.shards {
            let shard = shard.lock().expect("cache shard poisoned");
            if self.config.ecs_sharing.enabled {
                sharing = shard.sharing.states(&name, kind, now).chain(sharing).max();
            }
            for p in &shard.partitions {
                for (_, e) in &p.lru {
                    if e.key.name == name && kind.is_none_or(|k| e.key.kind == k) {
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
            json!({"name":e.key.name.to_ascii(),"qtype":e.key.kind.to_string(),
                "dnssec_ok":e.key.dnssec_ok,"checking_disabled":e.key.checking_disabled,
                "recursion_desired":e.key.recursion_desired,"edns":e.key.edns,
                "scope":e.scope.tag(),"negative":e.negative,"servfail":e.failure.is_some(),
                "fresh_remaining_secs":e.lifetime.saturating_sub(age).as_secs(),
                "retention_remaining_secs":e.retention.saturating_sub(age).as_secs(),
                "hits":e.hits.load(Ordering::Relaxed),"bytes":e.charge,
                "state":if age < e.lifetime {"fresh"} else if age < e.retention {"stale"} else {"expired"}})
        }).collect();
        json!({"name":name.to_ascii(),"variants":variants,"truncated":truncated,
            "ecs_sharing":sharing})
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
        let fresh = shard.select(&key, outgoing, now, false);
        let failure =
            fresh.and_then(|(part, id)| shard.partitions[part].lru.peek(&id).unwrap().failure);
        let selected = match (fresh, failure) {
            (Some(entry), Some(Failure::StaleFallback)) => {
                shard.select(&key, outgoing, now, true).or(Some(entry))
            }
            (Some(entry), _) => Some(entry),
            (None, _) => shard.select(&key, outgoing, now, true),
        }
        .map(|(part, id)| {
            let e = shard.partitions[part].lru.peek(&id).unwrap();
            (
                now.saturating_duration_since(e.inserted) < e.lifetime,
                e.scope,
                e.failure,
            )
        });
        drop(shard);
        json!({"eligible":true,"reason":match selected {
            Some((_,_,Some(Failure::Final)))=>"cached_dnssec_failure",
            Some((_,_,Some(Failure::StaleFallback)))=>"cached_upstream_failure",
            Some((false,_,None)) if failure.is_some()=>"cached_failure_stale_fallback",
            Some((true,_,None))=>"matching_fresh_scope",Some((false,_,None))=>"failure_only_stale",None=>"no_matching_retained_entry"},
            "policy":policy,"state":match selected {Some((_,_,Some(_)))=>"failure",Some((true,_,None))=>"fresh",Some((false,_,None))=>"stale",None=>"miss"},
            "scope":selected.map(|(_,scope,_)|scope.tag())})
    }
}

fn charge(wire: &[u8], key: &Key) -> usize {
    wire.len() + key.name.len() + size_of::<Entry>() + size_of::<Key>() + 192
}

fn key_name(name: &Name) -> Name {
    let mut name = name.to_lowercase();
    name.set_fqdn(true);
    name
}

fn scopes_overlap(a: Scope, b: Scope) -> bool {
    match (a, b) {
        (Scope::Network(a) | Scope::ExactSource(a), Scope::Network(b) | Scope::ExactSource(b)) => {
            a.contains(&b.addr()) || b.contains(&a.addr())
        }
        (Scope::Learned { ipv4 }, Scope::Network(n) | Scope::ExactSource(n))
        | (Scope::Network(n) | Scope::ExactSource(n), Scope::Learned { ipv4 }) => {
            n.addr().is_ipv4() == ipv4
        }
        _ => a == b,
    }
}

struct Stored {
    wire: Vec<u8>,
    scope: Scope,
    lifetime: Duration,
    retention: Duration,
    negative: bool,
    failure: Option<Failure>,
    prefetch: bool,
    refreshed: bool,
    accessed: Option<u64>,
}

#[derive(Clone, Copy)]
enum Origin {
    Answer,
    Refresh,
    Restore(Restore),
}

#[derive(Clone, Copy)]
struct Restore {
    remaining: u32,
    accessed: u64,
}

fn is_negative(question: &Query, response: &Message) -> bool {
    response.response_code == ResponseCode::NXDomain
        || !response
            .answers
            .iter()
            .any(|rr| rr.record_type() == question.query_type())
}

fn prepare_response(
    query: &Message,
    response: &Message,
    config: &Policy,
) -> Result<(Message, u32, bool), DecisionReason> {
    if !plain_edns(response) {
        return Err(DecisionReason::UnsupportedEdns);
    }
    let question = &query.queries[0];
    let negative = is_negative(question, response);
    let mut stored = response.clone();
    let mut lifetime = config.max_ttl_secs;
    if negative {
        let negative_ttl = if response.answers.is_empty() {
            response
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
                .min()
                .ok_or(DecisionReason::NegativeWithoutSoa)?
        } else {
            if question.query_type() == RecordType::CNAME {
                return Err(DecisionReason::UncacheableResponse);
            }
            cname_negative_ttl(question.name(), question.query_class(), response)?
        };
        lifetime = lifetime.min(config.negative_ttl_cap_secs).min(negative_ttl);
    }
    for rr in stored
        .answers
        .iter_mut()
        .chain(&mut stored.authorities)
        .chain(&mut stored.additionals)
    {
        if matches!(rr.record_type(), RecordType::SIG | RecordType::TSIG) {
            return Err(DecisionReason::UncacheableResponse);
        }
        if rr.ttl > i32::MAX as u32 {
            return Err(DecisionReason::TtlZero);
        }
        rr.ttl = rr.ttl.min(config.max_ttl_secs);
        if negative && rr.record_type() == RecordType::SOA {
            rr.ttl = rr.ttl.min(lifetime);
        }
        lifetime = lifetime.min(rr.ttl);
    }
    if lifetime == 0 {
        return Err(DecisionReason::TtlZero);
    }
    stored.metadata.id = 0;
    stored.edns = None;
    Ok((stored, lifetime, negative))
}

fn cname_negative_ttl(
    name: &Name,
    class: DNSClass,
    response: &Message,
) -> Result<u32, DecisionReason> {
    let mut targets = HashMap::with_capacity(response.answers.len());
    for rr in &response.answers {
        let RData::CNAME(target) = &rr.data else {
            return Err(DecisionReason::UncacheableResponse);
        };
        if rr.dns_class != class
            || targets
                .insert(&rr.name, &target.0)
                .is_some_and(|previous| previous != &target.0)
        {
            return Err(DecisionReason::UncacheableResponse);
        }
    }
    let mut terminal = name;
    for _ in 0..targets.len() {
        terminal = targets
            .get(terminal)
            .copied()
            .ok_or(DecisionReason::UncacheableResponse)?;
    }
    if targets.contains_key(terminal) {
        return Err(DecisionReason::UncacheableResponse);
    }

    let mut identity = None;
    let mut lifetime = None;
    for rr in &response.authorities {
        let RData::SOA(soa) = &rr.data else {
            continue;
        };
        if rr.dns_class != class || !rr.name.zone_of(terminal) {
            return Err(DecisionReason::NegativeWithoutSoa);
        }
        if let Some((owner, data)) = identity
            && (owner != &rr.name || data != soa)
        {
            return Err(DecisionReason::UncacheableResponse);
        }
        identity = Some((&rr.name, soa));
        let ttl = rr.ttl.min(soa.minimum);
        lifetime = Some(lifetime.map_or(ttl, |current: u32| current.min(ttl)));
    }
    lifetime.ok_or(DecisionReason::NegativeWithoutSoa)
}

#[cfg(test)]
mod tests;

fn learned_serves(entry: crate::ecs::Scope, filter: crate::ecs::Scope) -> bool {
    use crate::ecs::Scope;
    match (entry, filter) {
        (Scope::Learned { ipv4 }, Scope::Network(network) | Scope::ExactSource(network)) => {
            network.prefix_len() > 0 && network.addr().is_ipv4() == ipv4
        }
        _ => false,
    }
}
