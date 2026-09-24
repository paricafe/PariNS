//! Bounded clean-snapshot codec. Only the lifecycle coordinator opens files.
//! Call after writers have stopped, or before the cache is published to DNS.

use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use hickory_proto::op::{MessageType, OpCode, Query};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::*;

const VERSION: u32 = 2;
const CACHE_SEMANTICS: u32 = 2;
const HEADER_BYTES: usize = 512;
const MAX_RECORD_BYTES: usize = 96 * 1024;
const MAX_RECORDS: usize = 1_000_000;

#[derive(Clone, Debug, Default, Serialize)]
pub struct SnapshotReport {
    pub saved: usize,
    pub restored: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub reason: Option<String>,
}

impl SnapshotReport {
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u32,
    fingerprint: String,
    saved_at_ns: u128,
    records: usize,
    checksum: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedKey {
    name: String,
    kind: u16,
    class: u16,
    dnssec_ok: bool,
    checking_disabled: bool,
    recursion_desired: bool,
    edns: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "namespace", content = "network", rename_all = "snake_case")]
enum SavedScope {
    NoEcs,
    PrivacyV4,
    PrivacyV6,
    Network(String),
    ExactSource(String),
}

impl From<Scope> for SavedScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::NoEcs => Self::NoEcs,
            Scope::Privacy { ipv4: true } => Self::PrivacyV4,
            Scope::Privacy { ipv4: false } => Self::PrivacyV6,
            Scope::Network(network) => Self::Network(network.to_string()),
            Scope::ExactSource(network) => Self::ExactSource(network.to_string()),
        }
    }
}

impl SavedScope {
    fn decode(&self) -> Result<Scope> {
        Ok(match self {
            Self::NoEcs => Scope::NoEcs,
            Self::PrivacyV4 => Scope::Privacy { ipv4: true },
            Self::PrivacyV6 => Scope::Privacy { ipv4: false },
            Self::Network(text) => {
                let network: ipnet::IpNet = text.parse().context("invalid cache scope")?;
                ensure!(
                    network == network.trunc() && text == &network.to_string(),
                    "noncanonical cache scope"
                );
                Scope::Network(network)
            }
            Self::ExactSource(text) => Scope::parse_tag(&format!("exact_ecs:{text}"))?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    key: SavedKey,
    scope: SavedScope,
    remaining_secs: u32,
    wire: String,
}

impl Record {
    fn decode(&self) -> Result<(Message, Message, Scope)> {
        ensure!(
            self.remaining_secs > 0 && self.remaining_secs <= i32::MAX as u32,
            "invalid remaining lifetime"
        );
        ensure!(
            !self.key.dnssec_ok || self.key.edns,
            "DNSSEC flag without EDNS"
        );
        let name = Name::from_ascii(&self.key.name).context("invalid snapshot name")?;
        ensure!(
            canonical(&self.key.name) == self.key.name && self.key.name.len() <= 1024,
            "noncanonical snapshot name"
        );
        let mut query = Message::new(0, MessageType::Query, OpCode::Query);
        let mut question = Query::query(name, RecordType::from(self.key.kind));
        question.set_query_class(DNSClass::from(self.key.class));
        query.add_query(question);
        query.metadata.checking_disabled = self.key.checking_disabled;
        query.metadata.recursion_desired = self.key.recursion_desired;
        if self.key.edns {
            let mut edns = Edns::new();
            edns.set_dnssec_ok(self.key.dnssec_ok)
                .set_max_payload(MAX_UDP_PAYLOAD);
            query.edns = Some(edns);
        }
        ensure!(Key::of(&query).is_some(), "ineligible snapshot key");
        ensure!(
            self.wire.len() <= protocol::MAX_MESSAGE.div_ceil(3) * 4,
            "snapshot wire too large"
        );
        let response = protocol::decode(&STANDARD.decode(&self.wire)?)?;
        ensure!(
            response.message_type == MessageType::Response
                && response.op_code == OpCode::Query
                && response.queries.len() == 1
                && response.queries[0].name().to_lowercase()
                    == query.queries[0].name().to_lowercase()
                && response.queries[0].query_type() == query.queries[0].query_type()
                && response.queries[0].query_class() == query.queries[0].query_class()
                && response.edns.is_none(),
            "snapshot question/key mismatch"
        );
        Ok((query, response, self.scope.decode()?))
    }
}

fn timestamp(time: SystemTime) -> Result<u128> {
    Ok(time.duration_since(SystemTime::UNIX_EPOCH)?.as_nanos())
}

fn cancelled(flag: &AtomicBool) -> Result<()> {
    ensure!(!flag.load(Ordering::Acquire), "cache snapshot cancelled");
    Ok(())
}

fn digest(mut hash: Sha256, header: &Header) -> String {
    hash.update(header.version.to_le_bytes());
    hash.update(header.saved_at_ns.to_le_bytes());
    hash.update((header.records as u64).to_le_bytes());
    hash.update(header.fingerprint.as_bytes());
    format!("{:x}", hash.finalize())
}

fn age_records(message: &mut Message, seconds: u32) {
    for rr in message
        .answers
        .iter_mut()
        .chain(&mut message.authorities)
        .chain(&mut message.additionals)
    {
        rr.ttl = rr.ttl.saturating_sub(seconds);
    }
}

impl Cache {
    /// Streams records with a fixed-size header, without changing recency/hit counters.
    /// The caller owns quiescence and must run this synchronous work off Tokio workers.
    pub fn write_clean_snapshot<W: Write + Seek>(
        &self,
        writer: &mut W,
        fingerprint: &str,
        saved_at: SystemTime,
        now: Instant,
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<SnapshotReport> {
        ensure!(
            fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid semantic fingerprint"
        );
        ensure!(max_bytes >= HEADER_BYTES, "snapshot budget too small");
        cancelled(cancel)?;
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(&[b' '; HEADER_BYTES])?;
        let mut report = SnapshotReport {
            bytes: HEADER_BYTES as u64,
            ..SnapshotReport::default()
        };
        let mut hash = Sha256::new();
        for shard in &self.shards {
            let entries = {
                let shard = shard.lock().expect("cache shard poisoned");
                let mut entries: Vec<_> = shard
                    .partitions
                    .iter()
                    .flat_map(|p| p.lru.iter().map(|(_, e)| Arc::clone(e)))
                    .collect();
                entries.sort_unstable_by_key(|e| {
                    std::cmp::Reverse(e.accessed.load(Ordering::Relaxed))
                });
                entries
            };
            for entry in entries {
                cancelled(cancel)?;
                // Round upward to avoid extending the original fresh deadline by
                // truncating a fractional second at either snapshot boundary.
                let age = now.saturating_duration_since(entry.inserted);
                let remaining = entry
                    .lifetime
                    .saturating_sub(age)
                    .as_secs()
                    .min(u64::from(u32::MAX)) as u32;
                if remaining == 0 || report.saved >= MAX_RECORDS {
                    report.skipped += 1;
                    continue;
                }
                let elapsed = age
                    .as_secs()
                    .saturating_add(u64::from(age.subsec_nanos() > 0))
                    .min(u64::from(u32::MAX)) as u32;
                let mut response = protocol::decode(&entry.wire)?;
                age_records(&mut response, elapsed);
                let record = Record {
                    key: SavedKey {
                        name: entry.key.name.to_string(),
                        kind: entry.key.kind.into(),
                        class: entry.key.class.into(),
                        dnssec_ok: entry.key.dnssec_ok,
                        checking_disabled: entry.key.checking_disabled,
                        recursion_desired: entry.key.recursion_desired,
                        edns: entry.key.edns,
                    },
                    scope: entry.scope.into(),
                    remaining_secs: remaining,
                    wire: STANDARD.encode(response.to_vec()?),
                };
                let mut line = serde_json::to_vec(&record)?;
                line.push(b'\n');
                if line.len() > MAX_RECORD_BYTES
                    || report.bytes + line.len() as u64 > max_bytes as u64
                {
                    report.skipped += 1;
                    continue;
                }
                writer.write_all(&line)?;
                hash.update(&line);
                report.saved += 1;
                report.bytes += line.len() as u64;
            }
        }
        let mut header = Header {
            version: VERSION,
            fingerprint: fingerprint.into(),
            saved_at_ns: timestamp(saved_at)?,
            records: report.saved,
            checksum: String::new(),
        };
        header.checksum = digest(hash, &header);
        let json = serde_json::to_vec(&header)?;
        ensure!(json.len() < HEADER_BYTES, "snapshot header too large");
        let mut block = [b' '; HEADER_BYTES];
        block[..json.len()].copy_from_slice(&json);
        block[HEADER_BYTES - 1] = b'\n';
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(&block)?;
        writer.flush()?;
        cancelled(cancel)?;
        Ok(report)
    }

    /// Validates the complete bounded stream before admitting any record. No
    /// disk lookup layer, request metrics, refresh work, or recency is restored.
    pub fn restore_clean_snapshot<R: Read + Seek>(
        &self,
        reader: &mut R,
        fingerprint: &str,
        now_wall: SystemTime,
        now: Instant,
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<SnapshotReport> {
        cancelled(cancel)?;
        let length = reader.seek(SeekFrom::End(0))?;
        ensure!(
            length >= HEADER_BYTES as u64 && length <= max_bytes as u64,
            "snapshot exceeds byte budget or is truncated"
        );
        reader.seek(SeekFrom::Start(0))?;
        let mut block = [0; HEADER_BYTES];
        reader.read_exact(&mut block)?;
        let header: Header = serde_json::from_slice(&block).context("invalid snapshot header")?;
        ensure!(header.version == VERSION, "unsupported snapshot version");
        ensure!(
            header.fingerprint == fingerprint,
            "snapshot policy mismatch"
        );
        ensure!(
            header.records <= MAX_RECORDS,
            "snapshot record limit exceeded"
        );
        let now_ns = timestamp(now_wall)?;
        ensure!(now_ns >= header.saved_at_ns, "snapshot clock rollback");
        let offline = (now_ns - header.saved_at_ns)
            .div_ceil(1_000_000_000)
            .min(u128::from(u32::MAX)) as u32;
        // Pass one authenticates the entire payload, including semantic metadata,
        // before any entry is inserted. Each allocation is one bounded record.
        let mut hash = Sha256::new();
        scan(reader, header.records, length, cancel, |line, record| {
            hash.update(line);
            record.decode()?;
            Ok(())
        })?;
        ensure!(
            digest(hash, &header) == header.checksum,
            "snapshot checksum mismatch"
        );
        reader.seek(SeekFrom::Start(HEADER_BYTES as u64))?;
        let mut report = SnapshotReport {
            saved: header.records,
            bytes: length,
            ..SnapshotReport::default()
        };
        scan(reader, header.records, length, cancel, |_, record| {
            let remaining = record.remaining_secs.saturating_sub(offline);
            let (query, mut response, scope) = record.decode()?;
            age_records(&mut response, offline);
            if remaining > 0
                && self
                    .admit(&query, &response, scope, now, self.epoch(), Some(remaining))
                    .admitted()
            {
                report.restored += 1;
            } else {
                report.skipped += 1;
            }
            Ok(())
        })?;
        Ok(report)
    }
}

fn scan<R: Read>(
    reader: &mut R,
    count: usize,
    length: u64,
    cancel: &AtomicBool,
    mut visit: impl FnMut(&[u8], Record) -> Result<()>,
) -> Result<()> {
    let mut reader = BufReader::new(reader.take(length - HEADER_BYTES as u64));
    let mut line = Vec::new();
    let mut consumed = HEADER_BYTES as u64;
    for _ in 0..count {
        cancelled(cancel)?;
        line.clear();
        reader
            .by_ref()
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        ensure!(
            !line.is_empty() && line.len() <= MAX_RECORD_BYTES && line.last() == Some(&b'\n'),
            "invalid or oversized snapshot record"
        );
        consumed += line.len() as u64;
        visit(
            &line,
            serde_json::from_slice(&line).context("invalid snapshot record")?,
        )?;
    }
    ensure!(consumed == length, "snapshot record count mismatch");
    Ok(())
}

/// Uses the actual loaded filter digest supplied by the current Resolver, never
/// reopens a rule file whose contents might differ from the active generation.
pub fn semantic_fingerprint(
    config: &crate::config::Config,
    filter_digest: &[u8],
) -> Result<String> {
    let mut cache = serde_json::to_value(&config.cache)?;
    cache
        .as_object_mut()
        .context("cache policy is not an object")?
        .remove("persistence");
    let semantics = serde_json::json!({"semantics": CACHE_SEMANTICS, "cache": cache, "ecs": config.ecs, "upstreams": config.upstreams,
        "filter_digest": STANDARD.encode(filter_digest)});
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&semantics)?)
    ))
}

#[cfg(test)]
mod tests;
