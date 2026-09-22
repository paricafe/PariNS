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
use crate::protocol;

type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type Stream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

pub(super) struct Client {
    tls: rustls::ClientConfig,
    budget: Duration,
    state: Mutex<State>,
    connecting: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    connection: Option<Arc<Connection>>,
    retry_after: Option<Instant>,
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
    pub fn new(mut tls: rustls::ClientConfig, query_timeout: Duration) -> Self {
        tls.alpn_protocols = vec![b"h3".to_vec()];
        Self {
            tls,
            budget: Duration::from_millis(250).min(query_timeout / 2),
            state: Mutex::new(State::default()),
            connecting: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn exchange(
        &self,
        spec: &Endpoint,
        query: &Message,
        address: SocketAddr,
    ) -> Result<Message> {
        let result = timeout(self.budget, self.request(spec, query, address))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
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

    fn cached(&self, address: SocketAddr) -> Result<Option<Arc<Connection>>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state
                .retry_after
                .is_none_or(|until| until <= Instant::now()),
            "HTTP/3 cooling down"
        );
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

    async fn connection(&self, spec: &Endpoint, address: SocketAddr) -> Result<Arc<Connection>> {
        if let Some(connection) = self.cached(address)? {
            return Ok(connection);
        }
        let _connecting = self.connecting.lock().await;
        if let Some(connection) = self.cached(address)? {
            return Ok(connection);
        }
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let mut endpoint = EndpointGuard(quinn::Endpoint::client(bind.parse()?)?);
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(self.tls.clone())?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(0u32.into())
            .max_concurrent_uni_streams(3u32.into())
            .stream_receive_window(131072u32.into())
            .receive_window((1024u32 * 1024).into())
            .max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
        config.transport_config(Arc::new(transport));
        endpoint.0.set_default_client_config(config);
        let quic = endpoint.0.connect(address, &spec.host)?.await?;
        let (mut driver, sender) = h3::client::builder()
            .max_field_section_size(16 * 1024)
            .build(h3_quinn::Connection::new(quic.clone()))
            .await?;
        let driver = tokio::spawn(async move {
            let _ = futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await;
        });
        let connection = Arc::new(Connection {
            _endpoint: endpoint,
            quic,
            sender,
            driver,
        });
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connection = Some(connection.clone());
        Ok(connection)
    }

    async fn request(
        &self,
        spec: &Endpoint,
        query: &Message,
        address: SocketAddr,
    ) -> Result<Message> {
        let connection = self.connection(spec, address).await?;
        let mut sender = connection.sender.clone();
        let host = if spec.host.contains(':') {
            format!("[{}]", spec.host)
        } else {
            spec.host.clone()
        };
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("https://{host}:{}{}", spec.port, spec.path))
            .header("content-type", "application/dns-message")
            .header("accept", "application/dns-message")
            .body(())?;
        let mut outbound = query.clone();
        outbound.metadata.id = 0;
        let mut stream = RequestGuard(sender.send_request(request).await?, false);
        stream.0.send_data(Bytes::from(outbound.to_vec()?)).await?;
        stream.0.finish().await?;
        let response = stream.0.recv_response().await?;
        ensure!(
            response.status() == http::StatusCode::OK,
            "DoH HTTP status {}",
            response.status()
        );
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
        let mut bytes = Vec::new();
        while let Some(mut chunk) = stream.0.recv_data().await? {
            ensure!(
                bytes.len() + chunk.remaining() <= protocol::MAX_MESSAGE,
                "DoH response too large"
            );
            bytes.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
        }
        let mut response = protocol::decode(&bytes)?;
        ensure!(
            protocol::matches_response(&outbound, &response) && !response.truncation,
            "invalid DoH response"
        );
        response.metadata.id = query.id;
        stream.1 = true;
        Ok(response)
    }
}
