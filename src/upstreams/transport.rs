//! DNS wire exchange and authenticated connections to configured upstreams.
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
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

pub(super) fn doh_chunk(bytes: &mut Vec<u8>, chunk: &[u8], attempt: &mut Attempt) -> Result<()> {
    if bytes.len() + chunk.len() > protocol::MAX_MESSAGE {
        attempt.reason(super::diagnostics::Reason::ProtocolInvalid);
        anyhow::bail!("DoH response too large");
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

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

pub(super) fn doh_request(
    spec: &Endpoint,
    query: &Message,
) -> Result<(http::Request<Bytes>, Message)> {
    use base64::Engine;
    let mut outbound = query.clone();
    outbound.metadata.id = 0;
    let host = if spec.host.contains(':') {
        format!("[{}]", spec.host)
    } else {
        spec.host.clone()
    };
    let endpoint: http::Uri = format!("https://{host}:{}{}", spec.port, spec.path).parse()?;
    let wire = protocol::encode_upstream(&outbound, true)?;
    let separator = if spec.path.contains('?') { '&' } else { '?' };
    let get = format!(
        "{endpoint}{separator}dns={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&wire)
    )
    .parse::<http::Uri>();
    let builder = http::Request::builder().header("accept", "application/dns-message");
    let request = match get {
        Ok(uri) => builder.method("GET").uri(uri).body(Bytes::new())?,
        Err(_) => builder
            .method("POST")
            .uri(endpoint)
            .header("content-type", "application/dns-message")
            .body(Bytes::from(wire))?,
    };
    Ok((request, outbound))
}

struct H2Connection {
    address: SocketAddr,
    sender: h2::client::SendRequest<Bytes>,
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
const H2_PING_INTERVAL: Duration = Duration::from_secs(1);
const H2_PING_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct Usage {
    activity: std::sync::Mutex<Activity>,
}

struct Activity {
    active: usize,
    last_used: Instant,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            activity: std::sync::Mutex::new(Activity {
                active: 0,
                last_used: Instant::now(),
            }),
        }
    }
}

impl Usage {
    fn idle_time(&self) -> Option<Duration> {
        let activity = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        (activity.active == 0).then(|| activity.last_used.elapsed())
    }
}

pub(super) struct InFlight(Arc<Usage>);
impl InFlight {
    pub(super) fn new(usage: &Arc<Usage>) -> Self {
        usage
            .activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active += 1;
        Self(usage.clone())
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        let mut activity = self.0.activity.lock().unwrap_or_else(|e| e.into_inner());
        activity.last_used = Instant::now();
        activity.active -= 1;
    }
}

pub(super) async fn idle(usage: Arc<Usage>, after: Duration) {
    loop {
        let wait = usage
            .idle_time()
            .map_or(after, |idle| after.saturating_sub(idle));
        if wait.is_zero() {
            return;
        }
        tokio::time::sleep(wait).await;
    }
}

pub(super) struct Client {
    pub spec: Endpoint,
    bootstrap: Vec<SocketAddr>,
    addresses: Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    tls: Option<Arc<ClientConfig>>,
    dot: Option<crate::tls::Upstream>,
    listeners: Vec<SocketAddr>,
    quic: Mutex<Option<Arc<QuicConnection>>>,
    h2: std::sync::Mutex<Option<Arc<H2Connection>>>,
    h2_connecting: Mutex<()>,
    h3: Option<super::h3::Client>,
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

struct DoqStreamGuard {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    completed: bool,
}

impl Drop for DoqStreamGuard {
    fn drop(&mut self) {
        if !self.completed {
            let cancelled = quinn::VarInt::from_u32(3);
            let _ = self.send.reset(cancelled);
            let _ = self.recv.stop(cancelled);
        }
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

    async fn addresses(&self, deadline: Instant, scope: &AttemptScope) -> Result<Vec<SocketAddr>> {
        let fresh = |cached: &Option<(Instant, Vec<SocketAddr>)>| {
            cached
                .as_ref()
                .filter(|(expiry, _)| *expiry > Instant::now())
                .map(|(_, addresses)| addresses.clone())
        };
        if let Ok(cached) = self.addresses.try_lock()
            && let Some(addresses) = fresh(&cached)
        {
            return Ok(addresses);
        }
        let mut cached = self.addresses.lock().await;
        if let Some(addresses) = fresh(&cached) {
            return Ok(addresses);
        }
        ensure!(scope.remaining(), "bootstrap deadline exhausted");
        let name = Name::from_ascii(format!("{}.", self.spec.host))?;
        let mut attempt = scope.start(None);
        attempt.stage(Stage::Bootstrap);
        let result = self.bootstrap_addresses(name, deadline, &mut attempt).await;
        attempt.finish(&result);
        let (expiry, addresses) = result?;
        *cached = Some((expiry, addresses.clone()));
        Ok(addresses)
    }

    async fn bootstrap_addresses(
        &self,
        name: Name,
        deadline: Instant,
        attempt: &mut Attempt,
    ) -> Result<(Instant, Vec<SocketAddr>)> {
        let mut addresses = Vec::new();
        let mut missing_reason = super::diagnostics::Reason::ProtocolInvalid;
        for server in &self.bootstrap {
            let mut ttl = 3600;
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
                if response.response_code != ResponseCode::NoError {
                    continue;
                }
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
                return Ok((
                    Instant::now() + Duration::from_secs(u64::from(ttl)),
                    addresses,
                ));
            }
        }
        attempt.reason(missing_reason);
        anyhow::bail!("bootstrap returned no upstream addresses")
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
            Err(_) => self.addresses(deadline, scope).await?,
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
                    let result = self.quic(query, address, deadline, &mut attempt).await;
                    attempt.finish(&result);
                    result
                }
            };
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
                    if usage
                        .idle_time()
                        .is_some_and(|idle| idle >= H2_PING_INTERVAL)
                    {
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
        let mut retried = false;
        loop {
            let (connection, used) = self
                .h2_connection(address, attempt)
                .await
                .map_err(not_sent)?;
            attempt.connection(used);
            let result = self.h2_request(&connection, query, attempt).await;
            let Err(error) = &result else {
                return result;
            };
            let unsent = error.is::<NotSent>();
            if unsent
                || error
                    .downcast_ref::<h2::Error>()
                    .is_some_and(|e| e.is_go_away() || e.is_io())
            {
                self.h2_retire(&connection);
            }
            if retried || used != ConnectionUse::Reused || !unsent {
                return result;
            }
            retried = true;
        }
    }

    async fn h2_request(
        &self,
        connection: &Arc<H2Connection>,
        query: &Message,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        attempt.stage(Stage::Wait);
        let _permit = connection.streams.acquire().await?;
        let _stream = InFlight::new(&connection.usage);
        let mut sender = connection.sender.clone().ready().await.map_err(not_sent)?;
        let (request, outbound) = doh_request(&self.spec, query)?;
        let (parts, body) = request.into_parts();
        attempt.stage(Stage::RequestWrite);
        let (response, mut stream) = sender
            .send_request(http::Request::from_parts(parts, ()), body.is_empty())
            .map_err(not_sent)?;
        if !body.is_empty() {
            stream.send_data(body, true)?;
        }
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
    ) -> Result<(Arc<QuicConnection>, ConnectionUse)> {
        attempt.stage(Stage::Wait);
        let mut cached = self.quic.lock().await;
        if let Some(current) = cached.as_ref().filter(|c| {
            c.connection.remote_address() == address && c.connection.close_reason().is_none()
        }) {
            return Ok((current.clone(), ConnectionUse::Reused));
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
        Ok((connection, ConnectionUse::New))
    }

    async fn quic_retire(&self, connection: &Arc<QuicConnection>) {
        let mut cached = self.quic.lock().await;
        if cached
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            *cached = None;
        }
    }

    async fn quic(
        &self,
        query: &Message,
        address: SocketAddr,
        deadline: Instant,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        let (mut connection, used) = self
            .quic_connection(address, attempt)
            .await
            .map_err(not_sent)?;
        attempt.connection(used);
        attempt.stage(Stage::Wait);
        let (send, recv) = match connection.connection.open_bi().await {
            Err(_) if used == ConnectionUse::Reused && Instant::now() < deadline => {
                self.quic_retire(&connection).await;
                let (replacement, used) = self
                    .quic_connection(address, attempt)
                    .await
                    .map_err(not_sent)?;
                connection = replacement;
                attempt.connection(used);
                attempt.stage(Stage::Wait);
                connection.connection.open_bi().await.map_err(not_sent)?
            }
            result => result.map_err(not_sent)?,
        };
        let mut stream = DoqStreamGuard {
            send,
            recv,
            completed: false,
        };
        let mut outbound = query.clone();
        outbound.metadata.id = 0;
        attempt.stage(Stage::RequestWrite);
        tcp::write_frame(
            &mut stream.send,
            &protocol::encode_upstream(&outbound, true)?,
        )
        .await?;
        stream.send.finish()?;
        attempt.stage(Stage::ResponseRead);
        let frame = stream.recv.read_to_end(protocol::MAX_MESSAGE + 2).await?;
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
        stream.completed = true;
        response.metadata.id = query.id;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn h2_retires_a_replacement_that_also_fails_before_sending() {
        use super::super::diagnostics::{Diagnostics, Operation};

        async fn closed_sender(address: SocketAddr) -> Arc<H2Connection> {
            let (local, remote) = tokio::io::duplex(1024);
            let (sender, connection) = h2::client::handshake(local).await.unwrap();
            drop(remote);
            let _ = connection.await;
            assert!(sender.clone().ready().await.is_err());
            Arc::new(H2Connection {
                address,
                sender,
                streams: Arc::new(tokio::sync::Semaphore::new(1)),
                usage: Arc::default(),
                driver: tokio::spawn(std::future::pending()),
            })
        }

        let address = "127.0.0.1:443".parse().unwrap();
        let client = Client::new(
            Endpoint::parse("https://127.0.0.1:443").unwrap(),
            &Settings::default(),
            vec![],
        )
        .unwrap();
        let first = closed_sender(address).await;
        let replacement = closed_sender(address).await;
        *client.h2.lock().unwrap() = Some(first);
        let connecting = client.h2_connecting.lock().await;
        let deadline = Instant::now() + Duration::from_secs(1);
        let scope = AttemptScope::new(
            &Diagnostics::new(1),
            &Operation::new(false, None),
            0,
            deadline,
        );
        let mut attempt = scope.start(Some(ActualProtocol::Doh2));
        let query = Message::query();
        let request = client.https(&query, address, &mut attempt);
        tokio::pin!(request);
        assert!(futures_util::poll!(&mut request).is_pending());
        assert!(client.h2.lock().unwrap().is_none());
        *client.h2.lock().unwrap() = Some(replacement);
        drop(connecting);
        let error = tokio::time::timeout_at(deadline, request)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is::<NotSent>());
        assert!(client.h2.lock().unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn unused_connection_reaches_idle_timeout() {
        let limit = Duration::from_secs(10);
        let usage = Arc::new(Usage::default());
        let started = Instant::now();
        idle(usage, limit).await;
        assert_eq!(started.elapsed(), limit);
    }

    #[tokio::test(start_paused = true)]
    async fn finishing_last_request_restarts_idle_timeout() {
        let limit = Duration::from_secs(10);
        let usage = Arc::new(Usage::default());
        let request = InFlight::new(&usage);
        let idle = idle(usage.clone(), limit);
        tokio::pin!(idle);
        assert!(futures_util::poll!(&mut idle).is_pending());
        tokio::time::advance(limit).await;
        assert!(futures_util::poll!(&mut idle).is_pending());
        assert_eq!(usage.idle_time(), None);
        drop(request);
        assert_eq!(usage.idle_time(), Some(Duration::ZERO));
        tokio::time::advance(limit - Duration::from_millis(1)).await;
        assert!(futures_util::poll!(&mut idle).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(futures_util::poll!(&mut idle).is_ready());
    }
}
