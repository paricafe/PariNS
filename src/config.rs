//! Startup configuration. Invalid values fail before any listeners are bound.

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub upstreams: crate::upstreams::Settings,
    #[serde(default)]
    pub query_log: crate::query_log::Settings,
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
    // Compiled policy is projected from source rules by management, not its trie.
    #[serde(default, skip_serializing)]
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
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub filter_file: Option<PathBuf>,
    #[serde(default)]
    pub admin_listen: Option<SocketAddr>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    pub public_host: String,
}

/// A concrete browser host, never an authority or URL. The returned value is
/// suitable for a certificate name check and for constructing an origin.
pub fn public_host(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value == value.trim(),
        "web.public_host must be one domain or IP"
    );
    if let Ok(ip) = value.parse::<IpAddr>() {
        ensure!(
            !ip.is_unspecified() && !ip.is_multicast(),
            "web.public_host must be a concrete IP"
        );
        return Ok(ip.to_string());
    }
    ensure!(
        !value.contains(['/', ':', '@', '[', ']', '*', '?', '#', '\\']) && !value.ends_with('.'),
        "web.public_host must be one domain or IP without a port or scheme"
    );
    let name = idna::domain_to_ascii_strict(value)
        .map_err(|_| anyhow::anyhow!("web.public_host is not a valid domain"))?
        .to_ascii_lowercase();
    ensure!(
        name.len() <= 253
            && name.split('.').all(|part| !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
        "web.public_host is not a valid domain"
    );
    Ok(name)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Zero disables aggregate stderr output; collection remains available in-process.
    pub interval_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_variants: usize,
    pub max_ttl_secs: u32,
    pub negative_ttl_cap_secs: u32,
    pub shards: usize,
    pub negative_percent: u8,
    pub prefetch: PrefetchConfig,
    pub stale: StaleConfig,
    pub rules: Vec<CacheRule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrefetchConfig {
    pub enabled: bool,
    pub min_hits: u64,
    pub remaining_percent: u8,
    pub max_inflight: usize,
    pub rate_per_sec: u32,
    pub backoff_secs: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_hits: 3,
            remaining_percent: 10,
            max_inflight: 2,
            rate_per_sec: 10,
            backoff_secs: 5,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaleConfig {
    pub enabled: bool,
    pub retention_secs: u64,
    pub reply_ttl_secs: u32,
}

impl Default for StaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention_secs: 300,
            reply_ttl_secs: 30,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheRule {
    pub name: String,
    pub suffix: bool,
    pub qtype: Option<String>,
    pub bypass: bool,
    pub max_ttl_secs: Option<u32>,
    pub negative_ttl_cap_secs: Option<u32>,
    pub prefetch: Option<bool>,
    pub stale: Option<bool>,
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
            shards: 4,
            negative_percent: 20,
            prefetch: PrefetchConfig::default(),
            stale: StaleConfig::default(),
            rules: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
        let base = path.parent().unwrap_or(Path::new("."));
        Self::parse_in(&text, base)
    }

    /// Parse a managed configuration with paths relative to its private state directory.
    pub fn parse_in(text: &str, base: &Path) -> Result<Self> {
        let mut config = Self::parse(text)?;
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
        if let Some(file) = config.upstreams.ca_file.as_mut()
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
        crate::upstreams::Pool::new(&self.upstreams, self)?;
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
        if let Some(web) = &self.web {
            public_host(&web.public_host)?;
        }
        self.query_log.validate()?;
        self.upstreams.validate()?;
        self.upstreams.validate_listeners(self)?;
        self.source_limits.validate()?;
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
            (1..=64).contains(&self.cache.shards),
            "cache.shards must be in 1..=64"
        );
        ensure!(
            self.cache.negative_percent <= 90,
            "cache.negative_percent must be in 0..=90"
        );
        let prefetch = &self.cache.prefetch;
        ensure!(
            (1..=1_000_000).contains(&prefetch.min_hits)
                && (1..=90).contains(&prefetch.remaining_percent)
                && (1..=256).contains(&prefetch.max_inflight)
                && (1..=10_000).contains(&prefetch.rate_per_sec)
                && (1..=3600).contains(&prefetch.backoff_secs),
            "invalid cache.prefetch limits"
        );
        ensure!(
            (1..=604800).contains(&self.cache.stale.retention_secs)
                && (1..=300).contains(&self.cache.stale.reply_ttl_secs),
            "invalid cache.stale limits"
        );
        ensure!(
            self.cache.rules.len() <= 256,
            "cache.rules exceeds 256 rules"
        );
        for rule in &self.cache.rules {
            ensure!(
                !rule.name.is_empty()
                    && rule.name.len() <= 253
                    && hickory_proto::rr::Name::from_ascii(&rule.name).is_ok(),
                "invalid cache rule name"
            );
            if let Some(kind) = &rule.qtype {
                ensure!(
                    kind.parse::<hickory_proto::rr::RecordType>().is_ok(),
                    "invalid cache rule qtype"
                );
            }
            ensure!(
                [rule.max_ttl_secs, rule.negative_ttl_cap_secs]
                    .into_iter()
                    .flatten()
                    .all(|ttl| (1..=86400).contains(&ttl)),
                "invalid cache rule TTL cap"
            );
        }
        ensure!(
            self.ecs.ipv4_prefix <= 32 && self.ecs.ipv6_prefix <= 128,
            "invalid ECS prefix limit"
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
    fn web_public_host_is_one_concrete_normalized_name_or_ip() {
        assert_eq!(
            public_host("BÜCHER.Example").unwrap(),
            "xn--bcher-kva.example"
        );
        assert_eq!(public_host("2001:db8::1").unwrap(), "2001:db8::1");
        for invalid in [
            "",
            "*.example.com",
            "https://dns.example",
            "dns.example:3000",
            "[::1]",
            "dns.example/path",
            "dns.example.",
            "0.0.0.0",
            "a..b",
        ] {
            assert!(public_host(invalid).is_err(), "{invalid}");
        }
        assert!(Config::parse(&format!("{EXAMPLE}\n[web]\npublic_host='dns.example'\n")).is_ok());
        assert!(Config::parse(&format!("{EXAMPLE}\n[web]\npublic_host='*.example'\n")).is_err());
    }

    #[test]
    fn upstreams_is_required_and_removed_fields_are_rejected() {
        let base = "listen='127.0.0.1:5353'\nquery_timeout_ms=1000\ntcp_io_timeout_ms=1000\nshutdown_grace_ms=500\nmax_inflight=128\nmax_tcp_connections=32\n";
        assert!(Config::parse(base).is_err());
        assert!(Config::parse(&format!("{base}upstream='1.1.1.1:53'\n")).is_err());
        let canonical = format!("{base}[upstreams]\nservers=['udp://1.1.1.1:53']\n");
        let config = Config::parse(&canonical).unwrap();
        assert_eq!(config.upstreams.servers, ["udp://1.1.1.1:53"]);
        assert!(crate::upstreams::Pool::new(&config.upstreams, &config).is_ok());
        assert!(Config::parse(&format!("upstream='9.9.9.9:53'\n{canonical}")).is_err());
        assert!(Config::parse(&format!("{base}[upstreams]\nservers=[]\n")).is_err());
        for section in ["upstream_tls", "upstream_pool", "scheduler"] {
            assert!(Config::parse(&format!("{canonical}\n[{section}]\n")).is_err());
        }
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
    fn dot_pool_is_opt_in_and_has_finite_limits() {
        let mut config = Config::parse(EXAMPLE).unwrap();
        assert!(!config.upstreams.dot_pool.enabled);
        config.upstreams.servers = vec!["tls://127.0.0.1:853".into()];
        config.upstreams.dot_pool.enabled = true;
        assert!(config.validate().is_ok());
        for value in [0, 257] {
            config.upstreams.dot_pool.max_connections = value;
            assert!(config.validate().is_err());
        }
        config.upstreams.dot_pool.max_connections = 8;
        for value in [0, 600001] {
            config.upstreams.dot_pool.idle_timeout_ms = value;
            assert!(config.validate().is_err());
        }
        let mut document: toml::Value = toml::from_str(EXAMPLE).unwrap();
        document["upstreams"]["dot_pool"]
            .as_table_mut()
            .unwrap()
            .insert("unknown".into(), toml::Value::Boolean(true));
        let error = Config::parse(&toml::to_string(&document).unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `unknown`"));
    }

    #[test]
    fn every_endpoint_is_checked_for_plain_and_tls_listener_loops() {
        let mut cfg = Config::parse(EXAMPLE).unwrap();
        cfg.upstreams.servers = vec![
            "udp://127.0.0.1:5354".into(),
            format!("udp://{}", cfg.listen),
        ];
        assert!(cfg.validate().is_err());
        cfg.upstreams.servers.pop();
        cfg.listen = "[::]:5354".parse().unwrap();
        assert!(cfg.validate().is_err());
        cfg.upstreams.servers = vec!["tls://127.0.0.1:5354".into()];
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
        cfg.upstreams.servers.push("tls://127.0.0.1:8530".into());
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

    #[test]
    fn cache_policy_validation_and_safe_defaults() {
        let mut cfg = Config::parse(EXAMPLE).unwrap();
        assert!(!cfg.cache.prefetch.enabled && !cfg.cache.stale.enabled);
        cfg.cache.rules.push(CacheRule {
            name: "example.test".into(),
            qtype: Some("A".into()),
            max_ttl_secs: Some(30),
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
        cfg.cache.rules[0].max_ttl_secs = Some(0);
        assert!(cfg.validate().is_err());
        cfg.cache.rules[0].max_ttl_secs = Some(30);
        cfg.cache.rules[0].qtype = Some("not-a-type".into());
        assert!(cfg.validate().is_err());
        cfg.cache.rules.clear();
        cfg.cache.shards = 0;
        assert!(cfg.validate().is_err());
        cfg.cache.shards = 4;
        cfg.cache.negative_percent = 91;
        assert!(cfg.validate().is_err());
        cfg.cache.negative_percent = 20;
        cfg.cache.prefetch.rate_per_sec = 0;
        assert!(cfg.validate().is_err());
        cfg.cache.prefetch.rate_per_sec = 10;
        cfg.cache.stale.reply_ttl_secs = 0;
        assert!(cfg.validate().is_err());
    }
}
