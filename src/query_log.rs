//! Opt-in bounded request records. SQLite owns retention, IDs, and clear barriers.
use std::{
    net::IpAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hickory_proto::op::{Message, ResponseCode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub max_entries: usize,
    pub max_bytes: u64,
    pub retention_secs: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: 100_000,
            max_bytes: 67_108_864,
            retention_secs: 86400,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.max_entries),
            "query_log.max_entries must be 1..=1000000"
        );
        anyhow::ensure!(
            (60..=2_592_000).contains(&self.retention_secs),
            "query_log.retention_secs must be 60..=2592000"
        );
        anyhow::ensure!(
            (1_048_576..=536_870_912).contains(&self.max_bytes),
            "query_log.max_bytes must be 1048576..=536870912"
        );
        Ok(())
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListOptions {
    pub before_id: Option<u64>,
    pub limit: Option<usize>,
    pub search: Option<String>,
    pub status: Option<String>,
    pub cache: Option<String>,
}
impl ListOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.before_id.is_none_or(|id| id <= i64::MAX as u64),
            "query log cursor is out of range"
        );
        anyhow::ensure!(
            self.cache
                .as_deref()
                .is_none_or(|s| matches!(s, "cached" | "fresh" | "stale" | "non_cached")),
            "invalid query log cache classification"
        );
        anyhow::ensure!(
            (1..=100).contains(&self.limit.unwrap_or(50)),
            "query log limit must be 1..=100"
        );
        anyhow::ensure!(
            self.search.as_ref().is_none_or(|s| s.len() <= 256),
            "query log search is too long"
        );
        anyhow::ensure!(
            self.status
                .as_deref()
                .is_none_or(|s| matches!(s, "success" | "blocked" | "error" | "dropped")),
            "invalid query log status"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub name: String,
    pub record_type: String,
    pub ttl: u32,
    pub data: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub id: u64,
    pub time_ms: u64,
    pub client: String,
    pub name: String,
    pub qtype: String,
    pub transport: String,
    pub status: String,
    pub rcode: Option<String>,
    pub duration_ms: f64,
    pub cache: String,
    pub cache_lookup: Option<crate::cache::LookupDecision>,
    pub cache_store: Option<crate::cache::StoreDecision>,
    pub cache_scope: Option<String>,
    pub upstream: Option<String>,
    pub upstream_relation: Option<UpstreamRelation>,
    pub upstream_attempts: Option<Vec<crate::upstreams::diagnostics::AttemptRecord>>,
    pub upstream_attempts_omitted: Option<u64>,
    pub failure_stage: Option<crate::upstreams::diagnostics::Stage>,
    pub failure_reason: Option<crate::upstreams::diagnostics::Reason>,
    pub incoming_ecs: Option<String>,
    pub outgoing_ecs: Option<String>,
    pub edns: bool,
    pub dnssec_ok: bool,
    pub checking_disabled: bool,
    pub recursion_desired: bool,
    pub answer: Vec<Record>,
    pub answer_truncated: bool,
}
#[derive(Serialize, Debug)]
pub struct Page {
    pub enabled: bool,
    pub total: usize,
    pub entries: Vec<Entry>,
    pub next_cursor: Option<u64>,
    pub log_epoch: u64,
    pub storage: crate::storage::Status,
}
pub struct QueryLog {
    storage: crate::storage::Handle,
}

/// Resolver-owned decisions; never inferred from global counters or configuration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamRelation {
    Leader,
    Follower,
    Bypass,
    PrefetchFollower,
}

#[derive(Default)]
pub(crate) struct Trace {
    pub cache: Option<&'static str>,
    pub upstream: Option<String>,
    pub upstream_trace: Option<crate::upstreams::diagnostics::Trace>,
    pub upstream_relation: Option<UpstreamRelation>,
    pub failure_stage: Option<crate::upstreams::diagnostics::Stage>,
    pub failure_reason: Option<crate::upstreams::diagnostics::Reason>,
    pub outgoing_ecs: Option<String>,
    pub cache_lookup: Option<crate::cache::LookupDecision>,
    pub cache_store: Option<crate::cache::StoreDecision>,
    pub cache_scope: Option<String>,
}

/// A cancelled foreground future still has one terminal history entry. The
/// original epoch prevents cancellation from repopulating a cleared log.
pub(crate) struct Pending<'a> {
    log: &'a QueryLog,
    epoch: u64,
    started: Instant,
    entry: Option<Entry>,
}

impl Pending<'_> {
    pub(crate) fn finish(mut self, response: Option<&Message>, trace: Trace) {
        let mut entry = self.entry.take().expect("pending query log entry");
        entry.finish(response, self.started.elapsed(), trace);
        self.log.record(self.epoch, entry);
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.entry.take() {
            entry.finish(
                None,
                self.started.elapsed(),
                Trace {
                    cache: Some("cancelled"),
                    ..Default::default()
                },
            );
            self.log.record(self.epoch, entry);
        }
    }
}

impl QueryLog {
    pub(crate) fn with_storage(storage: crate::storage::Handle) -> Self {
        Self { storage }
    }
    pub fn begin(&self) -> Option<u64> {
        self.storage.begin_log()
    }
    pub(crate) fn pending(
        &self,
        bytes: &[u8],
        peer: IpAddr,
        transport: &str,
    ) -> Option<Pending<'_>> {
        let epoch = self.begin()?;
        Some(Pending {
            log: self,
            epoch,
            started: Instant::now(),
            entry: Some(Entry::request(
                crate::protocol::decode(bytes).ok().as_ref(),
                peer,
                transport,
            )),
        })
    }
    pub async fn clear(&self, epoch: u64) -> crate::storage::Result<crate::storage::ClearResult> {
        self.storage.clear_logs(epoch).await
    }
    pub async fn list(&self, options: ListOptions) -> crate::storage::Result<Page> {
        self.storage.list_logs(options).await
    }
    pub async fn flush(&self) -> crate::storage::Result<()> {
        self.storage.flush().await
    }
    pub fn epoch(&self) -> u64 {
        self.storage.status().log_epoch
    }
    pub(crate) fn record(&self, epoch: u64, entry: Entry) {
        self.storage.record_log(epoch, entry);
    }
}
pub(crate) fn subnet(message: &Message) -> Option<String> {
    crate::ecs::subnet(message).map(|s| {
        format!(
            "{}/{} (scope /{})",
            s.addr(),
            s.source_prefix(),
            s.scope_prefix()
        )
    })
}
fn bounded(value: impl std::fmt::Display, limit: usize) -> String {
    value.to_string().chars().take(limit).collect()
}
impl Entry {
    pub(crate) fn request(query: Option<&Message>, peer: IpAddr, transport: &str) -> Self {
        Self {
            id: 0,
            time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            client: peer.to_string(),
            name: query
                .and_then(|q| q.queries.first())
                .map(|q| bounded(q.name(), 256))
                .unwrap_or_default(),
            qtype: query
                .and_then(|q| q.queries.first())
                .map(|q| q.query_type().to_string())
                .unwrap_or_default(),
            transport: bounded(transport, 16),
            status: "dropped".into(),
            rcode: None,
            duration_ms: 0.0,
            cache: "error".into(),
            cache_lookup: None,
            cache_store: None,
            cache_scope: None,
            upstream: None,
            upstream_relation: None,
            upstream_attempts: None,
            upstream_attempts_omitted: None,
            failure_stage: None,
            failure_reason: None,
            incoming_ecs: query.and_then(subnet),
            outgoing_ecs: None,
            edns: query.is_some_and(|q| q.edns.is_some()),
            dnssec_ok: query
                .and_then(|q| q.edns.as_ref())
                .is_some_and(|e| e.flags().dnssec_ok),
            checking_disabled: query.is_some_and(|q| q.checking_disabled),
            recursion_desired: query.is_some_and(|q| q.recursion_desired),
            answer: Vec::new(),
            answer_truncated: false,
        }
    }
    pub(crate) fn finish(&mut self, response: Option<&Message>, elapsed: Duration, trace: Trace) {
        self.duration_ms = elapsed.as_secs_f64() * 1000.0;
        self.cache = trace.cache.unwrap_or("error").into();
        self.cache_lookup = trace.cache_lookup;
        self.cache_store = trace.cache_store;
        self.cache_scope = trace.cache_scope;
        self.upstream = trace.upstream.map(|s| bounded(s, 512));
        self.upstream_relation = trace.upstream_relation;
        self.failure_stage = trace.failure_stage;
        self.failure_reason = trace.failure_reason;
        self.upstream_attempts_omitted = trace.upstream_trace.as_ref().map(|trace| trace.omitted);
        self.upstream_attempts = trace.upstream_trace.map(|trace| trace.attempts);
        self.outgoing_ecs = trace.outgoing_ecs;
        let Some(response) = response else {
            return;
        };
        self.rcode = Some(format!("{:?}", response.response_code).to_ascii_uppercase());
        self.status = if self.cache == "blocked" {
            "blocked"
        } else if matches!(
            response.response_code,
            ResponseCode::NoError | ResponseCode::NXDomain
        ) {
            "success"
        } else {
            "error"
        }
        .into();
        self.answer_truncated = response.answers.len() > 16;
        self.answer = response
            .answers
            .iter()
            .take(16)
            .map(|r| {
                let data = r.data.to_string();
                self.answer_truncated |= data.chars().count() > 512;
                Record {
                    name: bounded(&r.name, 256),
                    record_type: r.record_type().to_string(),
                    ttl: r.ttl,
                    data: bounded(data, 512),
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounds_retention_and_clear_generation() {
        let runtime =
            crate::runtime_services::RuntimeServices::ephemeral(crate::storage::RuntimeSettings {
                query_log: Settings {
                    enabled: true,
                    max_entries: 2,
                    ..Default::default()
                },
                ..Default::default()
            });
        let log = &runtime.query_log;
        let epoch = log.begin().unwrap();
        for _ in 0..3 {
            log.record(
                epoch,
                Entry::request(None, "127.0.0.1".parse().unwrap(), "udp"),
            );
        }
        log.flush().await.unwrap();
        let page = log
            .list(ListOptions {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.entries[0].id, 3);
        assert_eq!(page.next_cursor, Some(3));
        assert_eq!(
            log.list(ListOptions {
                before_id: page.next_cursor,
                ..Default::default()
            })
            .await
            .unwrap()
            .entries[0]
                .id,
            2
        );
        log.clear(epoch).await.unwrap();
        log.record(
            epoch,
            Entry::request(None, "127.0.0.1".parse().unwrap(), "udp"),
        );
        log.flush().await.unwrap();
        assert_eq!(log.list(ListOptions::default()).await.unwrap().total, 0);
        let new_epoch = log.begin().unwrap();
        let mut expired = Entry::request(None, "127.0.0.1".parse().unwrap(), "udp");
        expired.time_ms = 1;
        expired.duration_ms = 86_401_000.0;
        log.record(new_epoch, expired);
        log.flush().await.unwrap();
        assert_eq!(log.list(ListOptions::default()).await.unwrap().total, 0);
        assert!(
            crate::runtime_services::RuntimeServices::ephemeral(Default::default())
                .query_log
                .begin()
                .is_none()
        );
        assert!(
            ListOptions {
                limit: Some(101),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ListOptions {
                status: Some("bogus".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
