//! Startup configuration. Invalid values fail before any listeners are bound.

use std::{net::SocketAddr, path::Path};

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
    pub ecs: EcsConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub filter: crate::policy::Policy,
    #[serde(default)]
    pub coalescing: CoalescingConfig,
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
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
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
        ensure!(
            !self.upstream.ip().is_unspecified() && !self.upstream.ip().is_multicast(),
            "upstream must be a unicast IP address"
        );
        ensure!(
            self.listen != self.upstream
                && !(self.listen.ip().is_unspecified()
                    && self.upstream.ip().is_loopback()
                    && self.listen.is_ipv4() == self.upstream.is_ipv4()
                    && self.listen.port() == self.upstream.port()),
            "upstream must not point to the listener"
        );
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
