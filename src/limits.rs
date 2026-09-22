//! Bounded admission budgets keyed only by the transport peer's subnet.
//!
//! Reclamation examines at most 16 entries on a new-source admission when the
//! table is full. Only inactive, fully refilled entries can be removed: cycling
//! source addresses cannot reset token debt. A full table can conservatively
//! reject a new source even when a reclaimable entry lies beyond that scan.

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::ensure;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

const TOKEN: u64 = 1_000_000_000;
const RECLAIM_SCAN: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub rate_per_sec: u32,
    pub burst: u32,
    pub max_sources: usize,
    pub ipv4_prefix: u8,
    pub ipv6_prefix: u8,
    pub max_inflight: usize,
    pub max_connections: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            rate_per_sec: 100,
            burst: 200,
            max_sources: 4096,
            ipv4_prefix: 32,
            ipv6_prefix: 64,
            max_inflight: 32,
            max_connections: 8,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [("rate_per_sec", self.rate_per_sec), ("burst", self.burst)] {
            ensure!(
                (1..=1_000_000).contains(&value),
                "source_limits.{name} must be in 1..=1000000"
            );
        }
        for (name, value) in [
            ("max_sources", self.max_sources),
            ("max_inflight", self.max_inflight),
            ("max_connections", self.max_connections),
        ] {
            ensure!(
                (1..=65_536).contains(&value),
                "source_limits.{name} must be in 1..=65536"
            );
        }
        ensure!(
            self.ipv4_prefix <= 32,
            "source_limits.ipv4_prefix must be in 0..=32"
        );
        ensure!(
            self.ipv6_prefix <= 128,
            "source_limits.ipv6_prefix must be in 0..=128"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Denied {
    Rate,
    Inflight,
    Connections,
    TableFull,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Query,
    Connection,
}

#[derive(Debug)]
struct Entry {
    credit: u64,
    updated: Instant,
    queries: usize,
    connections: usize,
}

impl Entry {
    fn refill(&mut self, now: Instant, settings: &Settings) {
        // Contending callers may capture their timestamps in a different order
        // from locking; never move the accounting clock backward.
        if now > self.updated {
            let added =
                now.duration_since(self.updated).as_nanos() * u128::from(settings.rate_per_sec);
            self.credit = (u128::from(self.credit) + added)
                .min(u128::from(settings.burst) * u128::from(TOKEN))
                as u64;
            self.updated = now;
        }
    }
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<IpNet, Entry>,
    sweep: VecDeque<IpNet>,
}

#[derive(Debug)]
pub struct Limiter {
    settings: Settings,
    // Disabled mode allocates no table and takes no lock.
    state: Option<Mutex<State>>,
}

/// Owns a concurrent budget until dropped, including cancellation and errors.
/// Dropping a query permit does not refund its rate token.
#[derive(Debug)]
pub struct Permit {
    owner: Option<(Arc<Limiter>, IpNet, Kind)>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some((limiter, source, kind)) = self.owner.take() {
            let mut state = limiter.state.as_ref().unwrap().lock().unwrap();
            // Active entries are never evicted, and each permit has one owner.
            let entry = state.entries.get_mut(&source).unwrap();
            match kind {
                Kind::Query => entry.queries -= 1,
                Kind::Connection => entry.connections -= 1,
            }
        }
    }
}

impl Limiter {
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        settings.validate()?;
        Ok(Self {
            settings: settings.clone(),
            state: settings.enabled.then(|| Mutex::new(State::default())),
        })
    }

    pub fn try_query(self: &Arc<Self>, peer: IpAddr) -> Result<Permit, Denied> {
        self.acquire_at(peer, Kind::Query, Instant::now())
    }

    pub fn try_connection(self: &Arc<Self>, peer: IpAddr) -> Result<Permit, Denied> {
        self.acquire_at(peer, Kind::Connection, Instant::now())
    }

    fn source(&self, peer: IpAddr) -> IpNet {
        let peer = match peer {
            IpAddr::V6(address) => address.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            _ => peer,
        };
        let prefix = if peer.is_ipv4() {
            self.settings.ipv4_prefix
        } else {
            self.settings.ipv6_prefix
        };
        IpNet::new(peer, prefix)
            .expect("prefix was validated")
            .trunc()
    }

    fn acquire_at(
        self: &Arc<Self>,
        peer: IpAddr,
        kind: Kind,
        now: Instant,
    ) -> Result<Permit, Denied> {
        let Some(state) = &self.state else {
            return Ok(Permit { owner: None });
        };
        let source = self.source(peer);
        let mut state = state.lock().unwrap();
        if !state.entries.contains_key(&source) {
            if state.entries.len() == self.settings.max_sources {
                for _ in 0..RECLAIM_SCAN.min(state.sweep.len()) {
                    let candidate = state.sweep.pop_front().unwrap();
                    let entry = state.entries.get_mut(&candidate).unwrap();
                    entry.refill(now, &self.settings);
                    if entry.queries == 0
                        && entry.connections == 0
                        && entry.credit == u64::from(self.settings.burst) * TOKEN
                    {
                        state.entries.remove(&candidate);
                        break;
                    }
                    state.sweep.push_back(candidate);
                }
                if state.entries.len() == self.settings.max_sources {
                    return Err(Denied::TableFull);
                }
            }
            state.entries.insert(
                source,
                Entry {
                    credit: u64::from(self.settings.burst) * TOKEN,
                    updated: now,
                    queries: 0,
                    connections: 0,
                },
            );
            state.sweep.push_back(source);
        }
        let entry = state.entries.get_mut(&source).unwrap();
        entry.refill(now, &self.settings);
        match kind {
            Kind::Query => {
                if entry.queries >= self.settings.max_inflight {
                    return Err(Denied::Inflight);
                }
                if entry.credit < TOKEN {
                    return Err(Denied::Rate);
                }
                entry.credit -= TOKEN;
                entry.queries += 1;
            }
            Kind::Connection => {
                if entry.connections >= self.settings.max_connections {
                    return Err(Denied::Connections);
                }
                entry.connections += 1;
            }
        }
        Ok(Permit {
            owner: Some((self.clone(), source, kind)),
        })
    }
}

#[cfg(test)]
mod tests;
