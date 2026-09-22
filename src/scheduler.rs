//! Opt-in hedging between two explicitly equivalent upstream replicas.
//!
//! The caller owns the total deadline. Each query keeps its futures locally so
//! choosing a winner or dropping the request immediately cancels the loser.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use hickory_proto::op::{Message, ResponseCode};
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, time::sleep};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub secondary: SocketAddr,
    pub hedge_after_ms: u64,
    pub max_extra_inflight: usize,
}

impl Settings {
    pub fn validate(&self, primary: SocketAddr) -> Result<()> {
        for address in [primary, self.secondary] {
            let ip = address.ip().to_canonical();
            ensure!(
                address.port() != 0
                    && !ip.is_unspecified()
                    && !ip.is_multicast()
                    && !matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast()),
                "scheduler replicas must be nonzero unicast endpoints"
            );
        }
        ensure!(
            primary.ip().to_canonical() != self.secondary.ip().to_canonical()
                || primary.port() != self.secondary.port(),
            "scheduler secondary must differ from primary"
        );
        ensure!(
            (1..=60_000).contains(&self.hedge_after_ms),
            "scheduler.hedge_after_ms must be in 1..=60000"
        );
        ensure!(
            (1..=65_536).contains(&self.max_extra_inflight),
            "scheduler.max_extra_inflight must be in 1..=65536"
        );
        Ok(())
    }
}

#[derive(Clone)]
struct Replica {
    settings: Settings,
    extra: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct Client {
    primary: SocketAddr,
    tls: Option<crate::tls::Upstream>,
    replica: Option<Replica>,
}

impl Client {
    pub fn new(
        primary: SocketAddr,
        tls: Option<crate::tls::Upstream>,
        settings: Option<Settings>,
    ) -> Result<Self> {
        let replica = match settings {
            Some(settings) => {
                settings.validate(primary)?;
                Some(Replica {
                    extra: Arc::new(Semaphore::new(settings.max_extra_inflight)),
                    settings,
                })
            }
            None => None,
        };
        Ok(Self {
            primary,
            tls,
            replica,
        })
    }

    pub async fn exchange(&self, query: &Message) -> Result<Message> {
        let primary = crate::upstream::exchange_with_tls(query, self.primary, self.tls.as_ref());
        let Some(replica) = &self.replica else {
            return primary.await;
        };
        tokio::pin!(primary);
        let first = tokio::select! {
            biased;
            result = &mut primary => Some(result),
            _ = sleep(Duration::from_millis(replica.settings.hedge_after_ms)) => None,
        };
        if first.as_ref().is_some_and(success) {
            return first.expect("completed primary");
        }
        // Never queue hedges: saturation preserves the original primary path.
        let Ok(permit) = replica.extra.try_acquire() else {
            return match first {
                Some(result) => result,
                None => primary.await,
            };
        };
        let secondary = async {
            let _permit = permit;
            crate::upstream::exchange_with_tls(query, replica.settings.secondary, self.tls.as_ref())
                .await
        };
        tokio::pin!(secondary);
        if let Some(result) = first {
            return prefer(result, secondary.await);
        }
        tokio::select! {
            result = &mut primary => {
                if success(&result) { result } else { prefer(result, secondary.await) }
            }
            result = &mut secondary => {
                if success(&result) { result } else { prefer(primary.await, result) }
            }
        }
    }
}

fn success(result: &Result<Message>) -> bool {
    result
        .as_ref()
        .is_ok_and(|message| message.response_code != ResponseCode::ServFail)
}

fn prefer(primary: Result<Message>, secondary: Result<Message>) -> Result<Message> {
    if success(&secondary) || primary.is_err() && secondary.is_ok() {
        secondary
    } else {
        primary
    }
}
