//! Opt-in, memory-only DNS query history. Never stores raw packets or EDNS blobs.
use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hickory_proto::op::{Message, ResponseCode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub max_entries: usize,
    pub retention_secs: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: 1000,
            retention_secs: 86400,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=10000).contains(&self.max_entries),
            "query_log.max_entries must be 1..=10000"
        );
        anyhow::ensure!(
            (1..=604800).contains(&self.retention_secs),
            "query_log.retention_secs must be 1..=604800"
        );
        Ok(())
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListOptions {
    pub before_id: Option<u64>,
    pub limit: Option<usize>,
    pub search: Option<String>,
    pub status: Option<String>,
}
impl ListOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
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
#[derive(Clone, Serialize)]
pub struct Record {
    pub name: String,
    pub record_type: String,
    pub ttl: u32,
    pub data: String,
}
#[derive(Clone, Serialize)]
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
    pub upstream: Option<String>,
    pub incoming_ecs: Option<String>,
    pub outgoing_ecs: Option<String>,
    pub edns: bool,
    pub dnssec_ok: bool,
    pub checking_disabled: bool,
    pub recursion_desired: bool,
    pub answer: Vec<Record>,
    pub answer_truncated: bool,
}
#[derive(Serialize)]
pub struct Page {
    pub enabled: bool,
    pub total: usize,
    pub entries: Vec<Entry>,
    pub next_cursor: Option<u64>,
}
struct State {
    epoch: u64,
    next_id: u64,
    entries: VecDeque<(Instant, Entry)>,
}
pub struct QueryLog {
    settings: Settings,
    state: Mutex<State>,
}

/// Resolver-owned decisions; never inferred from global counters or configuration.
#[derive(Default)]
pub(crate) struct Trace {
    pub cache: Option<&'static str>,
    pub upstream: Option<String>,
    pub outgoing_ecs: Option<String>,
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
    pub fn new(settings: Settings) -> Self {
        assert!(settings.validate().is_ok(), "validated query log settings");
        Self {
            settings,
            state: Mutex::new(State {
                epoch: 0,
                next_id: 1,
                entries: VecDeque::new(),
            }),
        }
    }
    pub fn begin(&self) -> Option<u64> {
        self.settings
            .enabled
            .then(|| self.state.lock().expect("query log poisoned").epoch)
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
    pub fn clear(&self) -> usize {
        let mut state = self.state.lock().expect("query log poisoned");
        let count = state.entries.len();
        state.epoch = state.epoch.wrapping_add(1);
        state.entries.clear();
        count
    }
    fn expire(&self, state: &mut State, now: Instant) {
        while state.entries.front().is_some_and(|(inserted, _)| {
            now.saturating_duration_since(*inserted)
                >= Duration::from_secs(self.settings.retention_secs)
        }) {
            state.entries.pop_front();
        }
    }
    /// Called by the server's lifecycle-bound timer, including while idle.
    pub fn purge_expired(&self) {
        let mut state = self.state.lock().expect("query log poisoned");
        self.expire(&mut state, Instant::now());
    }
    pub fn list(&self, options: ListOptions) -> Page {
        let mut state = self.state.lock().expect("query log poisoned");
        self.expire(&mut state, Instant::now());
        let search = options.search.unwrap_or_default().to_ascii_lowercase();
        let limit = options.limit.unwrap_or(50).clamp(1, 100);
        let mut matches = state.entries.iter().rev().map(|(_, e)| e).filter(|e| {
            options.before_id.is_none_or(|id| e.id < id)
                && options.status.as_ref().is_none_or(|s| e.status == *s)
                && (search.is_empty()
                    || e.name.to_ascii_lowercase().contains(&search)
                    || e.client.contains(&search)
                    || e.upstream
                        .as_ref()
                        .is_some_and(|value| value.to_ascii_lowercase().contains(&search))
                    || e.qtype.to_ascii_lowercase().contains(&search))
        });
        let entries: Vec<_> = matches.by_ref().take(limit).cloned().collect();
        let next_cursor = matches.next().and_then(|_| entries.last().map(|e| e.id));
        Page {
            enabled: self.settings.enabled,
            total: state.entries.len(),
            entries,
            next_cursor,
        }
    }
    pub(crate) fn record(&self, epoch: u64, mut entry: Entry) {
        if !self.settings.enabled {
            return;
        }
        let mut state = self.state.lock().expect("query log poisoned");
        if state.epoch != epoch {
            return;
        }
        let now = Instant::now();
        self.expire(&mut state, now);
        entry.id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        while state.entries.len() >= self.settings.max_entries {
            state.entries.pop_front();
        }
        state.entries.push_back((now, entry));
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
            upstream: None,
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
        self.upstream = trace.upstream.map(|s| bounded(s, 512));
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
    #[test]
    fn bounds_retention_and_clear_generation() {
        let log = QueryLog::new(Settings {
            enabled: true,
            max_entries: 2,
            retention_secs: 1,
        });
        let epoch = log.begin().unwrap();
        for _ in 0..3 {
            log.record(
                epoch,
                Entry::request(None, "127.0.0.1".parse().unwrap(), "udp"),
            );
        }
        let page = log.list(ListOptions {
            limit: Some(1),
            ..Default::default()
        });
        assert_eq!(page.total, 2);
        assert_eq!(page.entries[0].id, 3);
        assert_eq!(page.next_cursor, Some(3));
        assert_eq!(
            log.list(ListOptions {
                before_id: page.next_cursor,
                ..Default::default()
            })
            .entries[0]
                .id,
            2
        );
        {
            let mut state = log.state.lock().unwrap();
            for (time, _) in &mut state.entries {
                *time -= Duration::from_secs(2);
            }
        }
        log.purge_expired();
        assert!(log.state.lock().unwrap().entries.is_empty());
        assert_eq!(log.list(ListOptions::default()).total, 0);
        log.clear();
        log.record(
            epoch,
            Entry::request(None, "127.0.0.1".parse().unwrap(), "udp"),
        );
        assert_eq!(log.list(ListOptions::default()).total, 0);
        assert!(QueryLog::new(Settings::default()).begin().is_none());
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
