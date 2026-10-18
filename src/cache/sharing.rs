//! Learned ECS-invariant sharing. An answer an upstream returns without ECS to a
//! nonzero outgoing ECS query is reusable only for that exact source network
//! (`ExactSource`) unless the same normalized answer has been observed for at
//! least `min_networks` disjoint networks of one family, spanning at least two
//! /16 (IPv6 /32) prefixes, with no contradiction while those answers were
//! valid. The record then turns `Shared` and new answers are stored once as
//! `Scope::Learned` for the whole family. Any different answer, or an explicit
//! nonzero response scope, revokes it: shared entries are superseded by the
//! contradicting answer, and the record is not relearned until a cooldown ends.
//!
//! Records are keyed by the complete cache key (name, type, class, DO/CD/RD,
//! EDNS presence) plus the ECS address family. No-ECS, privacy `/0` and the
//! other family never participate. Each shard owns its slice of the bounded
//! LRU table under the shard lock, so learning, revocation, the epoch check and
//! admission are one linearized step and older in-flight work can never refill
//! a revoked or invalidated shared entry. The table is never persisted and
//! never changed by inspection.

use std::time::{Duration, Instant};

use hickory_proto::{
    op::{Message, Query},
    rr::{Name, RData, RecordType},
    serialize::binary::BinEncodable,
};
use ipnet::IpNet;
use lru::LruCache;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::Key;
use crate::{config::EcsSharingConfig, ecs::Scope};

/// Upper bound for the whole classification table, split across shards.
const MAX_RECORDS: usize = 65_536;

/// Shared evidence must be renewed: after this long the record observes again
/// and needs `min_networks` matching networks, so a name whose answers later
/// diverge by region cannot stay shared through whichever network refreshes first.
const SHARED_MAX_AGE: Duration = Duration::from_secs(3600);

pub(super) fn capacity(max_entries: usize, shards: usize) -> usize {
    max_entries
        .saturating_mul(2)
        .clamp(1024, MAX_RECORDS)
        .div_ceil(shards)
}

pub(super) type Fingerprint = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Observing,
    Shared,
    Revoked,
}

enum Phase {
    /// Every live sample produced `fingerprint`; at most `min_networks` samples.
    Observing {
        fingerprint: Fingerprint,
        samples: Vec<Sample>,
    },
    Shared {
        fingerprint: Fingerprint,
        since: Instant,
    },
    Revoked {
        until: Instant,
    },
}

struct Sample {
    network: IpNet,
    expires: Instant,
}

#[derive(Hash, PartialEq, Eq)]
struct Class {
    key: Key,
    ipv4: bool,
}

pub(super) struct Table {
    records: LruCache<Class, Phase>,
    capacity: usize,
}

pub(super) struct Observation {
    /// The namespace the answer is stored under.
    pub scope: Scope,
    pub revoked: bool,
}

impl Table {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            // Grow on demand like the cache partitions; `capacity` is enforced below.
            records: LruCache::unbounded(),
            capacity: capacity.max(1),
        }
    }

    /// Classify one successful answer that already passed the epoch check under
    /// the shard lock. `fingerprint` is present for ExactSource answers (None if
    /// the answer cannot be normalized); `lifetime` is the admitted fresh
    /// lifetime, or None when the answer is not admitted and cannot be evidence.
    pub(super) fn observe(
        &mut self,
        key: &Key,
        scope: Scope,
        fingerprint: Option<Fingerprint>,
        lifetime: Option<Duration>,
        now: Instant,
        config: &EcsSharingConfig,
    ) -> Observation {
        let unchanged = Observation {
            scope,
            revoked: false,
        };
        let revoke_until = now + Duration::from_secs(config.revoke_cooldown_secs);
        let network = match scope {
            Scope::ExactSource(network) => network,
            // An explicit nonzero scope says this answer depends on the client
            // subnet, which contradicts learned invariance for that family.
            Scope::Network(network) if network.prefix_len() > 0 => {
                let class = Class {
                    key: key.clone(),
                    ipv4: network.addr().is_ipv4(),
                };
                return match self.records.get_mut(&class) {
                    Some(phase) if !matches!(*phase, Phase::Revoked { until } if until > now) => {
                        *phase = Phase::Revoked {
                            until: revoke_until,
                        };
                        Observation {
                            scope,
                            revoked: true,
                        }
                    }
                    _ => unchanged,
                };
            }
            _ => return unchanged,
        };
        let class = Class {
            key: key.clone(),
            ipv4: network.addr().is_ipv4(),
        };
        let sample = lifetime.map(|lifetime| Sample {
            network,
            expires: now + lifetime,
        });
        let Some(phase) = self.records.get_mut(&class) else {
            let Some(fingerprint) = fingerprint else {
                return unchanged;
            };
            self.records.put(
                class,
                Phase::Observing {
                    fingerprint,
                    samples: sample.into_iter().collect(),
                },
            );
            while self.records.len() > self.capacity {
                self.records.pop_lru();
            }
            return unchanged;
        };
        let revoke = |phase: &mut Phase| {
            *phase = Phase::Revoked {
                until: revoke_until,
            };
            Observation {
                scope,
                revoked: true,
            }
        };
        match &mut *phase {
            Phase::Revoked { until } if *until > now => unchanged,
            Phase::Revoked { .. } => {
                if let Some(fingerprint) = fingerprint {
                    *phase = Phase::Observing {
                        fingerprint,
                        samples: sample.into_iter().collect(),
                    };
                }
                unchanged
            }
            Phase::Shared { since, .. }
                if now.saturating_duration_since(*since) >= SHARED_MAX_AGE =>
            {
                // Renew the evidence: this answer starts a new observation and is
                // stored for its own network until enough networks agree again.
                if let Some(fingerprint) = fingerprint {
                    *phase = Phase::Observing {
                        fingerprint,
                        samples: sample.into_iter().collect(),
                    };
                }
                unchanged
            }
            Phase::Shared {
                fingerprint: shared,
                ..
            } => {
                if fingerprint == Some(*shared) {
                    Observation {
                        scope: Scope::Learned { ipv4: class.ipv4 },
                        revoked: false,
                    }
                } else {
                    revoke(phase)
                }
            }
            Phase::Observing {
                fingerprint: observed,
                samples,
            } => {
                samples.retain(|sample| sample.expires > now);
                let Some(fingerprint) = fingerprint else {
                    return if samples.is_empty() {
                        unchanged
                    } else {
                        revoke(phase)
                    };
                };
                if samples.is_empty() {
                    // Earlier evidence expired; this answer starts a new observation.
                    *observed = fingerprint;
                } else if *observed != fingerprint {
                    return revoke(phase);
                }
                let Some(sample) = sample else {
                    return unchanged;
                };
                let min = usize::from(config.min_networks);
                // Nested prefixes (a client may send any ECS covering its own
                // address) are one network, so one client cannot count twice.
                // Replacing every overlapping sample keeps samples disjoint.
                samples.retain(|s| {
                    !s.network.contains(&network.addr()) && !network.contains(&s.network.addr())
                });
                if samples.len() >= min
                    && let Some(oldest) = samples
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, s)| s.expires)
                        .map(|(index, _)| index)
                {
                    samples.swap_remove(oldest);
                }
                samples.push(sample);
                if promotable(samples, min) {
                    *phase = Phase::Shared {
                        fingerprint,
                        since: now,
                    };
                    Observation {
                        scope: Scope::Learned { ipv4: class.ipv4 },
                        revoked: false,
                    }
                } else {
                    unchanged
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.records.len()
    }

    /// Read-only: the LRU order and every record are left untouched.
    pub(super) fn states<'a>(
        &'a self,
        name: &'a Name,
        kind: Option<RecordType>,
        now: Instant,
    ) -> impl Iterator<Item = State> + 'a {
        self.records.iter().filter_map(move |(class, phase)| {
            if &class.key.name != name || kind.is_some_and(|k| k != class.key.kind) {
                return None;
            }
            match phase {
                Phase::Observing { .. } => Some(State::Observing),
                Phase::Shared { .. } => Some(State::Shared),
                Phase::Revoked { until } if *until > now => Some(State::Revoked),
                // An elapsed cooldown is equivalent to no record.
                Phase::Revoked { .. } => None,
            }
        })
    }

    /// Clearing a name (or everything) also forgets what was learned about it,
    /// including any revocation cooldown; new answers start a new observation.
    pub(super) fn invalidate(
        &mut self,
        name: Option<&Name>,
        kind: Option<RecordType>,
        scope: Option<Scope>,
    ) {
        let family = match scope {
            None => None,
            Some(Scope::Learned { ipv4 }) => Some(ipv4),
            // A learned entry serves every network of its family, so clearing one
            // network also forgets what was learned for that family.
            Some(Scope::Network(network) | Scope::ExactSource(network))
                if network.prefix_len() > 0 =>
            {
                Some(network.addr().is_ipv4())
            }
            // No-ECS and privacy namespaces never hold learned entries.
            Some(_) => return,
        };
        if name.is_none() && kind.is_none() && family.is_none() {
            self.records.clear();
            return;
        }
        let matched: Vec<_> = self
            .records
            .iter()
            .filter(|(class, _)| {
                name.is_none_or(|n| &class.key.name == n)
                    && kind.is_none_or(|k| class.key.kind == k)
                    && family.is_none_or(|ipv4| class.ipv4 == ipv4)
            })
            .map(|(class, _)| Class {
                key: class.key.clone(),
                ipv4: class.ipv4,
            })
            .collect();
        for class in matched {
            self.records.pop(&class);
        }
    }
}

/// Samples are pairwise disjoint by construction; the span requires two
/// different /16 (IPv6 /32) prefixes so one operator block cannot decide alone.
/// With disjoint samples, unequal spans are also disjoint.
fn promotable(samples: &[Sample], min: usize) -> bool {
    if samples.len() < min {
        return false;
    }
    let span = |network: &IpNet| {
        let bits = if network.addr().is_ipv4() { 16 } else { 32 };
        IpNet::new(network.addr(), network.prefix_len().min(bits)).map(|n| n.trunc())
    };
    let first = span(&samples[0].network);
    samples.iter().any(|sample| span(&sample.network) != first)
}

/// Normalized answer identity: rcode, positive/negative classification (as
/// admission computes it), the answer section as an unordered set of owner,
/// type, class and RDATA (so CNAME chains are included), and for negative
/// answers the SOA owner names. TTLs, record order, IDs and EDNS are ignored.
pub(super) fn fingerprint(question: &Query, response: &Message) -> Option<Fingerprint> {
    let negative = super::is_negative(question, response);
    let encode = |name: &Name, kind: RecordType, class: u16, data: Option<&RData>| {
        let mut bytes = name.to_lowercase().to_bytes().ok()?;
        bytes.extend_from_slice(&u16::from(kind).to_be_bytes());
        bytes.extend_from_slice(&class.to_be_bytes());
        if let Some(data) = data {
            bytes.extend_from_slice(&data.to_bytes().ok()?);
        }
        Some(bytes)
    };
    let mut answers = response
        .answers
        .iter()
        .map(|rr| {
            encode(
                &rr.name,
                rr.record_type(),
                rr.dns_class.into(),
                Some(&rr.data),
            )
        })
        .collect::<Option<Vec<_>>>()?;
    answers.sort_unstable();
    answers.dedup();
    let mut soa = Vec::new();
    if negative {
        for rr in &response.authorities {
            if rr.record_type() == RecordType::SOA {
                soa.push(encode(
                    &rr.name,
                    RecordType::SOA,
                    rr.dns_class.into(),
                    None,
                )?);
            }
        }
        soa.sort_unstable();
        soa.dedup();
    }
    let mut hash = Sha256::new();
    hash.update(u16::from(response.response_code).to_be_bytes());
    hash.update([u8::from(negative)]);
    for section in [&answers, &soa] {
        hash.update((section.len() as u32).to_be_bytes());
        for record in section {
            hash.update((record.len() as u32).to_be_bytes());
            hash.update(record);
        }
    }
    let digest = hash.finalize();
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&digest[..16]);
    Some(fingerprint)
}
