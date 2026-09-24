//! Fixed-cardinality cache decisions, shared by diagnostics and caller traces.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(usize)]
pub enum DecisionReason {
    UnsupportedQuery,
    UnsupportedEdns,
    EcsUnusable,
    TtlZero,
    NegativeWithoutSoa,
    UncacheableResponse,
    PolicyDisabled,
    Capacity,
    VariantLimit,
    EpochChanged,
}

const REASONS: [DecisionReason; 10] = [
    DecisionReason::UnsupportedQuery,
    DecisionReason::UnsupportedEdns,
    DecisionReason::EcsUnusable,
    DecisionReason::TtlZero,
    DecisionReason::NegativeWithoutSoa,
    DecisionReason::UncacheableResponse,
    DecisionReason::PolicyDisabled,
    DecisionReason::Capacity,
    DecisionReason::VariantLimit,
    DecisionReason::EpochChanged,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(usize)]
pub enum LookupOutcome {
    Fresh,
    Stale,
    Miss,
    Bypass,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupDecision {
    pub outcome: LookupOutcome,
    pub reason: Option<DecisionReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(usize)]
pub enum StoreOutcome {
    Admitted,
    Replaced,
    Skipped,
    SupersededOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreDecision {
    pub outcome: StoreOutcome,
    pub reason: Option<DecisionReason>,
}

impl StoreDecision {
    pub fn admitted(self) -> bool {
        matches!(
            self.outcome,
            StoreOutcome::Admitted | StoreOutcome::Replaced
        )
    }
    pub fn skipped(reason: DecisionReason) -> Self {
        Self {
            outcome: StoreOutcome::Skipped,
            reason: Some(reason),
        }
    }
    pub(super) fn rejected(reason: DecisionReason, superseded: bool) -> Self {
        Self {
            outcome: if superseded {
                StoreOutcome::SupersededOnly
            } else {
                StoreOutcome::Skipped
            },
            reason: Some(reason),
        }
    }
}

pub struct LookupResult {
    pub hit: Option<super::Hit>,
    pub decision: LookupDecision,
}

impl LookupResult {
    pub(super) fn empty(outcome: LookupOutcome, reason: Option<DecisionReason>) -> Self {
        Self {
            hit: None,
            decision: LookupDecision { outcome, reason },
        }
    }
}

pub(super) struct Diagnostics {
    since_ms: Option<u64>,
    lookup: [AtomicU64; 5],
    store: [AtomicU64; 4],
    lookup_reasons: [AtomicU64; 10],
    store_reasons: [AtomicU64; 10],
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            since_ms: std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|d| d.as_millis().try_into().ok()),
            lookup: std::array::from_fn(|_| AtomicU64::new(0)),
            store: std::array::from_fn(|_| AtomicU64::new(0)),
            lookup_reasons: std::array::from_fn(|_| AtomicU64::new(0)),
            store_reasons: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Diagnostics {
    pub(super) fn lookup(&self, decision: LookupDecision) {
        self.lookup[decision.outcome as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(reason) = decision.reason {
            self.lookup_reasons[reason as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(super) fn store(&self, decision: StoreDecision) {
        self.store[decision.outcome as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(reason) = decision.reason {
            self.store_reasons[reason as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(super) fn snapshot(&self) -> Value {
        let reasons = |counters: &[AtomicU64; 10]| -> Value {
            REASONS
                .iter()
                .map(|reason| {
                    (
                        serde_json::to_value(reason)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .to_owned(),
                        json!(counters[*reason as usize].load(Ordering::Relaxed)),
                    )
                })
                .collect()
        };
        json!({"scope":"cache_generation", "since_ms":self.since_ms, "lookup":{
            "fresh":self.lookup[0].load(Ordering::Relaxed), "stale":self.lookup[1].load(Ordering::Relaxed),
            "miss":self.lookup[2].load(Ordering::Relaxed), "bypass":self.lookup[3].load(Ordering::Relaxed),
            "disabled":self.lookup[4].load(Ordering::Relaxed), "reasons":reasons(&self.lookup_reasons)},
            "store":{"admitted":self.store[0].load(Ordering::Relaxed), "replaced":self.store[1].load(Ordering::Relaxed),
            "skipped":self.store[2].load(Ordering::Relaxed), "superseded_only":self.store[3].load(Ordering::Relaxed),
            "reasons":reasons(&self.store_reasons)}})
    }
}
