//! Startup configuration. Invalid values fail before any listeners are bound.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub upstream: SocketAddr,
    pub query_timeout_ms: u64,
    pub tcp_io_timeout_ms: u64,
    pub shutdown_grace_ms: u64,
    pub max_inflight: usize,
    pub max_tcp_connections: usize,
    #[serde(default)]
    pub source_limits: crate::limits::Settings,
    #[serde(default)]
    pub ecs: EcsConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub filter: crate::policy::Policy,
    #[serde(default)]
    pub coalescing: CoalescingConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub dot: Option<crate::tls::ListenerConfig>,
    #[serde(default)]
    pub doh: Option<crate::tls::ListenerConfig>,
    #[serde(default)]
    pub doq: Option<crate::tls::ListenerConfig>,
    #[serde(default)]
    pub doh3: Option<crate::tls::ListenerConfig>,
    #[serde(default)]
    pub upstream_tls: Option<crate::tls::ClientSettings>,
    #[serde(default)]
    pub upstream_pool: crate::tls::PoolSettings,
    #[serde(default)]
    pub filter_file: Option<PathBuf>,
    #[serde(default)]
    pub admin_listen: Option<SocketAddr>,
    #[serde(default)]
    pub scheduler: Option<crate::scheduler::Settings>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Zero disables aggregate stderr output; collection remains available in-process.
    pub interval_secs: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoalescingConfig {
    pub enabled: bool,
    pub max_groups: usize,
    pub max_waiters: usize,
}

impl Default for CoalescingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_groups: 128,
            max_waiters: 64,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_variants: usize,
    pub max_ttl_secs: u32,
    pub negative_ttl_cap_secs: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 4096,
            max_bytes: 8 * 1024 * 1024,
            max_variants: 64,
            max_ttl_secs: 3600,
            negative_ttl_cap_secs: 300,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EcsConfig {
    pub enabled: bool,
    pub ipv4_prefix: u8,
    pub ipv6_prefix: u8,
}

impl Default for EcsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ipv4_prefix: 24,
            ipv6_prefix: 56,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut config = Self::parse(&text)?;
        let base = path.parent().unwrap_or(Path::new("."));
        if let Some(file) = &mut config.filter_file
            && file.is_relative()
        {
            *file = base.join(&*file);
        }
        for listener in [
            &mut config.dot,
            &mut config.doh,
            &mut config.doq,
            &mut config.doh3,
        ]
        .into_iter()
        .flatten()
        {
            for file in [&mut listener.files.cert_file, &mut listener.files.key_file] {
                if file.is_relative() {
                    *file = base.join(&*file);
                }
            }
        }
        if let Some(file) = config
            .upstream_tls
            .as_mut()
            .and_then(|settings| settings.ca_file.as_mut())
            && file.is_relative()
        {
            *file = base.join(&*file);
        }
        Ok(config)
    }

    /// Load all cryptographic material without opening a listener.
    pub fn check_files(&self) -> Result<()> {
        self.load_policy()?;
        for listener in [&self.dot, &self.doh, &self.doq, &self.doh3]
            .into_iter()
            .flatten()
        {
            crate::tls::server_config(&listener.files, &[])?;
        }
        if let Some(settings) = &self.upstream_tls {
            crate::tls::Upstream::new(settings)?;
        }
        Ok(())
    }

    pub fn load_policy(&self) -> Result<crate::policy::Policy> {
        match &self.filter_file {
            Some(path) => crate::policy::Policy::load(path),
            None => Ok(self.filter.clone()),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.source_limits.validate()?;
        self.upstream_pool.validate()?;
        ensure!(
            !self.upstream_pool.enabled || self.upstream_tls.is_some(),
            "upstream_pool requires upstream_tls"
        );
        if let Some(settings) = &self.scheduler {
            settings.validate(self.upstream)?;
        }
        if let Some(address) = self.admin_listen {
            ensure!(address.ip().is_loopback(), "admin_listen must be loopback");
        }
        ensure!(
            self.metrics.interval_secs <= 3600,
            "metrics.interval_secs must be in 0..=3600"
        );
        ensure!(
            (1..=65536).contains(&self.coalescing.max_groups)
                && (1..=65536).contains(&self.coalescing.max_waiters),
            "invalid coalescing limits"
        );
        ensure!(
            (1..=262_144).contains(&self.cache.max_entries),
            "cache.max_entries must be in 1..=262144"
        );
        ensure!(
            (512..=1_073_741_824).contains(&self.cache.max_bytes),
            "cache.max_bytes must be in 512..=1073741824"
        );
        ensure!(
            (1..=256).contains(&self.cache.max_variants)
                && self.cache.max_variants <= self.cache.max_entries,
            "invalid cache.max_variants"
        );
        ensure!(
            (1..=86400).contains(&self.cache.max_ttl_secs)
                && (1..=86400).contains(&self.cache.negative_ttl_cap_secs),
            "cache TTL caps must be in 1..=86400"
        );
        ensure!(
            self.ecs.ipv4_prefix <= 32 && self.ecs.ipv6_prefix <= 128,
            "invalid ECS prefix limit"
        );
        ensure!(self.upstream.port() != 0, "upstream port must be nonzero");
        let ip = self.upstream.ip().to_canonical();
        ensure!(
            !ip.is_unspecified()
                && !ip.is_multicast()
                && !matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast()),
            "upstream must be a unicast IP address"
        );
        let listener = if self.upstream_tls.is_some() {
            self.dot.as_ref().map(|listener| listener.listen)
        } else {
            Some(self.listen)
        };
        if let Some(listener) = listener {
            for address in
                std::iter::once(self.upstream).chain(self.scheduler.as_ref().map(|s| s.secondary))
            {
                let local = listener.ip().to_canonical();
                let upstream = address.ip().to_canonical();
                ensure!(
                    listener.port() != address.port()
                        || !(local == upstream || local.is_unspecified() && upstream.is_loopback()),
                    "upstream replica must not point to a matching DNS listener"
                );
            }
        }
        for (name, value) in [
            ("query_timeout_ms", self.query_timeout_ms),
            ("tcp_io_timeout_ms", self.tcp_io_timeout_ms),
            ("shutdown_grace_ms", self.shutdown_grace_ms),
        ] {
            ensure!((1..=60_000).contains(&value), "{name} must be in 1..=60000");
        }
        for (name, value) in [
            ("max_inflight", self.max_inflight),
            ("max_tcp_connections", self.max_tcp_connections),
        ] {
            ensure!((1..=65_536).contains(&value), "{name} must be in 1..=65536");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../parins.example.toml");

    #[test]
    fn example_is_valid() {
        Config::parse(EXAMPLE).unwrap();
    }

    #[test]
    fn source_limits_are_opt_in_and_reject_unknown_or_invalid_settings() {
        assert!(!Config::parse(EXAMPLE).unwrap().source_limits.enabled);
        assert!(
            Config::parse(&format!("{EXAMPLE}\n[source_limits]\nenabled = true\n"))
                .unwrap()
                .source_limits
                .enabled
        );
        for value in [
            "rate_per_sec = 0",
            "burst = 0",
            "max_sources = 65537",
            "ipv4_prefix = 33",
            "ipv6_prefix = 129",
            "max_inflight = 0",
            "max_connections = 0",
            "unknown = 1",
        ] {
            assert!(
                Config::parse(&format!("{EXAMPLE}\n[source_limits]\n{value}\n")).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn upstream_pool_is_opt_in_requires_tls_and_has_finite_limits() {
        let mut config = Config::parse(EXAMPLE).unwrap();
        assert!(!config.upstream_pool.enabled);
        config.upstream_pool.enabled = true;
        assert!(config.validate().is_err());
        config.upstream_tls = Some(crate::tls::ClientSettings {
            server_name: "localhost".into(),
            ca_file: None,
        });
        assert!(config.validate().is_ok());
        for value in [0, 257] {
            config.upstream_pool.max_connections = value;
            assert!(config.validate().is_err());
        }
        config.upstream_pool.max_connections = 8;
        for value in [0, 600001] {
            config.upstream_pool.idle_timeout_ms = value;
            assert!(config.validate().is_err());
        }
        assert!(Config::parse(&format!("{EXAMPLE}\n[upstream_pool]\nunknown = true\n")).is_err());
    }

    #[test]
    fn every_replica_is_checked_for_plain_and_tls_listener_loops() {
        let mut cfg = Config::parse(EXAMPLE).unwrap();
        cfg.scheduler = Some(crate::scheduler::Settings {
            secondary: cfg.listen,
            hedge_after_ms: 10,
            max_extra_inflight: 1,
        });
        assert!(cfg.validate().is_err());
        cfg.scheduler = None;
        cfg.listen = "[::]:5354".parse().unwrap();
        assert!(cfg.validate().is_err());
        cfg.upstream_tls = Some(crate::tls::ClientSettings {
            server_name: "localhost".into(),
            ca_file: None,
        });
        cfg.dot = Some(crate::tls::ListenerConfig {
            listen: "[::ffff:127.0.0.1]:5354".parse().unwrap(),
            files: crate::tls::TlsFiles {
                cert_file: "unused.pem".into(),
                key_file: "unused.key".into(),
            },
        });
        assert!(cfg.validate().is_err());
        cfg.dot.as_mut().unwrap().listen.set_port(8530);
        assert!(cfg.validate().is_ok());
        cfg.scheduler = Some(crate::scheduler::Settings {
            secondary: "127.0.0.1:8530".parse().unwrap(),
            hedge_after_ms: 10,
            max_extra_inflight: 1,
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn wildcard_listener_allows_remote_upstream_on_the_same_port() {
        let text = EXAMPLE
            .replace("127.0.0.1:5353", "0.0.0.0:53")
            .replace("127.0.0.1:5354", "192.0.2.53:53");
        Config::parse(&text).unwrap();
        assert!(Config::parse(&text.replace("192.0.2.53:53", "127.0.0.1:53")).is_err());
    }

    #[test]
    fn invalid_config_fails_early() {
        for (from, to) in [
            ("query_timeout_ms = 2000", "query_timeout_ms = 0"),
            ("max_inflight = 256", "max_inflight = 0"),
            ("127.0.0.1:5354", "127.0.0.1:5353"),
            ("127.0.0.1:5354", "0.0.0.0:5354"),
            ("127.0.0.1:5354", "resolver.example:53"),
            ("max_inflight", "max_inflght"),
            ("block_exact = []", "block_exact = ['*.test']"),
            ("block_suffix = []", "block_sufix = []"),
            ("max_groups = 128", "max_groups = 0"),
            ("max_waiters = 64", "max_waiters = 65537"),
            ("interval_secs = 0", "interval_secs = 3601"),
            ("ipv4_prefix = 24", "ipv4_prefix = 33"),
            ("ipv6_prefix = 56", "ipv6_prefix = 129"),
            ("max_entries = 4096", "max_entries = 0"),
            ("max_entries = 4096", "max_entries = 63"),
            ("max_bytes = 8388608", "max_bytes = 511"),
            ("max_variants = 64", "max_variants = 257"),
            ("max_ttl_secs = 3600", "max_ttl_secs = 0"),
            (
                "negative_ttl_cap_secs = 300",
                "negative_ttl_cap_secs = 86401",
            ),
        ] {
            assert!(Config::parse(&EXAMPLE.replace(from, to)).is_err(), "{to}");
        }
    }
}
