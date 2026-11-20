//! Learns and revokes ECS sharing by comparing normalized cached records.

use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Message, Query},
    rr::{Name, RData, Record, RecordType},
    serialize::binary::{BinEncodable, BinEncoder, NameEncoding},
};
use ipnet::IpNet;
use lru::LruCache;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::Key;
use crate::{config::EcsSharingConfig, ecs::Scope};

const MAX_RECORDS: usize = 65_536;

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
    Shared,
    Observing,
    Revoked,
}

enum Phase {
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
}

pub(super) struct Observation {
    pub scope: Scope,
    pub revoked: bool,
}

impl Table {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            records: LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap()),
        }
    }

    pub(super) fn revoke(
        &mut self,
        key: &Key,
        ipv4: bool,
        now: Instant,
        config: &EcsSharingConfig,
    ) -> bool {
        let class = Class {
            key: key.clone(),
            ipv4,
        };
        if matches!(self.records.get(&class), Some(Phase::Revoked { until }) if *until > now) {
            return false;
        }
        let until = now + Duration::from_secs(config.revoke_cooldown_secs);
        matches!(
            self.records.put(class, Phase::Revoked { until }),
            Some(Phase::Observing { .. } | Phase::Shared { .. })
        )
    }

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
        let Scope::ExactSource(network) = scope else {
            return unchanged;
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
                    *observed = fingerprint;
                } else if *observed != fingerprint {
                    return revoke(phase);
                }
                let Some(sample) = sample else {
                    return unchanged;
                };
                let min = usize::from(config.min_networks);
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
                Phase::Shared { since, .. }
                    if now.saturating_duration_since(*since) >= SHARED_MAX_AGE =>
                {
                    Some(State::Observing)
                }
                Phase::Shared { .. } => Some(State::Shared),
                Phase::Revoked { until } if *until > now => Some(State::Revoked),
                Phase::Revoked { .. } => None,
            }
        })
    }

    pub(super) fn invalidate(
        &mut self,
        name: Option<&Name>,
        kind: Option<RecordType>,
        scope: Option<Scope>,
    ) {
        let family = match scope {
            None => None,
            Some(Scope::Learned { ipv4 }) => Some(ipv4),
            Some(Scope::Network(network) | Scope::ExactSource(network))
                if network.prefix_len() > 0 =>
            {
                Some(network.addr().is_ipv4())
            }
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

pub(super) fn fingerprint(question: &Query, response: &Message) -> Option<Fingerprint> {
    let negative = super::is_negative(question, response);
    let encode = |rr: &Record| {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.set_canonical_form(true);
        encoder.set_name_encoding(NameEncoding::UncompressedLowercase);
        rr.name.emit(&mut encoder).ok()?;
        rr.record_type().emit(&mut encoder).ok()?;
        rr.dns_class.emit(&mut encoder).ok()?;
        if let RData::SOA(soa) = &rr.data
            && negative
        {
            soa.mname.emit(&mut encoder).ok()?;
            soa.rname.emit(&mut encoder).ok()?;
            encoder.emit_u32(soa.minimum).ok()?;
        } else {
            rr.data.emit(&mut encoder).ok()?;
        }
        Some(bytes)
    };
    let mut hash = Sha256::new();
    hash.update(u16::from(response.response_code).to_be_bytes());
    hash.update([u8::from(negative)]);
    for section in [
        &response.answers,
        &response.authorities,
        &response.additionals,
    ] {
        let mut records = section
            .iter()
            .filter(|rr| rr.record_type() != RecordType::OPT)
            .map(encode)
            .collect::<Option<Vec<_>>>()?;
        records.sort_unstable();
        records.dedup();
        hash.update((records.len() as u32).to_be_bytes());
        for record in records {
            hash.update((record.len() as u32).to_be_bytes());
            hash.update(record);
        }
    }
    let digest = hash.finalize();
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&digest[..16]);
    Some(fingerprint)
}
