//! Subscription identities, managed source storage, and process-owned policy work.
//!
//! Sources and catalog:
//! - Validators bind the final exact URL, request representation and accepted
//!   content.
//! - Source objects precede catalog selection; a durable empty catalog is
//!   initialized before the first object. Missing or corrupt authority is never
//!   reconstructed by scanning objects.
//! - Post-rename directory-sync uncertainty retains both versions and pauses GC
//!   until reconciliation. Current/previous GC roots are synchronized before a new
//!   collection.
//! - Preparation cannot refresh an active source or bypass later aggregate
//!   compilation, revision, local-basis and update-freeze gates.
//!
//! Generations and work:
//! - One worker owns aggregate policy construction and at most one request-held
//!   retired generation.
//! - Status GETs do no IO. Background download/compile runs outside Manager and
//!   mutation locks, then rechecks the captured configuration, content, local basis
//!   and update freeze before publication.
//! - Configuration candidates use try-admission under Manager and never await the
//!   worker there. Config is persisted before its infallible policy swap; background
//!   content selects the catalog before policy publication.
//! - A process started while a software update is frozen serves read-only material
//!   and leaves the Store (and its lock) closed; its status reports
//!   `update_in_progress`. After the update coordinator's reconcile thaws,
//!   management calls `Service::thaw`, which opens the Store once and seeds the
//!   refresh schedule; a failure is logged and stays
//!   `subscription_storage_unavailable`, and the next thaw retries.
pub(crate) mod download;
pub mod handle;
pub mod service;
pub mod settings;
pub(crate) mod store;
mod writer;

use crate::policy::canonical::{Format, PARSER_VERSION};
use anyhow::Result;
use sha2::{Digest, Sha256};

/// Identity is the canonical URL, list format and parser version; names,
/// schedules and enablement leave it unchanged.
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
