//! Validated equivalent upstream pool. The resolver owns the total deadline.
//! No system DNS, detached races, plaintext fallback, or implicit trust bypass.
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, ensure};
use futures_util::{StreamExt, stream::FuturesUnordered};
use hickory_proto::op::{Message, ResponseCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

mod h3;
mod transport;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Weighted,
    Parallel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub servers: Vec<String>,
    pub mode: Mode,
    pub bootstrap: Vec<SocketAddr>,
    pub max_parallel: usize,
    pub max_extra_inflight: usize,
    pub ca_file: Option<PathBuf>,
    pub prefer_h3: bool,
    pub dot_pool: crate::tls::PoolSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            mode: Mode::Weighted,
            bootstrap: Vec::new(),
            max_parallel: 32,
            max_extra_inflight: 128,
            ca_file: None,
            prefer_h3: false,
            dot_pool: crate::tls::PoolSettings::default(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        self.dot_pool.validate()?;
        ensure!(
            (1..=32).contains(&self.servers.len()),
            "upstreams.servers requires 1..=32 entries"
        );
        ensure!(
            (1..=32).contains(&self.max_parallel),
            "upstreams.max_parallel must be in 1..=32"
        );
        ensure!(
            self.mode != Mode::Parallel || self.servers.len() <= self.max_parallel,
            "parallel mode requires max_parallel >= number of servers"
        );
        ensure!(
            (1..=65536).contains(&self.max_extra_inflight),
            "upstreams.max_extra_inflight must be in 1..=65536"
        );
        ensure!(
            self.bootstrap.len() <= 8,
            "upstreams.bootstrap supports at most 8 addresses"
        );
        for address in &self.bootstrap {
            valid_address(*address)?;
        }
        let mut endpoints = std::collections::HashSet::new();
        for line in &self.servers {
            let spec = Endpoint::parse(line)?;
            ensure!(
                endpoints.insert((
                    spec.protocol,
                    spec.host.clone(),
                    spec.port,
                    spec.path.clone()
                )),
                "duplicate upstream endpoint"
            );
            ensure!(
                spec.host.parse::<IpAddr>().is_ok() || !self.bootstrap.is_empty(),
                "hostname upstreams require explicit bootstrap IP:port addresses"
            );
        }
        Ok(())
    }

    pub fn validate_listeners(&self, config: &crate::config::Config) -> Result<()> {
        for line in &self.servers {
            let spec = Endpoint::parse(line)?;
            if let Ok(ip) = spec.host.parse::<IpAddr>() {
                not_self(
                    SocketAddr::new(ip, spec.port),
                    &listeners(config, spec.protocol, self.prefer_h3),
                )?;
            }
        }
        for address in &self.bootstrap {
            not_self(*address, &[config.listen])?;
        }
        Ok(())
    }
}

fn listeners(
    config: &crate::config::Config,
    protocol: Protocol,
    prefer_h3: bool,
) -> Vec<SocketAddr> {
    match protocol {
        Protocol::Udp | Protocol::Tcp => vec![config.listen],
        Protocol::Tls => config.dot.as_ref().map(|v| v.listen).into_iter().collect(),
        Protocol::Https => config
            .doh
            .as_ref()
            .map(|v| v.listen)
            .into_iter()
            .chain(config.doh3.as_ref().filter(|_| prefer_h3).map(|v| v.listen))
            .collect(),
        Protocol::Quic => config.doq.as_ref().map(|v| v.listen).into_iter().collect(),
    }
}

fn not_self(address: SocketAddr, listeners: &[SocketAddr]) -> Result<()> {
    ensure!(
        !listeners.iter().any(|listener| {
            let target = address.ip().to_canonical();
            let bound = listener.ip().to_canonical();
            address.port() == listener.port()
                && (target == bound || bound.is_unspecified() && target.is_loopback())
        }),
        "upstream/bootstrap cannot target a PariNS listener"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Protocol {
    Udp,
    Tcp,
    Tls,
    Https,
    Quic,
}

#[derive(Debug)]
struct Endpoint {
    label: String,
    protocol: Protocol,
    host: String,
    port: u16,
    path: String,
    weight: i64,
}

impl Endpoint {
    fn parse(line: &str) -> Result<Self> {
        ensure!(line.len() <= 2048, "upstream line too long");
        let mut parts = line.split_whitespace();
        let endpoint = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty upstream"))?;
        let weight = match parts.next() {
            Some(value) => value
                .strip_prefix("weight=")
                .ok_or_else(|| anyhow::anyhow!("use weight=N after the endpoint"))?
                .parse::<i64>()?,
            None => 1,
        };
        ensure!(
            parts.next().is_none() && (1..=1000).contains(&weight),
            "upstream weight must be in 1..=1000"
        );
        let (scheme, address) = endpoint.split_once("://").unwrap_or(("udp", endpoint));
        let (protocol, default_port) = match scheme {
            "udp" => (Protocol::Udp, 53),
            "tcp" => (Protocol::Tcp, 53),
            "tls" => (Protocol::Tls, 853),
            "https" => (Protocol::Https, 443),
            "quic" => (Protocol::Quic, 853),
            _ => anyhow::bail!("supported upstream protocols: udp, tcp, tls, https, quic"),
        };
        ensure!(
            !address.contains(['@', '?', '#']),
            "upstream credentials, query parameters and fragments are unsupported"
        );
        let (authority, path) = address
            .split_once('/')
            .map_or((address, "/dns-query".to_string()), |(a, p)| {
                (a, format!("/{p}"))
            });
        ensure!(
            protocol == Protocol::Https || !address.contains('/'),
            "only HTTPS upstreams accept a path"
        );
        let authority: http::uri::Authority = authority.parse()?;
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        ensure!(!host.is_empty(), "upstream host is empty");
        ensure!(
            authority.port().is_none() || authority.port_u16().is_some(),
            "invalid upstream port"
        );
        let port = authority.port_u16().unwrap_or(default_port);
        ensure!(port != 0, "upstream port cannot be zero");
        if let Ok(ip) = host.parse::<IpAddr>() {
            valid_address(SocketAddr::new(ip, port))?;
        } else {
            ensure!(
                host.len() <= 253
                    && host.split('.').all(|label| !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-')),
                "invalid upstream hostname"
            );
        }
        let _: http::uri::PathAndQuery = path.parse()?;
        Ok(Self {
            label: endpoint.to_string(),
            protocol,
            host,
            port,
            path,
            weight,
        })
    }
}

fn valid_address(address: SocketAddr) -> Result<()> {
    let ip = address.ip().to_canonical();
    ensure!(
        address.port() != 0
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !matches!(ip, IpAddr::V4(v) if v.is_broadcast()),
        "upstream must be a nonzero unicast endpoint"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Exchange {
    pub message: Message,
    pub upstream: String,
}

pub struct Pool {
    endpoints: Vec<transport::Client>,
    settings: Settings,
    scores: Mutex<Vec<i64>>,
    cursor: AtomicUsize,
    extra: Arc<Semaphore>,
}

impl Pool {
    pub fn new(settings: &Settings, config: &crate::config::Config) -> Result<Self> {
        settings.validate()?;
        settings.validate_listeners(config)?;
        let endpoints = settings
            .servers
            .iter()
            .map(|line| {
                let spec = Endpoint::parse(line)?;
                let bound = listeners(config, spec.protocol, settings.prefer_h3);
                transport::Client::new(
                    spec,
                    settings,
                    bound,
                    std::time::Duration::from_millis(config.query_timeout_ms),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            scores: Mutex::new(vec![0; endpoints.len()]),
            endpoints,
            settings: settings.clone(),
            cursor: AtomicUsize::new(0),
            extra: Arc::new(Semaphore::new(settings.max_extra_inflight)),
        })
    }

    pub async fn exchange(&self, query: &Message) -> Result<Exchange> {
        if self.settings.mode == Mode::Weighted {
            // Smooth weighted round-robin: no long runs from large weights.
            let index = {
                let mut scores = self.scores.lock().unwrap_or_else(|e| e.into_inner());
                let mut selected = 0;
                let mut total = 0;
                for (i, client) in self.endpoints.iter().enumerate() {
                    scores[i] += client.spec.weight;
                    total += client.spec.weight;
                    if scores[i] > scores[selected] {
                        selected = i;
                    }
                }
                scores[selected] -= total;
                selected
            };
            return self.endpoints[index].exchange(query).await;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.endpoints.len();
        let mut requests = FuturesUnordered::new();
        for offset in 0..self.endpoints.len().min(self.settings.max_parallel) {
            let permit = if offset == 0 {
                None
            } else {
                let Ok(permit) = self.extra.try_acquire() else {
                    break;
                };
                Some(permit)
            };
            let client = &self.endpoints[(start + offset) % self.endpoints.len()];
            requests.push(async move {
                let _permit = permit;
                client.exchange(query).await
            });
        }
        let mut failure = Err(anyhow::anyhow!("no upstream response"));
        while let Some(result) = requests.next().await {
            if result.as_ref().is_ok_and(|r| {
                matches!(
                    r.message.response_code,
                    ResponseCode::NoError | ResponseCode::NXDomain
                )
            }) {
                return result;
            }
            if failure.is_err() || result.is_ok() {
                failure = result;
            }
        }
        failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_protocols_weights_and_bootstrap() {
        for line in [
            "1.1.1.1",
            "udp://[::1]:53",
            "tcp://127.0.0.1:53 weight=3",
            "tls://dns.example",
            "https://dns.example/dns-query",
            "quic://dns.example",
        ] {
            assert!(Endpoint::parse(line).is_ok(), "{line}");
        }
        for line in [
            "udp://0.0.0.0",
            "https://user:pass@dns.example",
            "https://dns.example/?token=foo",
            "http://dns.example",
            "tls://dns.example/path",
            "1.1.1.1 weight=0",
            "1.1.1.1 weight=1 trailing",
            "tls://bad_name",
            "udp://1.1.1.1:0",
        ] {
            assert!(Endpoint::parse(line).is_err(), "{line}");
        }
        let mut settings = Settings {
            servers: vec!["https://dns.example/dns-query".into()],
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
        settings.bootstrap.push("1.1.1.1:53".parse().unwrap());
        settings.validate().unwrap();
    }
}
