//! Optional HTTP/3 preference for an authenticated HTTPS endpoint.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use bytes::{Buf, Bytes};
use hickory_proto::op::Message;
use tokio::time::{Instant, timeout};

use super::diagnostics::{ActualProtocol, Attempt, AttemptScope, Reason, Stage};
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
            return Err(not_sent(anyhow::anyhow!("HTTP/3 deadline exhausted")));
        }
        let mut connection = None;
        let mut sent = false;
        let request = timeout(
            budget,
            self.send(
                spec,
                query,
                address,
                &mut attempt,
                &mut connection,
                &mut sent,
            ),
        )
        .await;
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                attempt.reason(Reason::Deadline);
                Err(error.into())
            }
        };
        let result = match request {
            Ok((stream, outbound, in_flight)) => {
                self.receive(stream, in_flight, &outbound, query, &mut attempt)
                    .await
            }
            Err(error) => Err(error),
        };
        attempt.finish(&result);
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let current = match (&connection, &state.connection) {
                (Some(used), Some(current)) => Arc::ptr_eq(used, current),
                (None, None) => true,
                _ => false,
            };
            if current {
                match &result {
                    Ok(_) => state.answered(),
                    Err(error)
                        if sent
                            && !matches!(
                                error.downcast_ref::<h3::error::StreamError>(),
                                Some(
                                    h3::error::StreamError::ConnectionError { .. }
                                        | h3::error::StreamError::RemoteClosing { .. }
                                )
                            )
                            && connection.as_ref().is_some_and(|connection| {
                                connection.quic.close_reason().is_none()
                                    && !connection.driver.is_finished()
                            }) => {}
                    Err(_) => state.connection_failed(Instant::now()),
                }
            }
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

    fn cached(
        &self,
        address: SocketAddr,
        observed: &mut Option<Arc<Connection>>,
    ) -> Option<Arc<Connection>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .connection
            .as_ref()
            .is_some_and(|c| c.quic.close_reason().is_some() || c.driver.is_finished())
        {
            state.connection = None;
        }
        *observed = state.connection.clone();
        observed
            .as_ref()
            .filter(|c| c.quic.remote_address() == address)
            .cloned()
    }

    async fn connection(
        &self,
        spec: &Endpoint,
        address: SocketAddr,
        attempt: &mut Attempt,
        observed: &mut Option<Arc<Connection>>,
    ) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(address, observed) {
            return Ok(connection);
        }
        attempt.stage(Stage::Wait);
        let _connecting = self.connecting.lock().await;
        if self.cooling_down() {
            attempt.skipped();
            anyhow::bail!("HTTP/3 cooling down");
        }
        if let Some(connection) = self.cached(address, observed) {
            return Ok(connection);
        }
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
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
        *observed = Some(connection.clone());
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connection = Some(connection.clone());
        Ok(connection)
    }

    async fn send(
        &self,
        spec: &Endpoint,
        query: &Message,
        address: SocketAddr,
        attempt: &mut Attempt,
        used: &mut Option<Arc<Connection>>,
        sent: &mut bool,
    ) -> Result<(RequestGuard, Message, InFlight)> {
        let connection = self.connection(spec, address, attempt, used).await?;
        let mut sender = connection.sender.clone();
        let in_flight = InFlight::new(&connection.usage);
        let (request, outbound) = super::transport::doh_request(spec, query)?;
        let (parts, body) = request.into_parts();
        attempt.stage(Stage::RequestWrite);
        let mut stream = RequestGuard(
            sender
                .send_request(http::Request::from_parts(parts, ()))
                .await?,
            false,
        );
        *sent = true;
        if !body.is_empty() {
            stream.0.send_data(body).await?;
        }
        stream.0.finish().await?;
        Ok((stream, outbound, in_flight))
    }

    async fn receive(
        &self,
        mut stream: RequestGuard,
        _in_flight: InFlight,
        outbound: &Message,
        query: &Message,
        attempt: &mut Attempt,
    ) -> Result<Message> {
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
    use super::*;

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
