//! Authoritative subscription settings. Validation is pure and never resolves a host.
use super::Source;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
}
