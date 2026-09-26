//! Subscription preparation foundations. This module never activates DNS rules.
pub(crate) mod download;
pub(crate) mod store;
mod writer;

use crate::policy::canonical::{Builder, Format, Limits, MemoryStats, PARSER_VERSION, SourceStats};
use anyhow::{Context, Result, ensure};
use download::{DownloadError, DownloadOutcome, SubscriptionReader};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Seek, SeekFrom},
    path::Path,
};
use store::{CommitOutcome, PreparedMetadata, Store};
use writer::StagingWriter;

const PARSE_BUFFER: usize = 16 * 1024;

/// Identity excludes names, schedules and enablement. There is no public Config
/// field yet: this is input to the isolated preparation foundation only.
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

#[derive(Debug)]
pub(crate) struct Prepared {
    pub fingerprint: String,
    pub sha256: String,
    pub stats: SourceStats,
    pub semantic_digest: [u8; 32],
    pub memory: MemoryStats,
    /// None means an existing verified preparation was reused without network.
    pub content_commit: Option<CommitOutcome>,
}

/// One private-directory owner serializes complete preparations. No scheduler,
/// active refresh, aggregate index, PolicyHandle or runtime publication exists
/// here. Future runtime integration must own blocking parsing/store operations.
pub(crate) struct Foundation {
    store: Store,
    reader: SubscriptionReader,
    limits: Limits,
}
impl Foundation {
    pub(crate) fn open(path: &Path, max_disk_bytes: u64, limits: Limits, now: u64) -> Result<Self> {
        Builder::new(parse_limits(limits)?)
            .map_err(|error| anyhow::anyhow!("subscription parser: {error:?}"))?;
        Ok(Self {
            store: Store::open(path, max_disk_bytes, now)?,
            reader: SubscriptionReader::new(),
            limits,
        })
    }
    pub(crate) async fn prepare(&mut self, source: &Source, now: u64) -> Result<Prepared> {
        let reader = &self.reader;
        prepare_with(
            &mut self.store,
            source,
            self.limits,
            now,
            |mut file| async move {
                let outcome = reader.download(&source.url, None, None, &mut file).await;
                (file, outcome)
            },
        )
        .await
    }
}

fn parse_limits(mut limits: Limits) -> Result<Limits> {
    limits.retained_bytes = limits
        .retained_bytes
        .checked_add(PARSE_BUFFER)
        .context("subscription memory limit")?;
    ensure!(
        limits.retained_bytes <= limits.max_memory_bytes,
        "subscription memory limit"
    );
    Ok(limits)
}

fn inspect(
    mut file: File,
    source: &Source,
    limits: Limits,
) -> Result<(SourceStats, [u8; 32], MemoryStats)> {
    file.seek(SeekFrom::Start(0))?;
    let mut builder = Builder::new(parse_limits(limits)?)
        .map_err(|error| anyhow::anyhow!("subscription parser: {error:?}"))?;
    let stats = builder
        .parse_source(
            BufReader::with_capacity(PARSE_BUFFER, file),
            source.format,
            0,
        )
        .map_err(|error| anyhow::anyhow!("subscription parser: {error:?}"))?;
    let canonical = builder
        .prepare()
        .map_err(|error| anyhow::anyhow!("subscription parser: {error:?}"))?;
    Ok((stats, canonical.semantic_digest, canonical.memory()))
}

// The generic seam is private and shares all persistence/parser code. Production
// calls only SubscriptionReader; tests supply its test-only wire fixture.
async fn prepare_with<F, Fut>(
    store: &mut Store,
    source: &Source,
    limits: Limits,
    now: u64,
    fetch: F,
) -> Result<Prepared>
where
    F: FnOnce(StagingWriter) -> Fut,
    Fut: Future<Output = (StagingWriter, Result<DownloadOutcome, DownloadError>)>,
{
    store.ensure_preparable(&source.fingerprint)?;
    if let Some((record, file)) = store.read_prepared(&source.fingerprint, now)? {
        let (stats, semantic_digest, memory) = inspect(file, source, limits)?;
        ensure!(
            stats.input_rules as u64 == record.rules && stats.decoded_bytes as u64 == record.bytes,
            "stored source statistics mismatch"
        );
        return Ok(Prepared {
            fingerprint: source.fingerprint.clone(),
            sha256: record.sha256,
            stats,
            semantic_digest,
            memory,
            content_commit: None,
        });
    }
    let stage = store.begin_staging(download::MAX_BYTES, now)?;
    let writer = match StagingWriter::new(&stage) {
        Ok(file) => file,
        Err(error) => {
            store.discard(stage)?;
            return Err(error.into());
        }
    };
    let (writer, outcome) = fetch(writer).await;
    // Actual writes own the directory lease even if this future is cancelled.
    // Explicit completion drains IO before releasing the staging reservation.
    let file = match writer.finish().await {
        Ok(file) => file,
        Err(error) => {
            store.discard(stage)?;
            return Err(error.into());
        }
    };
    let checked = (|| -> Result<_> {
        let DownloadOutcome::Downloaded(downloaded) = outcome? else {
            anyhow::bail!("new preparation has no verified object for 304");
        };
        let (stats, semantic_digest, memory) = inspect(file, source, limits)?;
        ensure!(
            stats.decoded_bytes as u64 == downloaded.bytes,
            "download/parser size mismatch"
        );
        Ok((downloaded, stats, semantic_digest, memory))
    })();
    let (downloaded, stats, semantic_digest, memory) = match checked {
        Ok(value) => value,
        Err(error) => {
            store.discard(stage)?;
            return Err(error);
        }
    };
    let sha256 = downloaded.sha256;
    let outcome = store.prepare(
        stage,
        PreparedMetadata {
            fingerprint: source.fingerprint.clone(),
            sha256: sha256.clone(),
            bytes: downloaded.bytes,
            rules: stats.input_rules as u64,
            validators: downloaded.validators,
        },
        now,
    )?;
    Ok(Prepared {
        fingerprint: source.fingerprint.clone(),
        sha256,
        stats,
        semantic_digest,
        memory,
        content_commit: Some(outcome),
    })
}

#[cfg(test)]
mod tests;
