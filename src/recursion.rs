//! Recursive resolution from the root servers with DNSSEC validation, an
//! alternative to the upstream pool selected by `[resolution] mode`.
//!
//! The Resolver keeps the deadline, cache, coalescing, filtering and stale
//! fallback for both sources; this module owns the hickory recursor, its global
//! outbound bounds, and the translation of a recursive result into a response:
//!
//! - ECS: the client subnet the Resolver prepared is sent only to the answer's
//!   authoritative servers. Their scope is echoed in the response, and a missing
//!   ECS option from an authority means the answer does not depend on the
//!   client network (RFC 7871), so it is echoed as scope 0, which the cache
//!   shares within the address family. Answers never become ExactSource.
//! - DNSSEC: Bogus data becomes SERVFAIL with Extended DNS Error 6. AD is set
//!   only when every answer and authority record is Secure.
//! - Infrastructure caches (delegations, name servers, DNSSEC keys) belong to
//!   the recursor and are bounded by entry counts; answers are never read from
//!   or stored in the recursor's answer cache.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Result, anyhow, ensure};
use hickory_proto::{
    dnssec::Proof,
    op::{Edns, Message, MessageType, OpCode, ResponseCode},
    rr::{
        Record, RecordType,
        rdata::opt::{ClientSubnet, EdnsCode, EdnsOption},
    },
};
use hickory_resolver::{
    net::{DnsError, NetError, NoRecords},
    recursor::{
        DnssecConfig, DnssecPolicy, EcsScope, QNameMinimization, Recursor, RecursorError,
        RecursorOptions, ResolveOptions,
    },
};
use serde::{Deserialize, Serialize};

use crate::{ecs, protocol::MAX_UDP_PAYLOAD};

mod outbound;

/// RFC 8914 Extended DNS Error option code and the "DNSSEC Bogus" info code.
const EDE: u16 = 15;
const EDE_DNSSEC_BOGUS: u16 = 6;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Also query authoritative servers over IPv6.
    pub ipv6: bool,
    /// Sustained queries per second to all authoritative servers together.
    pub max_outbound_per_second: u32,
    /// Queries to authoritative servers awaiting a response at once.
    pub max_outbound_inflight: usize,
    /// Referral and CNAME depth for one resolution.
    pub max_depth: u8,
    /// Queries to authoritative servers one resolution may send.
    pub max_queries_per_request: u16,
    /// Delegation, name server and DNSSEC key responses kept by the recursor.
    pub cache_entries: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ipv6: false,
            max_outbound_per_second: 500,
            max_outbound_inflight: 256,
            max_depth: 24,
            max_queries_per_request: 200,
            cache_entries: 65_536,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=10_000).contains(&self.max_outbound_per_second),
            "recursion.max_outbound_per_second must be in 1..=10000"
        );
        ensure!(
            (1..=4096).contains(&self.max_outbound_inflight),
            "recursion.max_outbound_inflight must be in 1..=4096"
        );
        ensure!(
            (4..=64).contains(&self.max_depth),
            "recursion.max_depth must be in 4..=64"
        );
        ensure!(
            (1..=1000).contains(&self.max_queries_per_request),
            "recursion.max_queries_per_request must be in 1..=1000"
        );
        ensure!(
            (1024..=1_048_576).contains(&self.cache_entries),
            "recursion.cache_entries must be in 1024..=1048576"
        );
        Ok(())
    }
}

const ROOTS_V4: [Ipv4Addr; 13] = [
    Ipv4Addr::new(198, 41, 0, 4),
    Ipv4Addr::new(170, 247, 170, 2),
    Ipv4Addr::new(192, 33, 4, 12),
    Ipv4Addr::new(199, 7, 91, 13),
    Ipv4Addr::new(192, 203, 230, 10),
    Ipv4Addr::new(192, 5, 5, 241),
    Ipv4Addr::new(192, 112, 36, 4),
    Ipv4Addr::new(198, 97, 190, 53),
    Ipv4Addr::new(192, 36, 148, 17),
    Ipv4Addr::new(192, 58, 128, 30),
    Ipv4Addr::new(193, 0, 14, 129),
    Ipv4Addr::new(199, 7, 83, 42),
    Ipv4Addr::new(202, 12, 27, 33),
];

const ROOTS_V6: [Ipv6Addr; 13] = [
    Ipv6Addr::new(0x2001, 0x503, 0xba3e, 0, 0, 0, 0x2, 0x30),
    Ipv6Addr::new(0x2801, 0x1b8, 0x10, 0, 0, 0, 0, 0xb),
    Ipv6Addr::new(0x2001, 0x500, 0x2, 0, 0, 0, 0, 0xc),
    Ipv6Addr::new(0x2001, 0x500, 0x2d, 0, 0, 0, 0, 0xd),
    Ipv6Addr::new(0x2001, 0x500, 0xa8, 0, 0, 0, 0, 0xe),
    Ipv6Addr::new(0x2001, 0x500, 0x2f, 0, 0, 0, 0, 0xf),
    Ipv6Addr::new(0x2001, 0x500, 0x12, 0, 0, 0, 0, 0xd0d),
    Ipv6Addr::new(0x2001, 0x500, 0x1, 0, 0, 0, 0, 0x53),
    Ipv6Addr::new(0x2001, 0x7fe, 0, 0, 0, 0, 0, 0x53),
    Ipv6Addr::new(0x2001, 0x503, 0xc27, 0, 0, 0, 0x2, 0x30),
    Ipv6Addr::new(0x2001, 0x7fd, 0, 0, 0, 0, 0, 0x1),
    Ipv6Addr::new(0x2001, 0x500, 0x9f, 0, 0, 0, 0, 0x42),
    Ipv6Addr::new(0x2001, 0xdc3, 0, 0, 0, 0, 0, 0x35),
];

pub struct Recursion {
    since_ms: Option<u64>,
    recursor: Recursor<outbound::Provider>,
    bounds: Arc<outbound::Bounds>,
    inflight_limit: usize,
    shutdown: AtomicBool,
    counts: Counts,
}

impl Recursion {
    /// `settings` has passed `Settings::validate`. Must run inside a Tokio runtime.
    pub fn new(settings: &Settings) -> Result<Self> {
        let bounds = outbound::Bounds::new(
            settings.max_outbound_per_second,
            settings.max_outbound_inflight,
        );
        let mut options = RecursorOptions {
            response_cache_size: settings.cache_entries,
            ns_cache_size: usize::try_from(settings.cache_entries / 16)?,
            recursion_limit: settings.max_depth,
            ns_recursion_limit: settings.max_depth,
            max_queries_per_request: settings.max_queries_per_request,
            // Some authoritative servers answer NXDOMAIN for empty non-terminals.
            qname_minimization: QNameMinimization::Relaxed,
            ..RecursorOptions::default()
        };
        let mut roots: Vec<IpAddr> = ROOTS_V4.iter().copied().map(IpAddr::V4).collect();
        if settings.ipv6 {
            roots.extend(ROOTS_V6.iter().copied().map(IpAddr::V6));
        } else {
            options.deny_server.push("::/0".parse()?);
        }
        let recursor = Recursor::new(
            &roots,
            DnssecPolicy::ValidateWithStaticKey(DnssecConfig::default()),
            None,
            options,
            outbound::Provider::new(bounds.clone()),
        )
        .map_err(|e| anyhow!("recursion setup failed: {e}"))?;
        Ok(Self {
            since_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|v| v.as_millis().try_into().ok()),
            recursor,
            bounds,
            inflight_limit: settings.max_outbound_inflight,
            shutdown: AtomicBool::new(false),
            counts: Counts::default(),
        })
    }

    /// Resolve the prepared outbound query. The caller bounds the time.
    pub async fn exchange(&self, query: &Message) -> Result<Message> {
        ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "recursion is shut down"
        );
        let question = query
            .queries
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("query has no question"))?;
        let subnet = ecs::subnet(query);
        let dnssec_ok = query.edns.as_ref().is_some_and(|e| e.flags().dnssec_ok);
        let mut options = ResolveOptions::default();
        options.client_subnet = subnet;
        // The Resolver's cache owns answers, partitioned by ECS scope.
        options.bypass_response_cache = true;
        // CD: the client validates; return data unchecked (RFC 4035 §3.2.2).
        options.checking_disabled = query.checking_disabled;
        self.counts.started.fetch_add(1, Ordering::Relaxed);
        // Counts a resolution the Resolver drops at its deadline (or at shutdown).
        let mut pending = Pending(Some(self));
        let resolution = self
            .recursor
            .resolve_with(question, Instant::now(), dnssec_ok, options)
            .await;
        pending.0 = None;
        let outcome = respond(query, resolution.result, resolution.ecs_scope);
        self.counts
            .record(&outcome, self.shutdown.load(Ordering::Acquire));
        outcome.response
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.bounds.close();
    }

    /// Fixed-cardinality counters for the local Prometheus exposition:
    /// (name, help, gauge).
    pub fn metrics(&self) -> Vec<(&'static str, &'static str, u64, bool)> {
        let c = &self.counts;
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        vec![
            (
                "recursion_started",
                "Recursive resolutions started.",
                load(&c.started),
                false,
            ),
            (
                "recursion_answered",
                "Recursive resolutions with an answer.",
                load(&c.answered),
                false,
            ),
            (
                "recursion_negative",
                "Recursive resolutions ending in NXDOMAIN or NODATA.",
                load(&c.negative),
                false,
            ),
            (
                "recursion_bogus",
                "Recursive resolutions with DNSSEC Bogus data.",
                load(&c.bogus),
                false,
            ),
            (
                "recursion_deadline",
                "Recursive resolutions cancelled at the query deadline.",
                load(&c.deadline),
                false,
            ),
            (
                "recursion_shutdown",
                "Recursive resolutions ended by shutdown.",
                load(&c.shutdown),
                false,
            ),
            (
                "recursion_failed_timeout",
                "Recursive resolutions failed by authoritative timeouts.",
                load(&c.fail_timeout),
                false,
            ),
            (
                "recursion_failed_referral",
                "Recursive resolutions failed on an unfollowed referral.",
                load(&c.fail_referral),
                false,
            ),
            (
                "recursion_failed_budget",
                "Recursive resolutions over a query or depth budget.",
                load(&c.fail_budget),
                false,
            ),
            (
                "recursion_failed_other",
                "Recursive resolutions failed for other reasons.",
                load(&c.fail_other),
                false,
            ),
            (
                "recursion_outbound_sent",
                "Queries sent to authoritative servers.",
                load(&self.bounds.sent),
                false,
            ),
            (
                "recursion_outbound_rate_waits",
                "Authoritative queries that waited for a rate token.",
                load(&self.bounds.rate_waits),
                false,
            ),
            (
                "recursion_outbound_rate_wait_ms",
                "Milliseconds spent waiting for rate tokens.",
                load(&self.bounds.rate_wait_us) / 1000,
                false,
            ),
            (
                "recursion_outbound_inflight",
                "Authoritative queries awaiting a response.",
                self.bounds.inflight(self.inflight_limit) as u64,
                true,
            ),
        ]
    }

    pub fn diagnostics_snapshot(&self) -> serde_json::Value {
        let c = &self.counts;
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        serde_json::json!({
            "scope": "recursion_generation",
            "since_ms": self.since_ms,
            "resolutions": {
                "started": load(&c.started), "answered": load(&c.answered),
                "negative": load(&c.negative), "bogus": load(&c.bogus),
                "failed": load(&c.failed), "deadline": load(&c.deadline),
                "shutdown": load(&c.shutdown),
            },
            "failures": {
                "timeout": load(&c.fail_timeout), "referral": load(&c.fail_referral),
                "budget": load(&c.fail_budget), "other": load(&c.fail_other),
            },
            "dnssec": {
                "secure": load(&c.secure), "insecure": load(&c.insecure),
                "bogus": load(&c.bogus), "indeterminate": load(&c.indeterminate),
            },
            "ecs_scope": {
                "not_sent": load(&c.ecs_not_sent), "absent": load(&c.ecs_absent),
                "scoped": load(&c.ecs_scoped),
            },
            "outbound": {
                "sent": load(&self.bounds.sent),
                "inflight": self.bounds.inflight(self.inflight_limit),
                "rate_waits": load(&self.bounds.rate_waits),
                "rate_wait_ms": load(&self.bounds.rate_wait_us) / 1000,
                "refused_after_shutdown": load(&self.bounds.refused),
            },
        })
    }
}

/// A resolution still in progress; dropping it unfinished records why it ended.
struct Pending<'a>(Option<&'a Recursion>);

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(recursion) = self.0 {
            let counts = &recursion.counts;
            if recursion.shutdown.load(Ordering::Acquire) {
                counts.shutdown.fetch_add(1, Ordering::Relaxed);
            } else {
                counts.deadline.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
struct Counts {
    started: AtomicU64,
    answered: AtomicU64,
    negative: AtomicU64,
    bogus: AtomicU64,
    failed: AtomicU64,
    shutdown: AtomicU64,
    deadline: AtomicU64,
    fail_timeout: AtomicU64,
    fail_referral: AtomicU64,
    fail_budget: AtomicU64,
    fail_other: AtomicU64,
    secure: AtomicU64,
    insecure: AtomicU64,
    indeterminate: AtomicU64,
    ecs_not_sent: AtomicU64,
    ecs_absent: AtomicU64,
    ecs_scoped: AtomicU64,
}

impl Counts {
    fn record(&self, outcome: &Outcome, shutting_down: bool) {
        let add = |v: &AtomicU64| v.fetch_add(1, Ordering::Relaxed);
        match outcome.kind {
            Kind::Answered => add(&self.answered),
            Kind::Negative => add(&self.negative),
            Kind::Bogus => add(&self.bogus),
            // Cancellation and shutdown are not resolution faults.
            Kind::Failed(_) if shutting_down => add(&self.shutdown),
            Kind::Failed(kind) => {
                add(match kind {
                    FailKind::Timeout => &self.fail_timeout,
                    FailKind::Referral => &self.fail_referral,
                    FailKind::Budget => &self.fail_budget,
                    FailKind::Other => &self.fail_other,
                });
                add(&self.failed)
            }
        };
        match outcome.proof {
            Some(Validation::Secure) => add(&self.secure),
            Some(Validation::Insecure) => add(&self.insecure),
            Some(Validation::Indeterminate) => add(&self.indeterminate),
            Some(Validation::Bogus) | None => 0,
        };
        match outcome.scope {
            Some(EcsScope::Absent) => add(&self.ecs_absent),
            Some(EcsScope::Scope(_)) => add(&self.ecs_scoped),
            Some(_) => add(&self.ecs_not_sent),
            None => 0,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Answered,
    Negative,
    Bogus,
    Failed(FailKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailKind {
    Timeout,
    Referral,
    Budget,
    Other,
}

impl FailKind {
    /// The validating recursor passes most errors on as text, so this falls back to the
    /// message. Diagnostics only: the answer is the same SERVFAIL either way.
    fn of(error: &RecursorError) -> Self {
        match error {
            RecursorError::Timeout | RecursorError::Net(NetError::Timeout) => Self::Timeout,
            RecursorError::ForwardNS(_) => Self::Referral,
            RecursorError::QueryBudgetExceeded
            | RecursorError::RecursionLimitExceeded { .. }
            | RecursorError::MaxRecordLimitExceeded { .. } => Self::Budget,
            other => {
                let text = other.to_string();
                if text.contains("forward NS Response") {
                    Self::Referral
                } else if text.contains("timed out") {
                    Self::Timeout
                } else if text.contains("budget") || text.contains("limit exceeded") {
                    Self::Budget
                } else {
                    Self::Other
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Validation {
    Secure,
    Insecure,
    Bogus,
    Indeterminate,
}

struct Outcome {
    response: Result<Message>,
    kind: Kind,
    proof: Option<Validation>,
    scope: Option<EcsScope>,
}

/// Translate a recursive result for `query` into a response message.
fn respond(query: &Message, result: Result<Message, RecursorError>, scope: EcsScope) -> Outcome {
    let (code, answers, authorities) = match result {
        Ok(message) => (
            message.metadata.response_code,
            message.answers,
            message.authorities,
        ),
        Err(error) => match classify(error) {
            Failure::Negative(negative) => (negative.code, Vec::new(), negative.authorities),
            Failure::Bogus => return bogus(query, scope),
            Failure::Other(error, kind) => {
                return Outcome {
                    response: Err(error),
                    kind: Kind::Failed(kind),
                    proof: None,
                    scope: None,
                };
            }
        },
    };
    let answered = answers
        .iter()
        .any(|record| record.record_type() == query.queries[0].query_type());
    // A positive answer stands on its answer records; its authority section only
    // carries delegation NS RRsets, which are unsigned by design (validated as Bogus).
    // A negative answer keeps its SOA and denial records, which carry its proof.
    let authorities = if answered { Vec::new() } else { authorities };
    let proof = validation(answers.iter().chain(&authorities));
    if proof == Some(Validation::Bogus) {
        return bogus(query, scope);
    }
    let mut response = header(query, code);
    response.answers = answers;
    response.authorities = authorities;
    response.metadata.authentic_data = proof == Some(Validation::Secure);
    echo_scope(query, &mut response, scope);
    Outcome {
        response: Ok(response),
        kind: if answered && code == ResponseCode::NoError {
            Kind::Answered
        } else {
            Kind::Negative
        },
        proof,
        scope: Some(scope),
    }
}

/// The weakest proof among the records; None for an empty set.
fn validation<'a>(records: impl Iterator<Item = &'a Record>) -> Option<Validation> {
    records
        .filter(|record| record.record_type() != RecordType::OPT)
        .map(|record| match record.proof {
            Proof::Secure => Validation::Secure,
            Proof::Insecure => Validation::Insecure,
            Proof::Bogus => Validation::Bogus,
            _ => Validation::Indeterminate,
        })
        .max_by_key(|proof| match proof {
            Validation::Secure => 0,
            Validation::Insecure => 1,
            Validation::Indeterminate => 2,
            Validation::Bogus => 3,
        })
}

/// The authorities' ECS behaviour as an echo of the prepared client subnet.
fn echo_scope(query: &Message, response: &mut Message, scope: EcsScope) {
    let Some(sent) = ecs::subnet(query) else {
        return;
    };
    let prefix = match scope {
        EcsScope::Scope(prefix) => prefix,
        // No authority returned ECS: the answer does not depend on the network.
        _ => 0,
    };
    ecs::set_subnet(
        response,
        Some(ClientSubnet::new(sent.addr(), sent.source_prefix(), prefix)),
    );
}

struct Negative {
    code: ResponseCode,
    authorities: Vec<Record>,
}

enum Failure {
    Negative(Negative),
    Bogus,
    Other(anyhow::Error, FailKind),
}

fn classify(error: RecursorError) -> Failure {
    let no_records = match error {
        RecursorError::Negative(data) => NoRecords::from(data),
        RecursorError::Net(NetError::Dns(DnsError::NoRecordsFound(no_records)))
            if no_records.ns.is_none() =>
        {
            no_records
        }
        RecursorError::Net(NetError::Dns(DnsError::Nsec { .. } | DnsError::DnssecBogus)) => {
            return Failure::Bogus;
        }
        other => {
            let kind = FailKind::of(&other);
            return Failure::Other(anyhow!("recursion failed: {other}"), kind);
        }
    };
    let mut authorities = no_records
        .authorities
        .map(|records| records.to_vec())
        .unwrap_or_default();
    if let Some(soa) = no_records.soa {
        let soa = soa.into_record_of_rdata();
        if !authorities.contains(&soa) {
            authorities.push(soa);
        }
    }
    Failure::Negative(Negative {
        code: no_records.response_code,
        authorities,
    })
}

/// A response this module produced for DNSSEC Bogus data.
pub fn is_bogus(response: &Message) -> bool {
    response.metadata.response_code == ResponseCode::ServFail
        && response
            .edns
            .as_ref()
            .and_then(|edns| edns.option(EdnsCode::Unknown(EDE)))
            .is_some_and(|option| {
                matches!(option, EdnsOption::Unknown(_, data)
                    if data.get(..2) == Some(&EDE_DNSSEC_BOGUS.to_be_bytes()[..]))
            })
}

fn bogus(query: &Message, scope: EcsScope) -> Outcome {
    let mut response = header(query, ResponseCode::ServFail);
    response
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(
            EDE,
            EDE_DNSSEC_BOGUS.to_be_bytes().to_vec(),
        ));
    Outcome {
        response: Ok(response),
        kind: Kind::Bogus,
        proof: Some(Validation::Bogus),
        scope: Some(scope),
    }
}

fn header(query: &Message, code: ResponseCode) -> Message {
    let mut response = Message::response(query.id, OpCode::Query);
    response.metadata.message_type = MessageType::Response;
    response.metadata.recursion_desired = query.recursion_desired;
    response.metadata.recursion_available = true;
    response.metadata.checking_disabled = query.checking_disabled;
    response.metadata.response_code = code;
    response.queries = query.queries.clone();
    if let Some(client) = &query.edns {
        let mut edns = Edns::new();
        edns.set_max_payload(MAX_UDP_PAYLOAD)
            .set_dnssec_ok(client.flags().dnssec_ok);
        response.edns = Some(edns);
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hickory_proto::{
        op::Query,
        rr::{Name, RData, rdata::A, rdata::SOA},
    };
    use hickory_resolver::recursor::AuthorityData;

    use super::*;

    fn query(subnet: Option<&str>, dnssec_ok: bool) -> Message {
        let mut query = Message::query();
        query.metadata.id = 7;
        query.metadata.recursion_desired = true;
        query.add_query(Query::query(
            Name::from_ascii("www.example.").unwrap(),
            RecordType::A,
        ));
        let mut edns = Edns::new();
        edns.set_dnssec_ok(dnssec_ok);
        query.edns = Some(edns);
        ecs::set_subnet(&mut query, subnet.map(|s| s.parse().unwrap()));
        query
    }

    fn a(proof: Proof) -> Record {
        let mut record = Record::from_rdata(
            Name::from_ascii("www.example.").unwrap(),
            60,
            RData::A(A::new(192, 0, 2, 1)),
        );
        record.proof = proof;
        record
    }

    fn soa() -> Record<SOA> {
        let name = Name::from_ascii("example.").unwrap();
        Record::from_rdata(
            name.clone(),
            300,
            SOA::new(name.clone(), name, 1, 1, 1, 1, 60),
        )
    }

    fn answer(records: Vec<Record>) -> Result<Message, RecursorError> {
        let mut message = Message::query();
        message.answers = records;
        Ok(message)
    }

    fn ede(response: &Message) -> Option<Vec<u8>> {
        match response.edns.as_ref()?.option(EdnsCode::Unknown(EDE))? {
            EdnsOption::Unknown(_, data) => Some(data.clone()),
            _ => None,
        }
    }

    #[test]
    fn secure_answer_sets_ad_and_echoes_authority_scope() {
        let q = query(Some("198.51.100.0/24"), true);
        let outcome = respond(&q, answer(vec![a(Proof::Secure)]), EcsScope::Scope(20));
        let response = outcome.response.unwrap();
        assert_eq!(outcome.kind, Kind::Answered);
        assert_eq!(response.id, 7);
        assert!(response.authentic_data && response.recursion_available);
        let echo = ecs::subnet(&response).unwrap();
        assert_eq!((echo.source_prefix(), echo.scope_prefix()), (24, 20));
    }

    #[test]
    fn unsigned_delegation_ns_does_not_fail_a_secure_answer() {
        let q = query(None, true);
        let mut message = Message::query();
        message.answers = vec![a(Proof::Secure)];
        let mut ns = Record::from_rdata(
            Name::from_ascii("glb.example.").unwrap(),
            300,
            RData::NS(hickory_proto::rr::rdata::NS(
                Name::from_ascii("ns.glb.example.").unwrap(),
            )),
        );
        ns.proof = Proof::Bogus;
        message.authorities = vec![ns];
        let outcome = respond(&q, Ok(message), EcsScope::NotSent);
        let response = outcome.response.unwrap();
        assert_eq!(outcome.kind, Kind::Answered);
        assert_eq!(response.response_code, ResponseCode::NoError);
        assert!(response.authentic_data && response.authorities.is_empty());
    }

    #[test]
    fn missing_authority_ecs_is_echoed_as_global_scope() {
        let q = query(Some("198.51.100.0/24"), false);
        for scope in [EcsScope::Absent, EcsScope::NotSent] {
            let response = respond(&q, answer(vec![a(Proof::Insecure)]), scope)
                .response
                .unwrap();
            assert!(!response.authentic_data);
            assert_eq!(ecs::subnet(&response).unwrap().scope_prefix(), 0);
        }
        // Without client ECS nothing is echoed.
        let response = respond(
            &query(None, false),
            answer(vec![a(Proof::Secure)]),
            EcsScope::NotSent,
        )
        .response
        .unwrap();
        assert!(ecs::subnet(&response).is_none());
    }

    #[test]
    fn any_bogus_record_becomes_servfail_with_ede_6() {
        let q = query(None, true);
        let outcome = respond(
            &q,
            answer(vec![a(Proof::Secure), a(Proof::Bogus)]),
            EcsScope::NotSent,
        );
        let response = outcome.response.unwrap();
        assert_eq!(outcome.kind, Kind::Bogus);
        assert_eq!(response.response_code, ResponseCode::ServFail);
        assert!(response.answers.is_empty() && !response.authentic_data);
        assert_eq!(ede(&response), Some(vec![0, 6]));
        assert!(is_bogus(&response));
        assert!(!is_bogus(&header(&q, ResponseCode::ServFail)));
    }

    #[test]
    fn failed_validation_error_becomes_servfail_with_ede_6() {
        // `DnsError::Nsec` (a failed denial proof) shares this arm but is not
        // constructible outside hickory.
        let q = query(None, true);
        let error = RecursorError::Net(NetError::Dns(DnsError::DnssecBogus));
        let response = respond(&q, Err(error), EcsScope::NotSent).response.unwrap();
        assert_eq!(response.response_code, ResponseCode::ServFail);
        assert_eq!(ede(&response), Some(vec![0, 6]));
    }

    #[test]
    fn negative_errors_become_negative_answers_with_soa() {
        let q = query(Some("198.51.100.0/24"), false);
        for nx in [true, false] {
            let error = RecursorError::Negative(AuthorityData::new(
                Box::new(q.queries[0].clone()),
                Some(Box::new(soa())),
                true,
                nx,
                Some(Arc::from(Vec::<Record>::new())),
            ));
            let outcome = respond(&q, Err(error), EcsScope::Scope(0));
            let response = outcome.response.unwrap();
            assert_eq!(outcome.kind, Kind::Negative);
            assert_eq!(
                response.response_code,
                if nx {
                    ResponseCode::NXDomain
                } else {
                    ResponseCode::NoError
                }
            );
            assert!(response.answers.is_empty());
            assert_eq!(response.authorities[0].record_type(), RecordType::SOA);
            assert_eq!(ecs::subnet(&response).unwrap().scope_prefix(), 0);
        }
    }

    #[test]
    fn other_failures_are_errors_for_the_resolver() {
        let q = query(None, false);
        let outcome = respond(&q, Err(RecursorError::Timeout), EcsScope::NotSent);
        assert!(outcome.response.is_err());
        assert_eq!(outcome.kind, Kind::Failed(FailKind::Timeout));
        for (error, kind) in [
            (
                RecursorError::from("net error: forward NS Response"),
                FailKind::Referral,
            ),
            (
                RecursorError::from("no nameserver found for a.: net error: request timed out"),
                FailKind::Timeout,
            ),
            (RecursorError::QueryBudgetExceeded, FailKind::Budget),
            (RecursorError::from("no glue found"), FailKind::Other),
        ] {
            assert_eq!(FailKind::of(&error), kind, "{error}");
        }
    }

    #[tokio::test]
    async fn unfinished_resolutions_count_as_deadline_or_shutdown() {
        let recursion = Recursion::new(&Settings::default()).unwrap();
        drop(Pending(Some(&recursion)));
        drop(Pending(None));
        recursion.shutdown();
        drop(Pending(Some(&recursion)));
        let snapshot = recursion.diagnostics_snapshot();
        assert_eq!(snapshot["resolutions"]["deadline"], 1);
        assert_eq!(snapshot["resolutions"]["shutdown"], 1);
    }

    #[test]
    fn settings_bounds() {
        assert!(Settings::default().validate().is_ok());
        for broken in [
            Settings {
                max_outbound_per_second: 0,
                ..Settings::default()
            },
            Settings {
                max_outbound_inflight: 0,
                ..Settings::default()
            },
            Settings {
                max_depth: 3,
                ..Settings::default()
            },
            Settings {
                max_queries_per_request: 0,
                ..Settings::default()
            },
            Settings {
                cache_entries: 1,
                ..Settings::default()
            },
        ] {
            assert!(broken.validate().is_err());
        }
    }
}
