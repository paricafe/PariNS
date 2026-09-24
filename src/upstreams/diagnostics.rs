//! Bounded diagnostics for actual upstream attempts, not client requests.
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActualProtocol {
    Udp,
    Tcp,
    Dot,
    Doh2,
    Doh3,
    Doq,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Wait,
    Bootstrap,
    Connect,
    TlsHandshake,
    QuicHandshake,
    RequestWrite,
    ResponseHeaders,
    ResponseRead,
    Decode,
    Validate,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Deadline,
    ConnectIo,
    Tls,
    HttpStatus,
    ProtocolInvalid,
    PeerClosed,
    CallerCancelled,
    Shutdown,
    Internal,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptRecord {
    pub pool_generation: u64,
    pub slot: usize,
    pub protocol: Option<ActualProtocol>,
    pub stage: Stage,
    pub outcome: Outcome,
    pub reason: Option<Reason>,
    pub code: Option<u64>,
    pub elapsed_ms: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Trace {
    pub attempts: Vec<AttemptRecord>,
    pub omitted: u64,
}
pub type Observer = Arc<dyn Fn(&AttemptRecord) + Send + Sync>;

#[derive(Clone)]
pub struct Operation(Arc<OperationInner>);
struct OperationInner {
    // No request detail vector or mutex is allocated when logging is off.
    trace: Option<Mutex<Trace>>,
    observer: Option<Observer>,
    shutdown: AtomicBool,
}
impl Operation {
    pub fn new(trace_enabled: bool, observer: Option<Observer>) -> Self {
        Self(Arc::new(OperationInner {
            trace: trace_enabled.then(|| Mutex::new(Trace::default())),
            observer,
            shutdown: AtomicBool::new(false),
        }))
    }
    pub fn trace(&self) -> Option<Trace> {
        self.0
            .trace
            .as_ref()
            .map(|trace| trace.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
    pub fn cancel(&self, reason: Reason) {
        if reason == Reason::Shutdown {
            self.0.shutdown.store(true, Ordering::Release);
        }
    }
    fn record(&self, record: AttemptRecord) {
        if let Some(observer) = &self.0.observer {
            observer(&record);
        }
        if let Some(trace) = &self.0.trace {
            let mut trace = trace.lock().unwrap_or_else(|e| e.into_inner());
            if trace.attempts.len() < 8 {
                trace.attempts.push(record);
            } else {
                trace.omitted += 1;
                // Keep representative early attempts and the last substantive
                // terminal. Dropped race losers cannot evict its winner.
                if record.outcome == Outcome::Succeeded {
                    trace.attempts[7] = record;
                } else if record.outcome == Outcome::Failed {
                    let index = if trace.attempts[7].outcome == Outcome::Succeeded {
                        6
                    } else {
                        7
                    };
                    trace.attempts[index] = record;
                }
            }
        }
    }
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
pub(super) struct Diagnostics {
    generation: u64,
    since_ms: Option<u64>,
    shutdown: AtomicBool,
    slots: Vec<Box<[AtomicU64; 2800]>>,
}
#[derive(Clone, Serialize)]
struct Count {
    protocol: Option<ActualProtocol>,
    stage: Stage,
    outcome: Outcome,
    reason: Option<Reason>,
    count: u64,
}
impl Diagnostics {
    pub fn new(slots: usize) -> Arc<Self> {
        Arc::new(Self {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            since_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|v| v.as_millis().try_into().ok()),
            shutdown: AtomicBool::new(false),
            slots: (0..slots)
                .map(|_| Box::new(std::array::from_fn(|_| AtomicU64::new(0))))
                .collect(),
        })
    }
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"scope":"pool_generation","pool_generation":self.generation,"since_ms":self.since_ms,
            "slots":self.slots.iter().enumerate().map(|(slot, counts)| {
                let mut values = Vec::new();
                for protocol in [Some(ActualProtocol::Udp),Some(ActualProtocol::Tcp),Some(ActualProtocol::Dot),Some(ActualProtocol::Doh2),Some(ActualProtocol::Doh3),Some(ActualProtocol::Doq),None] {
                    for stage in [Stage::Wait,Stage::Bootstrap,Stage::Connect,Stage::TlsHandshake,Stage::QuicHandshake,Stage::RequestWrite,Stage::ResponseHeaders,Stage::ResponseRead,Stage::Decode,Stage::Validate] {
                        for outcome in [Outcome::Succeeded,Outcome::Failed,Outcome::Cancelled,Outcome::Skipped] {
                            for reason in [Some(Reason::Deadline),Some(Reason::ConnectIo),Some(Reason::Tls),Some(Reason::HttpStatus),Some(Reason::ProtocolInvalid),Some(Reason::PeerClosed),Some(Reason::CallerCancelled),Some(Reason::Shutdown),Some(Reason::Internal),None] {
                                let count = counts[counter_index(protocol,stage,outcome,reason)].load(Ordering::Relaxed);
                                if count > 0 { values.push(Count{protocol,stage,outcome,reason,count}); }
                            }
                        }
                    }
                }
                serde_json::json!({"slot":slot,"counts":values})
            }).collect::<Vec<_>>()})
    }
    fn record(&self, record: &AttemptRecord) {
        self.slots[record.slot]
            [counter_index(record.protocol, record.stage, record.outcome, record.reason)]
        .fetch_add(1, Ordering::Relaxed);
    }
}

fn counter_index(
    protocol: Option<ActualProtocol>,
    stage: Stage,
    outcome: Outcome,
    reason: Option<Reason>,
) -> usize {
    (((protocol.map_or(6, |v| v as usize) * 10 + stage as usize) * 4 + outcome as usize) * 10)
        + reason.map_or(9, |v| v as usize)
}

#[derive(Clone)]
pub(crate) struct AttemptScope {
    diagnostics: Arc<Diagnostics>,
    operation: Operation,
    slot: usize,
    deadline: Instant,
    race_lost: Option<Arc<AtomicBool>>,
}
impl AttemptScope {
    pub(super) fn new(
        diagnostics: &Arc<Diagnostics>,
        operation: &Operation,
        slot: usize,
        deadline: Instant,
    ) -> Self {
        Self {
            diagnostics: diagnostics.clone(),
            operation: operation.clone(),
            slot,
            deadline,
            race_lost: None,
        }
    }
    pub(super) fn racing(mut self, flag: &Arc<AtomicBool>) -> Self {
        self.race_lost = Some(flag.clone());
        self
    }
    pub(crate) fn start(&self, protocol: Option<ActualProtocol>) -> Attempt {
        Attempt {
            scope: self.clone(),
            started: Instant::now(),
            protocol,
            stage: Stage::Wait,
            reason: None,
            code: None,
            finished: false,
        }
    }
    pub(crate) fn remaining(&self) -> bool {
        Instant::now() < self.deadline && !self.diagnostics.shutdown.load(Ordering::Acquire)
    }
}
pub(crate) struct Attempt {
    scope: AttemptScope,
    started: Instant,
    protocol: Option<ActualProtocol>,
    stage: Stage,
    reason: Option<Reason>,
    code: Option<u64>,
    finished: bool,
}
impl Attempt {
    pub(crate) fn stage(&mut self, stage: Stage) {
        self.stage = stage;
        self.reason = None;
        self.code = None;
    }
    pub(crate) fn reason(&mut self, reason: Reason) {
        self.reason = Some(reason);
    }
    pub(crate) fn http_status(&mut self, status: u16) {
        self.reason = Some(Reason::HttpStatus);
        self.code = Some(u64::from(status));
    }
    pub(crate) fn finish<T>(&mut self, result: &anyhow::Result<T>) {
        if let Err(error) = result {
            if let Some(error) = error.downcast_ref::<h2::Error>() {
                self.reason = Some(if error.is_io() || error.is_reset() || error.is_go_away() {
                    Reason::PeerClosed
                } else {
                    Reason::ProtocolInvalid
                });
                self.code = error.reason().map(|code| u64::from(u32::from(code)));
            }
            if let Some(error) = error.downcast_ref::<h3::error::StreamError>() {
                use h3::error::StreamError::*;
                match error {
                    StreamError { code, .. } => {
                        self.code = Some(code.value());
                        self.reason = Some(Reason::ProtocolInvalid);
                    }
                    RemoteTerminate { code, .. } => {
                        self.code = Some(code.value());
                        self.reason = Some(Reason::PeerClosed);
                    }
                    HeaderTooBig { .. } => self.reason = Some(Reason::ProtocolInvalid),
                    h3::error::StreamError::RemoteClosing { .. } => {
                        self.reason = Some(Reason::PeerClosed)
                    }
                    _ => {}
                }
            }
            if let Some(error) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<quinn::ConnectionError>())
            {
                use quinn::ConnectionError::*;
                let reason = match error {
                    TransportError(error) => {
                        let code = u64::from(error.code);
                        self.code = Some(code);
                        if (0x100..=0x1ff).contains(&code) {
                            Reason::Tls
                        } else {
                            Reason::ProtocolInvalid
                        }
                    }
                    ConnectionClosed(error) => {
                        self.code = Some(u64::from(error.error_code));
                        Reason::PeerClosed
                    }
                    ApplicationClosed(error) => {
                        self.code = Some(error.error_code.into_inner());
                        Reason::PeerClosed
                    }
                    VersionMismatch => Reason::ProtocolInvalid,
                    Reset => Reason::PeerClosed,
                    TimedOut => Reason::Deadline,
                    LocallyClosed => Reason::CallerCancelled,
                    CidsExhausted => Reason::Internal,
                };
                self.reason = Some(reason);
            }
            let reason = self.reason.unwrap_or_else(|| {
                if Instant::now() >= self.scope.deadline {
                    return Reason::Deadline;
                }
                if let Some(error) = error.downcast_ref::<std::io::Error>() {
                    use std::io::ErrorKind::*;
                    if matches!(
                        error.kind(),
                        UnexpectedEof
                            | BrokenPipe
                            | ConnectionReset
                            | ConnectionAborted
                            | NotConnected
                    ) {
                        return Reason::PeerClosed;
                    }
                }
                match self.stage {
                    Stage::TlsHandshake => Reason::Tls,
                    Stage::Decode | Stage::Validate => Reason::ProtocolInvalid,
                    Stage::Connect | Stage::Bootstrap => Reason::ConnectIo,
                    Stage::RequestWrite | Stage::ResponseHeaders | Stage::ResponseRead => {
                        Reason::PeerClosed
                    }
                    _ => Reason::Internal,
                }
            });
            let outcome = if matches!(reason, Reason::CallerCancelled | Reason::Shutdown) {
                Outcome::Cancelled
            } else {
                Outcome::Failed
            };
            self.terminal(outcome, Some(reason));
        } else {
            self.terminal(Outcome::Succeeded, None);
        }
    }
    pub(crate) fn skipped(&mut self) {
        self.terminal(Outcome::Skipped, None);
    }
    fn terminal(&mut self, outcome: Outcome, reason: Option<Reason>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let record = AttemptRecord {
            pool_generation: self.scope.diagnostics.generation,
            slot: self.scope.slot,
            protocol: self.protocol,
            stage: self.stage,
            outcome,
            reason,
            code: self.code,
            elapsed_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        };
        self.scope.diagnostics.record(&record);
        self.scope.operation.record(record);
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.scope.diagnostics.shutdown.load(Ordering::Acquire)
            || self.scope.operation.0.shutdown.load(Ordering::Acquire)
        {
            self.terminal(Outcome::Cancelled, Some(Reason::Shutdown));
        } else if self
            .scope
            .race_lost
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            self.terminal(Outcome::Cancelled, Some(Reason::CallerCancelled));
        } else if Instant::now() >= self.scope.deadline {
            self.terminal(Outcome::Failed, Some(Reason::Deadline));
        } else {
            self.terminal(Outcome::Cancelled, Some(Reason::CallerCancelled));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn representative_trace_is_bounded_and_preserves_winner_and_final_failure() {
        let diagnostics = Diagnostics::new(1);
        let operation = Operation::new(true, None);
        let scope = AttemptScope::new(
            &diagnostics,
            &operation,
            0,
            Instant::now() + Duration::from_secs(1),
        );
        for _ in 0..12 {
            let mut attempt = scope.start(Some(ActualProtocol::Doh3));
            attempt.stage(Stage::Connect);
            attempt.finish(&Err::<(), _>(anyhow::anyhow!("fixture")));
        }
        let mut winner = scope.start(Some(ActualProtocol::Doh2));
        winner.finish(&Ok::<(), anyhow::Error>(()));
        drop(scope.start(Some(ActualProtocol::Udp)));
        let mut final_failure = scope.start(Some(ActualProtocol::Dot));
        final_failure.stage(Stage::Decode);
        final_failure.finish(&Err::<(), _>(anyhow::anyhow!("fixture")));
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts.len(), 8);
        assert_eq!(trace.omitted, 7);
        assert!(
            trace.attempts.iter().any(
                |r| r.protocol == Some(ActualProtocol::Doh2) && r.outcome == Outcome::Succeeded
            )
        );
        assert!(
            trace
                .attempts
                .iter()
                .any(|r| r.protocol == Some(ActualProtocol::Dot)
                    && r.reason == Some(Reason::ProtocolInvalid))
        );
        let total: u64 = diagnostics.slots[0]
            .iter()
            .map(|v| v.load(Ordering::Relaxed))
            .sum();
        assert_eq!(total, 15);
    }

    #[test]
    fn terminal_guard_finishes_once_and_quic_crypto_errors_are_typed() {
        let diagnostics = Diagnostics::new(1);
        let operation = Operation::new(true, None);
        let scope = AttemptScope::new(
            &diagnostics,
            &operation,
            0,
            Instant::now() + Duration::from_secs(1),
        );
        let mut attempt = scope.start(Some(ActualProtocol::Doq));
        attempt.stage(Stage::QuicHandshake);
        attempt.finish(&Err::<(), _>(quinn::ConnectionError::Reset.into()));
        attempt.finish(&Ok::<(), anyhow::Error>(()));
        drop(attempt);
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts.len(), 1);
        assert_eq!(trace.attempts[0].reason, Some(Reason::PeerClosed));
        assert_eq!(trace.attempts[0].stage, Stage::QuicHandshake);
    }

    #[test]
    fn known_race_cancellation_is_not_reclassified_at_deadline_boundary() {
        let diagnostics = Diagnostics::new(1);
        let operation = Operation::new(true, None);
        let won = Arc::new(AtomicBool::new(true));
        let scope = AttemptScope::new(&diagnostics, &operation, 0, Instant::now()).racing(&won);
        drop(scope.start(Some(ActualProtocol::Udp)));
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts[0].outcome, Outcome::Cancelled);
        assert_eq!(trace.attempts[0].reason, Some(Reason::CallerCancelled));
    }
}
