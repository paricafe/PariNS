//! Bounded message cache. DNS/ECS semantics are owned here, not by the LRU crate.
//! Each query bucket holds independent subnet answers. The resolver owns one
//! instance per immutable upstream configuration; entries cannot cross profiles.

use std::{
    collections::{VecDeque, hash_map::RandomState},
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Edns, Message, ResponseCode},
    rr::{
        DNSClass, RData, RecordType,
        rdata::opt::{ClientSubnet, EdnsCode},
    },
};
use lru::LruCache;

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
        Some(Self {
            name: question.name().to_lowercase().to_ascii().into_boxed_str(),
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

struct Entry {
    wire: Box<[u8]>,
    scope: Scope,
    inserted: Instant,
    lifetime: Duration,
    charge: usize,
}

pub struct Cache {
    config: CacheConfig,
    buckets: LruCache<Key, VecDeque<Entry>, RandomState>,
    entries: usize,
    bytes: usize,
}

impl Cache {
    /// Config values are validated by Config before the server is bound.
    pub fn new(config: CacheConfig) -> Self {
        let cap = NonZeroUsize::new(config.max_entries).expect("validated cache capacity");
        Self {
            config,
            buckets: LruCache::with_hasher(cap, RandomState::new()),
            entries: 0,
            bytes: 0,
        }
    }

    pub fn get(
        &mut self,
        query: &Message,
        outgoing: Option<ClientSubnet>,
        now: Instant,
    ) -> Option<(Message, Scope)> {
        if !self.config.enabled {
            return None;
        }
        let key = Key::of(query)?;
        let bucket = self.buckets.get_mut(&key)?;
        bucket.retain(|entry| {
            let valid = now.saturating_duration_since(entry.inserted) < entry.lifetime;
            if !valid {
                self.entries -= 1;
                self.bytes -= entry.charge;
            }
            valid
        });
        if bucket.is_empty() {
            self.buckets.pop(&key);
            return None;
        }
        let index = bucket
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.scope.matches(outgoing))
            .max_by_key(|(_, entry)| entry.scope.prefix_len())
            .map(|(index, _)| index)?;
        let entry = &bucket[index];
        let elapsed = now.saturating_duration_since(entry.inserted).as_secs() as u32;
        let mut message = protocol::decode(&entry.wire).ok()?;
        for rr in message
            .answers
            .iter_mut()
            .chain(&mut message.authorities)
            .chain(&mut message.additionals)
        {
            rr.ttl = rr.ttl.saturating_sub(elapsed);
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
        let scope = entry.scope;
        let entry = bucket.remove(index)?;
        bucket.push_back(entry);
        Some((message, scope))
    }

    pub fn insert(&mut self, query: &Message, response: &Message, scope: Scope, now: Instant) {
        if !self.config.enabled {
            return;
        }
        let Some(key) = Key::of(query) else { return };
        let Some((stored, lifetime)) = prepare_response(query, response, &self.config) else {
            return;
        };
        let Ok(wire) = stored.to_vec() else { return };
        let charge = wire.len() + key.name.len();
        if charge > self.config.max_bytes {
            return;
        }
        let mut bucket = self.buckets.pop(&key).unwrap_or_default();
        self.entries -= bucket.len();
        self.bytes -= bucket.iter().map(|entry| entry.charge).sum::<usize>();
        bucket.retain(|entry| {
            entry.scope != scope && now.saturating_duration_since(entry.inserted) < entry.lifetime
        });
        bucket.push_back(Entry {
            wire: wire.into_boxed_slice(),
            scope,
            inserted: now,
            lifetime: Duration::from_secs(lifetime.into()),
            charge,
        });
        let mut bucket_bytes: usize = bucket.iter().map(|entry| entry.charge).sum();
        while bucket.len() > self.config.max_variants || bucket_bytes > self.config.max_bytes {
            bucket_bytes -= bucket
                .pop_front()
                .expect("nonempty oversized bucket")
                .charge;
        }
        while self.entries + bucket.len() > self.config.max_entries
            || self.bytes + bucket_bytes > self.config.max_bytes
        {
            let (_, evicted) = self
                .buckets
                .pop_lru()
                .expect("budgets require an existing bucket");
            self.entries -= evicted.len();
            self.bytes -= evicted.iter().map(|entry| entry.charge).sum::<usize>();
        }
        self.entries += bucket.len();
        self.bytes += bucket_bytes;
        self.buckets.put(key, bucket);
    }
}

fn prepare_response(
    query: &Message,
    response: &Message,
    config: &CacheConfig,
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
