//! Per-endpoint DNS-over-TLS connection pooling.
use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, SemaphorePermit};

use super::ClientStream;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    pub enabled: bool,
    pub max_connections: usize,
    pub idle_timeout_ms: u64,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_connections: 8,
            idle_timeout_ms: 30_000,
        }
    }
}

impl PoolSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.max_connections),
            "upstreams.dot_pool.max_connections must be in 1..=256"
        );
        ensure!(
            (1..=600_000).contains(&self.idle_timeout_ms),
            "upstreams.dot_pool.idle_timeout_ms must be in 1..=600000"
        );
        Ok(())
    }
}

pub(super) struct Idle {
    pub stream: ClientStream,
    pub address: SocketAddr,
    pub returned: tokio::time::Instant,
}

pub(super) struct Pool {
    slots: Mutex<Vec<Slot>>,
    available: Semaphore,
    idle_timeout: Duration,
}

struct Slot {
    busy: bool,
    idle: Option<Idle>,
}

pub(super) struct Lease<'a> {
    pool: &'a Pool,
    index: usize,
    idle: Option<Idle>,
    _permit: SemaphorePermit<'a>,
}

impl Deref for Lease<'_> {
    type Target = Option<Idle>;

    fn deref(&self) -> &Self::Target {
        &self.idle
    }
}

impl DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.idle
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut slots = self.pool.slots.lock().unwrap();
        slots[self.index].idle = self.idle.take();
        slots[self.index].busy = false;
    }
}

impl Pool {
    pub fn new(settings: &PoolSettings) -> Self {
        Self {
            slots: Mutex::new(
                (0..settings.max_connections)
                    .map(|_| Slot {
                        busy: false,
                        idle: None,
                    })
                    .collect(),
            ),
            available: Semaphore::new(settings.max_connections),
            idle_timeout: Duration::from_millis(settings.idle_timeout_ms),
        }
    }

    pub async fn checkout(&self, address: SocketAddr) -> Lease<'_> {
        let permit = self.available.acquire().await.unwrap();
        let mut expired = Vec::new();
        let (index, idle) = {
            let mut slots = self.slots.lock().unwrap();
            let now = tokio::time::Instant::now();
            let (mut matching, mut empty, mut oldest) = (None, None, None);
            for (index, slot) in slots.iter_mut().enumerate().filter(|(_, slot)| !slot.busy) {
                if slot
                    .idle
                    .as_ref()
                    .is_some_and(|idle| now - idle.returned >= self.idle_timeout)
                {
                    expired.extend(slot.idle.take());
                }
                if let Some(idle) = &slot.idle {
                    let candidate = (idle.returned, index);
                    if idle.address == address {
                        matching = Some(matching.map_or(candidate, |prior| candidate.max(prior)));
                    } else {
                        oldest = Some(oldest.map_or(candidate, |prior| candidate.min(prior)));
                    }
                } else {
                    empty.get_or_insert(index);
                }
            }
            let index = matching
                .map(|(_, index)| index)
                .or(empty)
                .or_else(|| oldest.map(|(_, index)| index))
                .expect("an acquired permit owns one available slot");
            slots[index].busy = true;
            (index, slots[index].idle.take())
        };
        drop(expired);
        Lease {
            pool: self,
            index,
            idle: idle.filter(|idle| idle.address == address),
            _permit: permit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
    use std::sync::Arc;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::Instant;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    #[tokio::test]
    async fn checkout_cleans_expired_slots_and_prefers_fresh_matches_then_empty_then_oldest() {
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = identity.cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let connector = TlsConnector::from(Arc::new(client));
        let stream = || async {
            let (client, server) = tokio::join!(
                async {
                    connector
                        .connect(
                            ServerName::try_from("localhost").unwrap(),
                            TcpStream::connect(tls_address).await.unwrap(),
                        )
                        .await
                        .unwrap()
                },
                async {
                    acceptor
                        .accept(listener.accept().await.unwrap().0)
                        .await
                        .unwrap()
                },
            );
            (client, server)
        };
        let pool = Pool::new(&PoolSettings {
            max_connections: 5,
            idle_timeout_ms: 60_000,
            ..Default::default()
        });
        let a = "127.0.0.1:1001".parse().unwrap();
        let b = "127.0.0.1:1002".parse().unwrap();
        let c = "127.0.0.1:1003".parse().unwrap();
        let now = Instant::now();
        let expired = now - Duration::from_secs(60);
        let mut peers = Vec::new();
        for (index, (address, returned)) in [
            (a, expired),
            (a, now - Duration::from_secs(1)),
            (a, now),
            (b, expired),
            (b, now),
        ]
        .into_iter()
        .enumerate()
        {
            let (stream, peer) = stream().await;
            peers.push(peer);
            pool.slots.lock().unwrap()[index].idle = Some(Idle {
                stream,
                address,
                returned,
            });
        }

        let mut match_a = pool.checkout(a).await;
        assert_eq!(match_a.index, 2);
        assert_eq!(match_a.as_ref().unwrap().returned, now);
        {
            let mut slots = pool.slots.lock().unwrap();
            assert!(slots[0].idle.is_none() && slots[3].idle.is_none());
            slots[0].idle = match_a.take();
        }
        drop(match_a);
        let slot_c = pool.checkout(c).await;
        assert_eq!(slot_c.index, 2);
        assert!(slot_c.is_none());
        assert!(pool.slots.lock().unwrap()[4].idle.is_some());
        drop(slot_c);
        let (new_stream, peer) = stream().await;
        peers.push(peer);
        let (other_stream, peer) = stream().await;
        peers.push(peer);
        {
            let mut slots = pool.slots.lock().unwrap();
            slots[2].idle = Some(Idle {
                stream: new_stream,
                address: b,
                returned: now,
            });
            slots[3].idle = Some(Idle {
                stream: other_stream,
                address: a,
                returned: now,
            });
        }
        let oldest = pool.checkout(c).await;
        assert_eq!(oldest.index, 1);
        assert!(oldest.is_none());
    }
}
