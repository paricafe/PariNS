//! Fixed protocol/result dimensions; no peer-supplied labels or query records.
use super::Protocol;
use crate::metrics::Metrics;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

const NAMES: &[&str] = &[
    "incoming",
    "admission_global_rejected",
    "admission_source_rejected",
    "handshake_started",
    "handshake_established",
    "handshake_application_deadline",
    "handshake_quinn_timeout",
    "handshake_tls",
    "handshake_transport_error",
    "handshake_peer_closed",
    "handshake_version_mismatch",
    "handshake_cancelled",
    "handshake_shutdown",
    "stream_started",
    "stream_full_frame",
    "stream_read_deadline",
    "stream_request_deadline",
    "stream_write_deadline",
    "stream_protocol_invalid",
    "stream_peer_cancelled",
    "stream_connection_lost",
    "stream_response_handed_to_transport",
    "stream_write_failed",
    "stream_no_response",
    "stream_cancelled",
    "stream_shutdown",
    "http_response_2xx",
    "http_response_4xx",
    "http_response_5xx",
    "http_request_failed",
    "http_request_deadline",
];

#[derive(Clone, Copy)]
pub enum Event {
    Incoming,
    AdmissionGlobalRejected,
    AdmissionSourceRejected,
    HandshakeStarted,
    HandshakeEstablished,
    HandshakeApplicationDeadline,
    HandshakeQuinnTimeout,
    HandshakeTls,
    HandshakeTransportError,
    HandshakePeerClosed,
    HandshakeVersionMismatch,
    HandshakeCancelled,
    HandshakeShutdown,
    StreamStarted,
    StreamFullFrame,
    StreamReadDeadline,
    StreamRequestDeadline,
    StreamWriteDeadline,
    StreamProtocolInvalid,
    StreamPeerCancelled,
    StreamConnectionLost,
    StreamResponseHandedToTransport,
    StreamWriteFailed,
    StreamNoResponse,
    StreamCancelled,
    StreamShutdown,
    HttpResponse2xx,
    HttpResponse4xx,
    HttpResponse5xx,
    HttpRequestFailed,
    HttpRequestDeadline,
}

pub struct Counters {
    counters: [[AtomicU64; NAMES.len()]; 2],
    inflight: [[AtomicU64; 2]; 2],
}
impl Default for Counters {
    fn default() -> Self {
        Self {
            counters: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            inflight: Default::default(),
        }
    }
}
fn index(protocol: Protocol) -> usize {
    match protocol {
        Protocol::Doq => 0,
        Protocol::H3 => 1,
    }
}
impl Counters {
    pub fn inc(&self, protocol: Protocol, event: Event) {
        self.counters[index(protocol)][event as usize].fetch_add(1, Relaxed);
    }
    pub fn counters(&self) -> BTreeMap<String, u64> {
        ["doq", "doh3"]
            .iter()
            .enumerate()
            .flat_map(|(p, proto)| {
                NAMES.iter().enumerate().map(move |(i, name)| {
                    (
                        format!("quic_{proto}_{name}"),
                        self.counters[p][i].load(Relaxed),
                    )
                })
            })
            .collect()
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let view = |p: usize| {
            let mut result: serde_json::Value = NAMES
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    (
                        name.to_string(),
                        serde_json::json!(self.counters[p][i].load(Relaxed)),
                    )
                })
                .collect();
            result["handshake_inflight"] = serde_json::json!(self.inflight[p][0].load(Relaxed));
            result["stream_inflight"] = serde_json::json!(self.inflight[p][1].load(Relaxed));
            result
        };
        serde_json::json!({"scope":"process","doq":view(0),"doh3":view(1)})
    }
}

pub(super) struct Guard {
    metrics: Arc<Metrics>,
    protocol: Protocol,
    kind: usize,
    stop: tokio::sync::watch::Receiver<bool>,
    done: bool,
}
impl Guard {
    pub fn new(
        metrics: Arc<Metrics>,
        protocol: Protocol,
        stop: tokio::sync::watch::Receiver<bool>,
        stream: bool,
    ) -> Self {
        let kind = usize::from(stream);
        metrics.quic.inc(
            protocol,
            if stream {
                Event::StreamStarted
            } else {
                Event::HandshakeStarted
            },
        );
        metrics.quic.inflight[index(protocol)][kind].fetch_add(1, Relaxed);
        Self {
            metrics,
            protocol,
            kind,
            stop,
            done: false,
        }
    }
    pub fn event(&self, event: Event) {
        self.metrics.quic.inc(self.protocol, event);
    }
    pub fn finish(&mut self, event: Event) {
        if !self.done {
            self.event(event);
            self.done = true;
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.done {
            let stopping = *self.stop.borrow();
            self.finish(match (self.kind, stopping) {
                (0, true) => Event::HandshakeShutdown,
                (0, false) => Event::HandshakeCancelled,
                (_, true) => Event::StreamShutdown,
                (_, false) => Event::StreamCancelled,
            });
        }
        self.metrics.quic.inflight[index(self.protocol)][self.kind].fetch_sub(1, Relaxed);
    }
}

pub(super) fn handshake_error(error: &quinn::ConnectionError) -> Event {
    match error {
        quinn::ConnectionError::TimedOut => Event::HandshakeQuinnTimeout,
        quinn::ConnectionError::VersionMismatch => Event::HandshakeVersionMismatch,
        quinn::ConnectionError::TransportError(error)
            if (0x100..=0x1ff).contains(&u64::from(error.code)) =>
        {
            Event::HandshakeTls
        }
        quinn::ConnectionError::TransportError(_) => Event::HandshakeTransportError,
        quinn::ConnectionError::LocallyClosed => Event::HandshakeCancelled,
        _ => Event::HandshakePeerClosed,
    }
}

pub(super) fn write_error(error: &anyhow::Error) -> Event {
    match error.downcast_ref::<quinn::WriteError>() {
        Some(quinn::WriteError::Stopped(_)) => Event::StreamPeerCancelled,
        Some(quinn::WriteError::ConnectionLost(_)) => Event::StreamConnectionLost,
        _ => Event::StreamWriteFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_stage_preserves_typed_peer_cancellation_and_connection_loss() {
        assert!(matches!(
            write_error(&quinn::WriteError::Stopped(3u32.into()).into()),
            Event::StreamPeerCancelled
        ));
        assert!(matches!(
            write_error(
                &quinn::WriteError::ConnectionLost(quinn::ConnectionError::TimedOut).into()
            ),
            Event::StreamConnectionLost
        ));
        assert!(matches!(
            write_error(&anyhow::anyhow!("unknown write failure")),
            Event::StreamWriteFailed
        ));
    }
    #[test]
    fn guards_count_one_terminal_and_distinguish_shutdown() {
        let metrics = Arc::new(Metrics::default());
        let (stop, receiver) = tokio::sync::watch::channel(false);
        {
            let mut completed = Guard::new(metrics.clone(), Protocol::Doq, receiver.clone(), false);
            completed.finish(Event::HandshakeEstablished);
            completed.finish(Event::HandshakeTls);
        }
        drop(Guard::new(
            metrics.clone(),
            Protocol::Doq,
            receiver.clone(),
            true,
        ));
        stop.send(true).unwrap();
        drop(Guard::new(metrics.clone(), Protocol::Doq, receiver, true));
        let snapshot = metrics.quic.snapshot();
        assert_eq!(snapshot["doq"]["handshake_established"], 1);
        assert_eq!(snapshot["doq"]["handshake_tls"], 0);
        assert_eq!(snapshot["doq"]["stream_cancelled"], 1);
        assert_eq!(snapshot["doq"]["stream_shutdown"], 1);
        assert_eq!(snapshot["doq"]["handshake_inflight"], 0);
        assert_eq!(snapshot["doq"]["stream_inflight"], 0);
        assert_eq!(snapshot["doh3"]["incoming"], 0);
    }
}
