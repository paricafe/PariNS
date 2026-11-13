//! Subscription identities, managed source storage, and process-owned policy work.
pub(crate) mod download;
pub mod handle;
pub mod service;
pub mod settings;
pub(crate) mod store;
mod writer;

use crate::policy::canonical::{Format, PARSER_VERSION};
use anyhow::Result;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(crate) struct Source {
    url: String,
    format: Format,
    fingerprint: String,
}
impl Source {
    pub(crate) fn new(url: &str, format: Format) -> Result<Self> {
        let url = download::canonical_url(url)?;
        let mut hash = Sha256::new();
        hash.update(b"parins-subscription-source\0");
        hash.update(PARSER_VERSION.to_be_bytes());
        hash.update(match format {
            Format::DomainList => b"domain_list\0".as_slice(),
            Format::HostsBlocklist => b"hosts_blocklist\0".as_slice(),
        });
        hash.update(url.as_bytes());
        Ok(Self {
            url,
            format,
            fingerprint: format!("{:x}", hash.finalize()),
        })
    }
}

#[cfg(test)]
mod tests;
