//! Upstream protocol exchange for UDP/TCP, DoT, DoH (HTTP/2) and DoQ endpoints.
//! Resolver owns the total deadline; hostname endpoints use only explicit bootstrap.
//!
//! Each HTTPS endpoint keeps one reusable H2 connection, opened serially, with a
//! local stream cap under the peer's limit, a PING while busy or just used that
//! retires a silently dead path, and a 10-second idle close owned by its driver.
//! GOAWAY or I/O errors retire it; only a request not yet sent may move to a new
//! connection, so a sent query is never replayed. Attempts record new or reused
//! connections.
//!
//! A sent query is never resent. A hostname endpoint tries its bootstrap addresses
//! in order, but moves to the next address only after a failure marked
//! [`NotSent`] (connect, TLS or QUIC handshake, stream open, or a request never
//! handed to the transport); any later failure is this endpoint's answer. With
//! HTTP/3 preferred, only a not-sent H3 attempt falls back to H2 (see `h3`).
//!
//! DoH answers have HTTP Age normalized into RR TTLs here, before Resolver sees
//! them; cache code has no HTTP policy. EDNS TCP keepalive in an answer belongs to
//! the upstream connection and is removed here (DoQ rejects it, RFC 9250). DoQ
//! in-flight requests retain their endpoint owner independently of the current
//! bootstrap-address cache entry.
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query},
    rr::{Name, RData, RecordType},
};
use rustls::{ClientConfig, pki_types::ServerName};
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, timeout},
};
use tokio_rustls::TlsConnector;

use super::diagnostics::{ActualProtocol, Attempt, AttemptScope, ConnectionUse, Stage};
use super::{Endpoint, NotSent, Protocol, Settings, not_sent, valid_address};
use crate::{protocol, transport::tcp};

// RFC 9111 sections 5.1 and 1.2.2: use the first Age member, ignore
// malformed values, and saturate a valid integer that exceeds our TTL range.
fn http_age(headers: &http::HeaderMap) -> u32 {
    let Some(value) = headers.get(http::header::AGE).and_then(|v| v.to_str().ok()) else {
        return 0;
    };
    let value = value
        .split(',')
        .next()
        .unwrap_or("")
        .trim_matches([' ', '\t']);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    value.bytes().fold(0u32, |age, digit| {
        age.saturating_mul(10)
            .saturating_add(u32::from(digit - b'0'))
    })
}

// RFC 8484 section 5.1 applies to downstream answers as well as local DNS
// caching. Change RR TTLs only, not EDNS metadata or SOA RDATA parameters.
fn apply_http_age(response: &mut Message, age: u32) {
    for record in response
        .answers
        .iter_mut()
        .chain(&mut response.authorities)
        .chain(&mut response.additionals)
    {
        record.ttl = record.ttl.saturating_sub(age);
    }
}

/// Validate an RFC 8484 response head shared by HTTP/2 and HTTP/3. Returns
/// the HTTP Age that `doh_answer` applies once the body is accepted.
pub(super) fn doh_head(
    status: http::StatusCode,
    headers: &http::HeaderMap,
    attempt: &mut Attempt,
) -> Result<u32> {
    if status != http::StatusCode::OK {
        attempt.http_status(status.as_u16());
        anyhow::bail!("DoH HTTP status {status}");
    }
    attempt.stage(Stage::Validate);
    ensure!(
        headers
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
    let age = http_age(headers);
    attempt.stage(Stage::ResponseRead);
    Ok(age)
}

/// Append one body chunk within the DNS message size limit.
pub(super) fn doh_chunk(bytes: &mut Vec<u8>, chunk: &[u8], attempt: &mut Attempt) -> Result<()> {
    if bytes.len() + chunk.len() > protocol::MAX_MESSAGE {
        attempt.reason(super::diagnostics::Reason::ProtocolInvalid);
        anyhow::bail!("DoH response too large");
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

/// Decode and validate a complete DoH body against the sent `outbound`
/// message, then normalize Age into TTLs and restore the client's ID.
pub(super) fn doh_answer(
    bytes: &[u8],
    outbound: &Message,
    query: &Message,
    age: u32,
    attempt: &mut Attempt,
) -> Result<Message> {
    attempt.stage(Stage::Decode);
    let mut response = protocol::decode(bytes)?;
    attempt.stage(Stage::Validate);
    ensure!(
        protocol::matches_response(outbound, &response) && !response.truncation,
        "invalid DoH response"
    );
    apply_http_age(&mut response, age);
    response.metadata.id = query.id;
    Ok(response)
}

/// RFC 8484 GET, the method every DoH server must support: the query (ID 0)
/// travels base64url-encoded without padding in the `dns` parameter. Production
/// servers answered some POSTs with 405. Returns the request and the message
/// that answers are validated against.
pub(super) fn doh_get(spec: &Endpoint, query: &Message) -> Result<(http::Request<()>, Message)> {
    use base64::Engine;
    let mut outbound = query.clone();
    outbound.metadata.id = 0;
    let host = if spec.host.contains(':') {
        format!("[{}]", spec.host)
    } else {
        spec.host.clone()
    };
    let separator = if spec.path.contains('?') { '&' } else { '?' };
    let request = http::Request::builder()
        .method("GET")
        .uri(format!(
            "https://{host}:{}{}{separator}dns={}",
            spec.port,
            spec.path,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(protocol::encode_upstream(&outbound, true)?)
        ))
        .header("accept", "application/dns-message")
        .body(())?;
    Ok((request, outbound))
}

/// One reusable HTTP/2 connection per configured HTTPS endpoint (like H3).
/// The driver task owns the socket; dropping the last handle aborts it.
struct H2Connection {
    address: SocketAddr,
    sender: h2::client::SendRequest<Bytes>,
    /// Local stream cap. h2 itself queues streams above the peer's SETTINGS
    /// limit; both waits stay inside the caller's deadline.
    streams: Arc<tokio::sync::Semaphore>,
    usage: Arc<Usage>,
    driver: tokio::task::JoinHandle<()>,
}
impl Drop for H2Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

const H2_MAX_STREAMS: usize = 64;
const H2_IDLE: Duration = Duration::from_secs(10);
/// While requests are in flight, a PING proves the path still works. A path
/// that dies silently (NAT or middlebox state loss) is retired within about two
/// seconds instead of stalling every query until TCP gives up.
const H2_PING_INTERVAL: Duration = Duration::from_secs(1);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(1);

/// In-flight requests and last use of one reusable HTTP/2 or HTTP/3 connection.
#[derive(Default)]
pub(super) struct Usage {
    active: std::sync::atomic::AtomicUsize,
    last_used: std::sync::Mutex<Option<Instant>>,
}

/// Counts one in-flight request; the idle clock starts when the last one ends.
pub(super) struct InFlight(Arc<Usage>);
impl InFlight {
    pub(super) fn new(usage: &Arc<Usage>) -> Self {
        usage
            .active
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(usage.clone())
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        self.0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Completes once no request has used the connection for `after`.
pub(super) async fn idle(usage: Arc<Usage>, after: Duration) {
    loop {
        let last = *usage.last_used.lock().unwrap_or_else(|e| e.into_inner());
        let busy = usage.active.load(std::sync::atomic::Ordering::Acquire) > 0;
        let wait = last.map_or(after, |last| {
            (last + after).saturating_duration_since(Instant::now())
        });
        if !busy && wait.is_zero() {
            return;
        }
        tokio::time::sleep(if wait.is_zero() { after } else { wait }).await;
    }
}

pub(super) struct Client {
    pub spec: Endpoint,
    bootstrap: Vec<SocketAddr>,
    addresses: Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    /// HTTPS and QUIC endpoints only; DoT owns its connector in `dot`.
    tls: Option<Arc<ClientConfig>>,
    dot: Option<crate::tls::Upstream>,
    listeners: Vec<SocketAddr>,
    quic: Mutex<Option<Arc<QuicConnection>>>,
    h2: std::sync::Mutex<Option<Arc<H2Connection>>>,
    h2_connecting: Mutex<()>,
    h3: Option<super::h3::Client>,
    /// The last address that answered (not HTTPS), for management diagnostics.
    answered_by: std::sync::Mutex<Option<SocketAddr>>,
}

struct QuicConnection {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
}
impl Drop for QuicConnection {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"upstream retired");
    }
}

impl Client {
    pub fn new(spec: Endpoint, settings: &Settings, listeners: Vec<SocketAddr>) -> Result<Self> {
        let alpn: Option<&[u8]> = match spec.protocol {
            Protocol::Https => Some(b"h2"),
            Protocol::Quic => Some(b"doq"),
            Protocol::Udp | Protocol::Tcp | Protocol::Tls => None,
        };
        let tls = alpn
            .map(|alpn| crate::tls::client_config(settings.ca_file.as_deref(), alpn))
            .transpose()?;
        let dot = (spec.protocol == Protocol::Tls)
            .then(|| {
                crate::tls::Upstream::with_pool(
                    &crate::tls::ClientSettings {
                        server_name: spec.host.clone(),
                        ca_file: settings.ca_file.clone(),
                    },
                    &settings.dot_pool,
                )
            })
            .transpose()?;
        let h3 = tls
            .as_ref()
            .filter(|_| settings.prefer_h3 && spec.protocol == Protocol::Https)
            .map(|tls| super::h3::Client::new(tls.clone()));
        Ok(Self {
            spec,
            bootstrap: settings.bootstrap.clone(),
            addresses: Mutex::new(None),
            tls: tls.map(Arc::new),
            dot,
            listeners,
            quic: Mutex::new(None),
            h2: std::sync::Mutex::new(None),
            h2_connecting: Mutex::new(()),
            h3,
            answered_by: std::sync::Mutex::new(None),
        })
    }

    /// Where this endpoint is reached, for authenticated diagnostics only: the URL
    /// host for HTTPS, otherwise the address that last answered (or the
    /// configured host and port before any answer).
    pub fn display_address(&self) -> String {
        if self.spec.protocol == Protocol::Https {
            return self.spec.host.clone();
        }
        match *self.answered_by.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(address) => address.to_string(),
            None => match self.spec.host.parse::<std::net::IpAddr>() {
                Ok(ip) => SocketAddr::new(ip, self.spec.port).to_string(),
                Err(_) => format!("{}:{}", self.spec.host, self.spec.port),
            },
        }
    }

    async fn addresses(&self, deadline: Instant, attempt: &mut Attempt) -> Result<Vec<SocketAddr>> {
        attempt.stage(Stage::Wait);
        let mut cached = self.addresses.lock().await;
        if let Some((_, addresses)) = cached
            .as_ref()
            .filter(|(expiry, _)| *expiry > Instant::now())
        {
            return Ok(addresses.clone());
        }
        // Explicit bootstrap, never the host's system resolver. The caller's
        // remaining overall deadline encloses lookup and every transport step.
        attempt.stage(Stage::Bootstrap);
        let name = Name::from_ascii(format!("{}.", self.spec.host))?;
        let mut addresses = Vec::new();
        let mut ttl = 3600;
        let mut missing_reason = super::diagnostics::Reason::ProtocolInvalid;
        for server in &self.bootstrap {
            for qtype in [RecordType::A, RecordType::AAAA] {
                ensure!(Instant::now() < deadline, "bootstrap deadline exhausted");
                let mut query = Message::new(0, MessageType::Query, OpCode::Query);
                query.metadata.recursion_desired = true;
                query.add_query(Query::query(name.clone(), qtype));
                let response = match timeout(
                    Duration::from_millis(500)
                        .min(deadline.saturating_duration_since(Instant::now())),
                    crate::upstream::exchange(&query, *server),
                )
                .await
                {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        missing_reason = if error.downcast_ref::<std::io::Error>().is_some() {
                            super::diagnostics::Reason::ConnectIo
                        } else {
                            super::diagnostics::Reason::ProtocolInvalid
                        };
                        continue;
                    }
                    Err(_) => {
                        missing_reason = super::diagnostics::Reason::Deadline;
                        continue;
                    }
                };
                // Accept only the queried owner or a proven in-message CNAME chain.
                let mut owner = name.clone();
                for _ in 0..8 {
                    let alias = response.answers.iter().find_map(|rr| match &rr.data {
                        RData::CNAME(target) if rr.name == owner => {
                            Some((target.0.clone(), rr.ttl))
                        }
                        _ => None,
                    });
                    let Some((target, record_ttl)) = alias else {
                        break;
                    };
                    ttl = ttl.min(record_ttl);
                    owner = target;
                }
                for rr in &response.answers {
                    if addresses.len() >= 16 {
                        break;
                    }
                    if rr.name != owner {
                        continue;
                    }
                    let ip = match &rr.data {
                        RData::A(ip) if qtype == RecordType::A => IpAddr::V4(ip.0),
                        RData::AAAA(ip) if qtype == RecordType::AAAA => IpAddr::V6(ip.0),
                        _ => continue,
                    };
                    let address = SocketAddr::new(ip, self.spec.port);
                    valid_address(address)
                        .and_then(|()| super::not_self(address, &self.listeners))
                        .inspect_err(|_| {
                            attempt.reason(super::diagnostics::Reason::ProtocolInvalid)
                        })?;
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                    ttl = ttl.min(rr.ttl);
                    if addresses.len() >= 16 {
                        break;
                    }
                }
            }
            if !addresses.is_empty() {
                break;
            }
        }
        if addresses.is_empty() {
            attempt.reason(missing_reason);
        }
        ensure!(
            !addresses.is_empty(),
            "bootstrap returned no upstream addresses"
        );
        *cached = Some((
            Instant::now() + Duration::from_secs(u64::from(ttl)),
            addresses.clone(),
        ));
        Ok(addresses)
    }

    pub async fn exchange(
        &self,
        query: &Message,
        deadline: Instant,
        scope: &AttemptScope,
    ) -> Result<super::Exchange> {
        let mut error = anyhow::anyhow!("no upstream address");
        let addresses = match self.spec.host.parse::<IpAddr>() {
            Ok(address) => vec![SocketAddr::new(address, self.spec.port)],
            Err(_) => {
                let mut attempt = scope.start(None);
                let result = self.addresses(deadline, &mut attempt).await;
                attempt.finish(&result);
                result?
            }
        };
        for address in addresses {
            ensure!(scope.remaining(), "upstream deadline exhausted");
            let response = match self.spec.protocol {
                Protocol::Udp => {
                    crate::upstream::exchange_observed(query, address, Some(scope)).await
                }
                Protocol::Tcp => {
                    crate::upstream::exchange_tcp_observed(query, address, Some(scope)).await
                }
                Protocol::Tls => {
                    self.dot
                        .as_ref()
                        .expect("DoT client")
                        .exchange_observed(query, address, scope)
                        .await
                }
                Protocol::Https => {
                    let h3 = match &self.h3 {
                        Some(h3) => Some(
                            h3.exchange(&self.spec, query, address, deadline, scope)
                                .await,
                        ),
                        None => None,
                    };
                    match h3 {
                        Some(Ok(response)) => Ok(response),
                        // A request H3 may have sent is never repeated over H2.
                        Some(Err(error)) if !error.is::<NotSent>() => Err(error),
                        _ => {
                            ensure!(scope.remaining(), "upstream deadline exhausted before H2");
                            let mut attempt = scope.start(Some(ActualProtocol::Doh2));
                            let result = self.https(query, address, &mut attempt).await;
                            attempt.finish(&result);
                            result
                        }
                    }
                }
                Protocol::Quic => {
                    let mut attempt = scope.start(Some(ActualProtocol::Doq));
                    let result = self.quic(query, address, &mut attempt).await;
                    attempt.finish(&result);
                    result
                }
            };
            // HTTPS is shown by its URL host; other protocols record the address
            // that answered.
            if response.is_ok() && self.spec.protocol != Protocol::Https {
                *self.answered_by.lock().unwrap_or_else(|e| e.into_inner()) = Some(address);
            }
            match response {
                Ok(mut message) => {
                    protocol::strip_tcp_keepalive(&mut message);
                    return Ok(super::Exchange {
                        message,
                        upstream: self.spec.label.clone(),
                    });
                }
                // Nothing reached this address: the next one may carry the query.
                Err(failure) if failure.is::<NotSent>() => error = failure,
                Err(failure) => return Err(failure),
            }
        }
        Err(error)
    }

    fn h2_cached(&self, address: SocketAddr) -> Option<Arc<H2Connection>> {
        let mut state = self.h2.lock().unwrap_or_else(|e| e.into_inner());
        if state.as_ref().is_some_and(|c| c.driver.is_finished()) {
            *state = None;
        }
        state.as_ref().filter(|c| c.address == address).cloned()
    }

    fn h2_retire(&self, connection: &Arc<H2Connection>) {
        let mut state = self.h2.lock().unwrap_or_else(|e| e.into_inner());
        if state.as_ref().is_some_and(|c| Arc::ptr_eq(c, connection)) {
            *state = None;
        }
    }

    /// Reuse the endpoint's connection when it serves this address; otherwise
    /// connect once under the endpoint lock so concurrent misses share it.
    async fn h2_connection(
        &self,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<(Arc<H2Connection>, ConnectionUse)> {
        if let Some(connection) = self.h2_cached(address) {
            return Ok((connection, ConnectionUse::Reused));
        }
        attempt.stage(Stage::Wait);
        let _connecting = self.h2_connecting.lock().await;
        if let Some(connection) = self.h2_cached(address) {
            return Ok((connection, ConnectionUse::Reused));
        }
        attempt.stage(Stage::Connect);
        let socket = TcpStream::connect(address).await?;
        attempt.stage(Stage::TlsHandshake);
        let tls = self.tls.clone().expect("HTTPS endpoint TLS configuration");
        let stream = TlsConnector::from(tls)
            .connect(ServerName::try_from(self.spec.host.clone())?, socket)
            .await?;
        attempt.stage(Stage::Validate);
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(b"h2"),
            "DoH upstream must negotiate HTTP/2"
        );
        attempt.stage(Stage::RequestWrite);
        let (sender, mut connection) = h2::client::Builder::new()
            .enable_push(false)
            .max_header_list_size(16 * 1024)
            .handshake(stream)
            .await?;
        let usage = Arc::new(Usage::default());
        let mut ping_pong = connection
            .ping_pong()
            .context("HTTP/2 keepalive unavailable")?;
        let keepalive = {
            let usage = usage.clone();
            async move {
                loop {
                    tokio::time::sleep(H2_PING_INTERVAL).await;
                    // Probe while busy or just after a request ended (a caller may
                    // have given up on a dead path); an idle connection stays quiet.
                    let recent = usage
                        .last_used
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some_and(|last| last.elapsed() < H2_PING_INTERVAL);
                    if usage.active.load(std::sync::atomic::Ordering::Acquire) == 0 && !recent {
                        continue;
                    }
                    let pong = timeout(H2_PING_TIMEOUT, ping_pong.ping(h2::Ping::opaque())).await;
                    if !matches!(pong, Ok(Ok(_))) {
                        return;
                    }
                }
            }
        };
        let idle = idle(usage.clone(), H2_IDLE);
        // GOAWAY, a connection error, a missed PING or idleness ends the driver;
        // dropping the connection closes its socket without delaying shutdown.
        let driver = tokio::spawn(async move {
            tokio::select! { _ = connection => {}, () = idle => {}, () = keepalive => {} }
        });
        let connection = Arc::new(H2Connection {
            address,
            sender,
            streams: Arc::new(tokio::sync::Semaphore::new(H2_MAX_STREAMS)),
            usage,
            driver,
        });
        *self.h2.lock().unwrap_or_else(|e| e.into_inner()) = Some(connection.clone());
        Ok((connection, ConnectionUse::New))
    }

    async fn https(
        &self,
        query: &Message,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        let (connection, used) = self
            .h2_connection(address, attempt)
            .await
            .map_err(not_sent)?;
        attempt.connection(used);
        match self.h2_request(&connection, query, attempt).await {
            // A reused connection that closed before taking the request (idle
            // close, GOAWAY): nothing was sent, so carry it on a new connection.
            Err(error) if used == ConnectionUse::Reused && error.is::<NotSent>() => {
                self.h2_retire(&connection);
                drop(connection);
                let (connection, used) = self
                    .h2_connection(address, attempt)
                    .await
                    .map_err(not_sent)?;
                attempt.connection(used);
                self.h2_request(&connection, query, attempt).await
            }
            Err(error) => {
                // A stream reset affects one request; GOAWAY or an I/O error ends
                // the connection for new requests (accepted streams finish per h2).
                let connection_failed = error.is::<NotSent>()
                    || error
                        .downcast_ref::<h2::Error>()
                        .is_some_and(|e| e.is_go_away() || e.is_io());
                if connection_failed {
                    self.h2_retire(&connection);
                }
                Err(error)
            }
            result => result,
        }
    }

    async fn h2_request(
        &self,
        connection: &Arc<H2Connection>,
        query: &Message,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        attempt.stage(Stage::Wait);
        // Both waits count against the caller's deadline, which encloses this call.
        let _permit = connection.streams.acquire().await?;
        let _stream = InFlight::new(&connection.usage);
        let mut sender = connection.sender.clone().ready().await.map_err(not_sent)?;
        let (request, outbound) = doh_get(&self.spec, query)?;
        attempt.stage(Stage::RequestWrite);
        // Dropping this on cancellation resets only this stream (RST_STREAM).
        let (response, _) = sender.send_request(request, true).map_err(not_sent)?;
        attempt.stage(Stage::ResponseHeaders);
        let response = response.await?;
        let age = doh_head(response.status(), response.headers(), attempt)?;
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk?;
            doh_chunk(&mut bytes, &chunk, attempt)?;
            body.flow_control().release_capacity(chunk.len())?;
        }
        doh_answer(&bytes, &outbound, query, age, attempt)
    }

    async fn quic_connection(
        &self,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<Arc<QuicConnection>> {
        attempt.stage(Stage::Wait);
        let mut cached = self.quic.lock().await;
        if let Some(current) = cached.as_ref().filter(|c| {
            c.connection.remote_address() == address && c.connection.close_reason().is_none()
        }) {
            return Ok(current.clone());
        }
        *cached = None;
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        attempt.stage(Stage::Connect);
        let mut endpoint = quinn::Endpoint::client(bind.parse()?)?;
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(
            (**self.tls.as_ref().expect("QUIC endpoint TLS configuration")).clone(),
        )?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(0u32.into())
            .max_concurrent_uni_streams(0u32.into())
            .stream_receive_window(131072u32.into())
            .receive_window(131072u32.into())
            .max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
        config.transport_config(Arc::new(transport));
        endpoint.set_default_client_config(config);
        attempt.stage(Stage::QuicHandshake);
        let connection = endpoint.connect(address, &self.spec.host)?.await?;
        let connection = Arc::new(QuicConnection {
            endpoint,
            connection,
        });
        *cached = Some(connection.clone());
        Ok(connection)
    }

    async fn quic(
        &self,
        query: &Message,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        let connection = self
            .quic_connection(address, attempt)
            .await
            .map_err(not_sent)?;
        // Retain the endpoint owner for this entire request. Bootstrap rotation
        // may replace the cached owner while this stream is still in flight.
        attempt.stage(Stage::Wait);
        let (mut send, mut recv) = connection.connection.open_bi().await.map_err(not_sent)?;
        let mut outbound = query.clone();
        outbound.metadata.id = 0;
        attempt.stage(Stage::RequestWrite);
        tcp::write_frame(&mut send, &protocol::encode_upstream(&outbound, true)?).await?;
        send.finish()?;
        attempt.stage(Stage::ResponseRead);
        let frame = recv.read_to_end(protocol::MAX_MESSAGE + 2).await?;
        attempt.stage(Stage::Validate);
        ensure!(
            frame.len() >= 14
                && usize::from(u16::from_be_bytes([frame[0], frame[1]])) == frame.len() - 2,
            "invalid DoQ response length"
        );
        attempt.stage(Stage::Decode);
        let mut response = protocol::decode(&frame[2..])?;
        attempt.stage(Stage::Validate);
        ensure!(
            protocol::matches_response(&outbound, &response) && !response.truncation,
            "invalid DoQ response"
        );
        ensure!(
            response.edns.as_ref().is_none_or(|e| e
                .option(hickory_proto::rr::rdata::opt::EdnsCode::from(11))
                .is_none()),
            "DoQ TCP keepalive prohibited"
        );
        response.metadata.id = query.id;
        Ok(response)
    }
}
