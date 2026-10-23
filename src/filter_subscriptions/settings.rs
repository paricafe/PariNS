//! Authoritative subscription settings. Validation is pure and never resolves a host.
use super::Source;
use crate::policy::{BlockMode, BlockResponse};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, Ipv6Addr},
};

pub use crate::policy::canonical::Format;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub max_rules: usize,
    pub max_memory_bytes: usize,
    pub max_disk_bytes: u64,
    pub sources: Vec<SourceSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rules: 1_000_000,
            max_memory_bytes: 128 * 1024 * 1024,
            max_disk_bytes: 256 * 1024 * 1024,
            sources: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    pub id: String,
    pub name: String,
    pub url: String,
    pub format: Format,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub auto_update: bool,
    #[serde(default = "daily")]
    pub update_interval_hours: u16,
    /// Absent follows the global `[filter]` method. Changing it recompiles the
    /// policy from stored content; it is not part of the source identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_response: Option<BlockMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_ipv4: Option<Ipv4Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_ipv6: Option<Ipv6Addr>,
}
fn yes() -> bool {
    true
}
fn daily() -> u16 {
    24
}

impl SourceSettings {
    pub(crate) fn identity(&self) -> Result<Source> {
        Source::new(&self.url, self.format)
    }
    /// This source's block method; None follows the global one.
    pub(crate) fn response(&self) -> Result<Option<BlockResponse>> {
        match self.block_response {
            None => {
                ensure!(
                    self.block_ipv4.is_none() && self.block_ipv6.is_none(),
                    "block_ipv4 and block_ipv6 are allowed only with block_response = \"custom_ip\""
                );
                Ok(None)
            }
            Some(mode) => {
                BlockResponse::from_settings(mode, self.block_ipv4, self.block_ipv6).map(Some)
            }
        }
    }
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        (1..=32).contains(&id.len())
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "subscription id must be 1..32 lowercase ASCII letters, digits or hyphens"
    );
    Ok(())
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sources.len() <= 16,
            "at most 16 subscription sources are allowed"
        );
        ensure!(
            (1..=5_000_000).contains(&self.max_rules),
            "subscription max_rules must be 1..=5000000"
        );
        ensure!(
            (1..=512 * 1024 * 1024).contains(&self.max_memory_bytes),
            "invalid subscription memory budget"
        );
        ensure!(
            (256 * 1024..=2 * 1024 * 1024 * 1024).contains(&self.max_disk_bytes),
            "invalid subscription disk budget"
        );
        let mut ids = BTreeSet::new();
        let mut fingerprints = BTreeSet::new();
        for source in &self.sources {
            validate_id(&source.id)?;
            ensure!(ids.insert(&source.id), "duplicate subscription id");
            ensure!(
                source.name.chars().count() <= 80,
                "subscription name exceeds 80 characters"
            );
            ensure!(
                (1..=168).contains(&source.update_interval_hours),
                "subscription update_interval_hours must be 1..=168"
            );
            source
                .response()
                .with_context(|| format!("subscription {}", source.id))?;
            let identity = source.identity()?;
            ensure!(
                fingerprints.insert(identity.fingerprint),
                "duplicate subscription URL and format"
            );
        }
        Ok(())
    }

    pub(crate) fn effective(&self) -> impl Iterator<Item = &SourceSettings> {
        self.sources
            .iter()
            .filter(|source| self.enabled && source.enabled)
    }

    pub(crate) fn fingerprints(&self) -> Result<Vec<String>> {
        self.sources
            .iter()
            .map(|source| Ok(source.identity()?.fingerprint))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_do_not_select_or_fetch_any_source() {
        let settings: Settings = toml::from_str("").unwrap();
        settings.validate().unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.effective().count(), 0);
    }
    #[test]
    fn ids_urls_formats_and_budgets_are_validated_even_when_disabled() {
        let source = SourceSettings {
            id: "local".into(),
            name: "Example".into(),
            url: "https://example.com/list".into(),
            format: Format::DomainList,
            enabled: true,
            auto_update: true,
            update_interval_hours: 24,
            block_response: None,
            block_ipv4: None,
            block_ipv6: None,
        };
        let mut settings = Settings {
            sources: vec![source.clone()],
            ..Default::default()
        };
        settings.validate().unwrap();
        settings.sources.push(source);
        assert!(settings.validate().is_err());
        settings.sources.pop();
        settings.sources[0].id = "Bad ID".into();
        assert!(settings.validate().is_err());
        settings.sources[0].id = "valid".into();
        settings.enabled = false;
        settings.sources[0].url = "https://127.0.0.1/list".into();
        assert!(settings.validate().is_err());
        settings.sources[0].url = "https://example.com/list".into();
        settings.max_rules = 5_000_001;
        assert!(settings.validate().is_err());
    }
    #[test]
    fn source_block_methods_follow_global_or_validate_their_addresses() {
        let parse = |extra: &str| {
            let settings: Settings = toml::from_str(&format!(
                "[[sources]]\nid = 'one'\nname = 'One'\nurl = 'https://example.com/list'\nformat = 'domain_list'\n{extra}"
            ))
            .unwrap();
            settings
                .validate()
                .map(|()| settings.sources[0].response().unwrap())
        };
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(
            parse("block_response = 'nxdomain'").unwrap(),
            Some(BlockResponse::Nxdomain)
        );
        assert_eq!(
            parse("block_response = 'custom_ip'\nblock_ipv4 = '192.0.2.1'").unwrap(),
            Some(BlockResponse::CustomIp {
                ipv4: Some("192.0.2.1".parse().unwrap()),
                ipv6: None
            })
        );
        for extra in [
            "block_response = 'custom_ip'",
            "block_ipv4 = '192.0.2.1'",
            "block_response = 'refused'\nblock_ipv6 = '2001:db8::1'",
            "block_response = 'custom_ip'\nblock_ipv4 = '255.255.255.255'",
        ] {
            let error = format!("{:#}", parse(extra).unwrap_err());
            assert!(error.starts_with("subscription one: "), "{extra}: {error}");
        }
    }
}
