//! Optional HTTP/3 preference for an authenticated HTTPS endpoint.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use bytes::{Buf, Bytes};
use hickory_proto::op::Message;
use tokio::time::{Instant, timeout, timeout_at};

use super::diagnostics::{ActualProtocol, Attempt, AttemptScope, ConnectionUse, Reason, Stage};
use super::transport::{InFlight, Usage, doh_answer, doh_chunk, doh_head};
use super::{Endpoint, not_sent};

type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type Stream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

pub(super) struct Client {
    tls: rustls::ClientConfig,
    state: Mutex<State>,
    connecting: tokio::sync::Mutex<()>,
}

const COOLDOWN_FIRST: Duration = Duration::from_secs(2);
const COOLDOWN_MAX: Duration = Duration::from_secs(30);

#[derive(Default)]
struct State {
    connection: Option<Arc<Connection>>,
    retry_after: Option<Instant>,
    failures: u32,
    refused: Option<u16>,
}

enum ConnectionOutcome {
    Answered,
    Failed,
    StreamFailed,
}

impl State {
    fn connection_failed(&mut self, now: Instant) {
        if self.retry_after.is_some_and(|until| until > now) {
            return;
        }
        let cooldown = (COOLDOWN_FIRST * 2u32.pow(self.failures.min(4))).min(COOLDOWN_MAX);
        self.failures = self.failures.saturating_add(1);
        self.retry_after = Some(now + cooldown);
    }

    fn answered(&mut self) {
        self.failures = 0;
        self.retry_after = None;
    }

    fn finish(&mut self, connection: Option<&Arc<Connection>>, outcome: ConnectionOutcome) {
        let current = match (connection, &self.connection) {
            (Some(used), Some(current)) => Arc::ptr_eq(used, current),
            (None, None) => true,
            _ => false,
        };
        if current {
            match outcome {
                ConnectionOutcome::Answered => self.answered(),
                ConnectionOutcome::Failed => {
                    self.connection = None;
                    self.connection_failed(Instant::now());
                }
                ConnectionOutcome::StreamFailed => {}
            }
        }
    }
}

fn definite_refusal(status: http::StatusCode) -> bool {
    status.is_client_error() && !matches!(status.as_u16(), 408 | 414 | 421 | 425 | 429 | 431)
}

struct EndpointGuard(quinn::Endpoint);
impl Drop for EndpointGuard {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"upstream retired");
    }
}

struct Connection {
    _endpoint: EndpointGuard,
    quic: quinn::Connection,
    sender: Sender,
    usage: Arc<Usage>,
    driver: tokio::task::JoinHandle<()>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

struct RequestGuard(Stream, bool);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.1 {
            self.0.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
        }
    }
}

impl Client {
    pub fn new(mut tls: rustls::ClientConfig) -> Self {
        tls.alpn_protocols = vec![b"h3".to_vec()];
        Self {
            tls,
            state: Mutex::new(State::default()),
            connecting: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn exchange(
        &self,
        spec: &Endpoint,
        query: &Message,
        address: SocketAddr,
        deadline: Instant,
        scope: &AttemptScope,
    ) -> Result<Message> {
        let mut attempt = scope.start(Some(ActualProtocol::Doh3));
        let refused = self.state.lock().unwrap_or_else(|e| e.into_inner()).refused;
        if let Some(status) = refused {
            attempt.http_status(status);
            attempt.skipped();
            return Err(not_sent(anyhow::anyhow!(
                "HTTP/3 refused with status {status}"
            )));
        }
        if self.cooling_down() {
            attempt.skipped();
            return Err(not_sent(anyhow::anyhow!("HTTP/3 cooling down")));
        }
        let budget =
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()) / 2);
        if budget.is_zero() {
            attempt.reason(Reason::Deadline);
            attempt.skipped();
            return Err(not_sent(anyhow::anyhow!("HTTP/3 deadline exhausted")));
        }
        let mut connection = None;
        let mut attempted = false;
        let mut sent = false;
        let result = timeout_at(deadline, async {
            let (request, outbound) = super::transport::doh_request(spec, query)?;
            let (parts, body) = request.into_parts();
            let active = match timeout(
                budget,
                self.connection(spec, address, &mut attempt, &mut attempted),
            )
            .await
            {
                Ok(result) => result?,
                Err(error) => {
                    attempt.reason(Reason::Deadline);
                    return Err(error.into());
                }
            };
            connection = Some(active.clone());
            let _in_flight = InFlight::new(&active.usage);
            let mut sender = active.sender.clone();
            attempt.stage(Stage::RequestWrite);
            attempted = true;
            let stream = sender
                .send_request(http::Request::from_parts(parts, ()))
                .await?;
            sent = true;
            self.finish_request(
                RequestGuard(stream, false),
                body,
                &outbound,
                query,
                &mut attempt,
            )
            .await
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                attempt.reason(Reason::Deadline);
                Err(error.into())
            }
        };
        attempt.finish(&result);
        if attempted {
            let outcome = match &result {
                Ok(_) => ConnectionOutcome::Answered,
                Err(error) => {
                    let stream = error.downcast_ref::<h3::error::StreamError>();
                    let failed = matches!(
                        stream,
                        Some(
                            h3::error::StreamError::ConnectionError { .. }
                                | h3::error::StreamError::RemoteClosing { .. }
                        )
                    ) || connection.as_ref().is_none_or(|connection| {
                        connection.quic.close_reason().is_some() || connection.driver.is_finished()
                    }) || (!sent && stream.is_none());
                    if failed {
                        ConnectionOutcome::Failed
                    } else {
                        ConnectionOutcome::StreamFailed
                    }
                }
            };
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish(connection.as_ref(), outcome);
        }
        result.map_err(|error| if sent { error } else { not_sent(error) })
    }

    fn cooling_down(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retry_after
            .is_some_and(|until| until > Instant::now())
    }

    fn cached(&self, address: SocketAddr) -> Option<Arc<Connection>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .connection
            .as_ref()
            .is_some_and(|c| c.quic.close_reason().is_some() || c.driver.is_finished())
        {
            state.connection = None;
        }
        state
            .connection
            .as_ref()
            .filter(|c| c.quic.remote_address() == address)
            .cloned()
    }

    async fn connection(
        &self,
        spec: &Endpoint,
        address: SocketAddr,
        attempt: &mut Attempt,
        attempted: &mut bool,
    ) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(address) {
            attempt.connection(ConnectionUse::Reused);
            return Ok(connection);
        }
        attempt.stage(Stage::Wait);
        let _connecting = self.connecting.lock().await;
        if self.cooling_down() {
            attempt.skipped();
            anyhow::bail!("HTTP/3 cooling down");
        }
        if let Some(connection) = self.cached(address) {
            attempt.connection(ConnectionUse::Reused);
            return Ok(connection);
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connection = None;
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        *attempted = true;
        attempt.connection(ConnectionUse::New);
        attempt.stage(Stage::Connect);
        let mut endpoint = EndpointGuard(quinn::Endpoint::client(bind.parse()?)?);
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(self.tls.clone())?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(0u32.into())
            .max_concurrent_uni_streams(3u32.into())
            .stream_receive_window(131072u32.into())
            .receive_window((1024u32 * 1024).into())
            .keep_alive_interval(Some(Duration::from_secs(1)))
            .max_idle_timeout(Some(Duration::from_secs(3).try_into()?));
        config.transport_config(Arc::new(transport));
        endpoint.0.set_default_client_config(config);
        attempt.stage(Stage::QuicHandshake);
        let quic = endpoint.0.connect(address, &spec.host)?.await?;
        attempt.stage(Stage::RequestWrite);
        let (mut driver, sender) = h3::client::builder()
            .max_field_section_size(16 * 1024)
            .build(h3_quinn::Connection::new(quic.clone()))
            .await?;
        let usage = Arc::new(Usage::default());
        let idle = super::transport::idle(usage.clone(), Duration::from_secs(10));
        let closing = quic.clone();
        let driver = tokio::spawn(async move {
            tokio::select! {
                _ = futures_util::future::poll_fn(|cx| driver.poll_close(cx)) => {}
                () = idle => closing.close(0u32.into(), b"idle"),
            }
        });
        let connection = Arc::new(Connection {
            _endpoint: endpoint,
            quic,
            sender,
            usage,
            driver,
        });
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connection = Some(connection.clone());
        Ok(connection)
    }

    async fn finish_request(
        &self,
        mut stream: RequestGuard,
        body: Bytes,
        outbound: &Message,
        query: &Message,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        if !body.is_empty() {
            stream.0.send_data(body).await?;
        }
        stream.0.finish().await?;
        attempt.stage(Stage::ResponseHeaders);
        let response = stream.0.recv_response().await?;
        if definite_refusal(response.status()) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).refused =
                Some(response.status().as_u16());
        }
        let age = doh_head(response.status(), response.headers(), attempt)?;
        let mut bytes = Vec::new();
        while let Some(mut chunk) = stream.0.recv_data().await? {
            doh_chunk(&mut bytes, &chunk.copy_to_bytes(chunk.remaining()), attempt)?;
        }
        let response = doh_answer(&bytes, outbound, query, age, attempt)?;
        stream.1 = true;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::{Diagnostics, Operation, Outcome};
    use super::*;

    #[tokio::test]
    async fn exhausted_setup_budget_is_a_skipped_deadline() {
        let client = Client::new(crate::tls::client_config(None, b"h3").unwrap());
        let spec = Endpoint::parse("https://127.0.0.1:443").unwrap();
        let address = "127.0.0.1:443".parse().unwrap();
        let deadline = Instant::now();
        let operation = Operation::new(true, None);
        let scope = AttemptScope::new(&Diagnostics::new(1), &operation, 0, deadline);
        assert!(
            client
                .exchange(&spec, &Message::query(), address, deadline, &scope)
                .await
                .unwrap_err()
                .is::<super::super::NotSent>()
        );
        let trace = operation.trace().unwrap();
        assert_eq!(trace.attempts.len(), 1);
        assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline));
        assert_eq!(trace.attempts[0].outcome, Outcome::Skipped);
    }

    #[tokio::test]
    async fn failed_connection_is_retired_and_its_late_answer_keeps_cooldown() {
        use hickory_proto::{
            op::Query,
            rr::{Name, RecordType},
        };
        use rustls::pki_types::PrivatePkcs8KeyDer;
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = identity.cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into(),
            )
            .unwrap();
        server.alpn_protocols = vec![b"h3".to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server).unwrap();
        let endpoint = quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let address = endpoint.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let quic = endpoint.accept().await.unwrap().await.unwrap();
            let mut server = h3::server::builder()
                .build::<_, Bytes>(h3_quinn::Connection::new(quic.clone()))
                .await
                .unwrap();
            let _request = server
                .accept()
                .await
                .unwrap()
                .unwrap()
                .resolve_request()
                .await
                .unwrap();
            quic.close(1u32.into(), b"test");
            endpoint.wait_idle().await;
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client = Client::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let spec = Endpoint::parse(&format!("https://localhost:{}", address.port())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let operation = Operation::new(true, None);
        let scope = AttemptScope::new(&Diagnostics::new(1), &operation, 0, deadline);
        let old = timeout_at(
            deadline,
            client.connection(
                &spec,
                address,
                &mut scope.start(Some(ActualProtocol::Doh3)),
                &mut false,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let mut query = Message::query();
        query.add_query(Query::query(
            Name::from_ascii("retire.test.").unwrap(),
            RecordType::A,
        ));
        let error = client
            .exchange(&spec, &query, address, deadline, &scope)
            .await
            .unwrap_err();
        assert!(!error.is::<super::super::NotSent>());
        {
            let mut state = client.state.lock().unwrap();
            assert!(state.connection.is_none());
            let until = state.retry_after.unwrap();
            state.finish(Some(&old), ConnectionOutcome::Answered);
            assert_eq!(state.retry_after, Some(until));
        }
        drop(old);
        timeout_at(deadline, server).await.unwrap().unwrap();
    }

    #[test]
    fn connection_failures_back_off_to_30_seconds_and_reset_after_an_answer() {
        let mut state = State::default();
        let mut now = Instant::now();
        let mut cooldowns = Vec::new();
        for _ in 0..6 {
            state.connection_failed(now);
            let until = state.retry_after.unwrap();
            state.connection_failed(until - Duration::from_millis(1));
            assert_eq!(state.retry_after, Some(until));
            cooldowns.push((until - now).as_secs());
            now = until;
        }
        assert_eq!(cooldowns, [2, 4, 8, 16, 30, 30]);
        state.answered();
        assert_eq!(state.retry_after, None);
        state.connection_failed(now);
        assert_eq!(state.retry_after, Some(now + COOLDOWN_FIRST));
    }
}
