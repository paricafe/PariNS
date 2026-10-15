//! Optional HTTP/3 preference for an authenticated HTTPS endpoint.
//! One multiplexed connection and one bounded cooldown per configured endpoint.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, ensure};
use bytes::{Buf, Bytes};
use hickory_proto::op::Message;
use tokio::time::{Instant, timeout};

use super::Endpoint;
use super::diagnostics::{ActualProtocol, Attempt, AttemptScope, Reason, Stage};
use super::transport::{InFlight, Usage};
use crate::protocol;

type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type Stream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

pub(super) struct Client {
    tls: rustls::ClientConfig,
    state: Mutex<State>,
    connecting: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    connection: Option<Arc<Connection>>,
    retry_after: Option<Instant>,
    /// A definite 4xx (for example 405) will not change on retry: stop trying
    /// HTTP/3 for this endpoint until the pool generation is replaced.
    refused: Option<u16>,
}

/// Client errors that a retry cannot fix. 408, 421, 425 and 429 invite a retry;
/// 414 and 431 concern one oversized query, not the endpoint.
fn definite_refusal(status: http::StatusCode) -> bool {
    status.is_client_error() && !matches!(status.as_u16(), 408 | 414 | 421 | 425 | 429 | 431)
}

// Own the endpoint even during a cancelled handshake, before a driver exists.
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

// A caller deadline or H2 fallback cancels just this request, not other streams.
struct RequestGuard(Stream, bool);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        // h3-quinn 0.0.10 moves its RecvStream into a pending read future:
        // stop_sending() during a cancelled read would panic. Dropping that
        // future drops Quinn's RecvStream, which sends STOP_SENDING itself.
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
            anyhow::bail!("HTTP/3 refused with status {status}");
        }
        if self.cooling_down() {
            attempt.skipped();
            anyhow::bail!("HTTP/3 cooling down");
        }
        // The budget covers only obtaining a usable connection and sending the
        // request, so a dead or slow H3 path still leaves time for H2. Once sent,
        // a slow upstream answer is waited for until the resolver deadline: it
        // is not an H3 fault, causes no cooldown and is not re-sent over H2.
        let budget =
            Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()) / 2);
        ensure!(!budget.is_zero(), "HTTP/3 deadline exhausted");
        let sent = match timeout(budget, self.send(spec, query, address, &mut attempt)).await {
            Ok(sent) => sent,
            Err(error) => {
                attempt.reason(Reason::Deadline);
                Err(error.into())
            }
        };
        let result = match sent {
            Ok((stream, outbound, in_flight)) => {
                self.receive(stream, in_flight, &outbound, query, &mut attempt)
                    .await
            }
            Err(error) => Err(error),
        };
        attempt.finish(&result);
        if result.is_err() {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Skipped requests must not extend the cooldown indefinitely.
            if state
                .retry_after
                .is_none_or(|until| until <= Instant::now())
            {
                state.retry_after = Some(Instant::now() + Duration::from_secs(30));
            }
        }
        result
    }

    fn cooling_down(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retry_after
            .is_some_and(|until| until > Instant::now())
    }

    fn cached(&self, address: SocketAddr) -> Result<Option<Arc<Connection>>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .connection
            .as_ref()
            .is_some_and(|c| c.quic.close_reason().is_some() || c.driver.is_finished())
        {
            state.connection = None;
        }
        Ok(state
            .connection
            .as_ref()
            .filter(|c| c.quic.remote_address() == address)
            .cloned())
    }

    async fn connection(
        &self,
        spec: &Endpoint,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(address)? {
            return Ok(connection);
        }
        attempt.stage(Stage::Wait);
        let _connecting = self.connecting.lock().await;
        if self.cooling_down() {
            attempt.skipped();
            anyhow::bail!("HTTP/3 cooling down");
        }
        if let Some(connection) = self.cached(address)? {
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
            // A sent request waits for its answer without a budget, so a silently
            // dead path must close the connection quickly: PING every second and
            // give up after 3 seconds without any packet from the peer.
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
        // Keep-alive would hold an unused connection open forever; close it after
        // 10 seconds without requests, as for HTTP/2.
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

    async fn send(
        &self,
        spec: &Endpoint,
        query: &Message,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<(RequestGuard, Message, InFlight)> {
        let connection = self.connection(spec, address, attempt).await?;
        let mut sender = connection.sender.clone();
        let in_flight = InFlight::new(&connection.usage);
        let (request, outbound) = super::transport::doh_get(spec, query)?;
        attempt.stage(Stage::RequestWrite);
        let mut stream = RequestGuard(sender.send_request(request).await?, false);
        stream.0.finish().await?;
        Ok((stream, outbound, in_flight))
    }

    async fn receive(
        &self,
        mut stream: RequestGuard,
        // Held until the answer is read: the connection is busy, not idle.
        _in_flight: InFlight,
        outbound: &Message,
        query: &Message,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        attempt.stage(Stage::ResponseHeaders);
        let response = stream.0.recv_response().await?;
        if response.status() != http::StatusCode::OK {
            attempt.http_status(response.status().as_u16());
            if definite_refusal(response.status()) {
                self.state.lock().unwrap_or_else(|e| e.into_inner()).refused =
                    Some(response.status().as_u16());
            }
        }
        ensure!(
            response.status() == http::StatusCode::OK,
            "DoH HTTP status {}",
            response.status()
        );
        attempt.stage(Stage::Validate);
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/dns-message")),
            "invalid DoH content type"
        );
        let age = super::transport::http_age(response.headers());
        let mut bytes = Vec::new();
        attempt.stage(Stage::ResponseRead);
        while let Some(mut chunk) = stream.0.recv_data().await? {
            if bytes.len() + chunk.remaining() > protocol::MAX_MESSAGE {
                attempt.reason(Reason::ProtocolInvalid);
            }
            ensure!(
                bytes.len() + chunk.remaining() <= protocol::MAX_MESSAGE,
                "DoH response too large"
            );
            bytes.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
        }
        attempt.stage(Stage::Decode);
        let mut response = protocol::decode(&bytes)?;
        attempt.stage(Stage::Validate);
        ensure!(
            protocol::matches_response(outbound, &response) && !response.truncation,
            "invalid DoH response"
        );
        super::transport::apply_http_age(&mut response, age);
        response.metadata.id = query.id;
        stream.1 = true;
        Ok(response)
    }
}
