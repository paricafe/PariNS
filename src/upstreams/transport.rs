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
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, timeout},
};
use tokio_rustls::TlsConnector;

use super::diagnostics::{ActualProtocol, Attempt, AttemptScope, Stage};
use super::{Endpoint, Protocol, Settings, valid_address};
use crate::{protocol, transport::tcp};

// RFC 9111 sections 5.1 and 1.2.2: use the first Age member, ignore
// malformed values, and saturate a valid integer that exceeds our TTL range.
pub(super) fn http_age(headers: &http::HeaderMap) -> u32 {
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
pub(super) fn apply_http_age(response: &mut Message, age: u32) {
    for record in response
        .answers
        .iter_mut()
        .chain(&mut response.authorities)
        .chain(&mut response.additionals)
    {
        record.ttl = record.ttl.saturating_sub(age);
    }
}

pub(super) struct Client {
    pub spec: Endpoint,
    bootstrap: Vec<SocketAddr>,
    addresses: Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    tls: Arc<ClientConfig>,
    dot: Option<crate::tls::Upstream>,
    listeners: Vec<SocketAddr>,
    quic: Mutex<Option<Arc<QuicConnection>>>,
    h3: Option<super::h3::Client>,
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
    pub fn new(
        spec: Endpoint,
        settings: &Settings,
        listeners: Vec<SocketAddr>,
        _query_timeout: Duration,
    ) -> Result<Self> {
        let mut roots = RootCertStore::empty();
        if let Some(path) = &settings.ca_file {
            for certificate in CertificateDer::pem_file_iter(path).context("open upstream CA")? {
                roots.add(certificate?)?;
            }
            ensure!(!roots.is_empty(), "upstream CA has no certificates");
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let mut tls =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
        tls.alpn_protocols = vec![match spec.protocol {
            Protocol::Https => b"h2".to_vec(),
            Protocol::Quic => b"doq".to_vec(),
            _ => b"dot".to_vec(),
        }];
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
        let h3 = (settings.prefer_h3 && spec.protocol == Protocol::Https)
            .then(|| super::h3::Client::new(tls.clone()));
        Ok(Self {
            spec,
            bootstrap: settings.bootstrap.clone(),
            addresses: Mutex::new(None),
            tls: Arc::new(tls),
            dot,
            listeners,
            quic: Mutex::new(None),
            h3,
        })
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
        let addresses = if self.spec.host.parse::<IpAddr>().is_err() {
            let mut attempt = scope.start(None);
            let result = self.addresses(deadline, &mut attempt).await;
            attempt.finish(&result);
            result?
        } else {
            vec![SocketAddr::new(
                self.spec.host.parse::<IpAddr>()?,
                self.spec.port,
            )]
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
                    if let Some(h3) = &self.h3
                        && let Ok(response) = h3
                            .exchange(&self.spec, query, address, deadline, scope)
                            .await
                    {
                        Ok(response)
                    } else {
                        ensure!(scope.remaining(), "upstream deadline exhausted before H2");
                        let mut attempt = scope.start(Some(ActualProtocol::Doh2));
                        let result = self.https(query, address, &mut attempt).await;
                        attempt.finish(&result);
                        result
                    }
                }
                Protocol::Quic => {
                    let mut attempt = scope.start(Some(ActualProtocol::Doq));
                    let result = self.quic(query, address, &mut attempt).await;
                    attempt.finish(&result);
                    result
                }
            };
            match response {
                Ok(message) => {
                    return Ok(super::Exchange {
                        message,
                        upstream: self.spec.label.clone(),
                    });
                }
                Err(failure) => error = failure,
            }
        }
        Err(error)
    }

    async fn https(
        &self,
        query: &Message,
        address: SocketAddr,
        attempt: &mut Attempt,
    ) -> Result<Message> {
        attempt.stage(Stage::Connect);
        let socket = TcpStream::connect(address).await?;
        attempt.stage(Stage::TlsHandshake);
        let stream = TlsConnector::from(self.tls.clone())
            .connect(ServerName::try_from(self.spec.host.clone())?, socket)
            .await?;
        attempt.stage(Stage::Validate);
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(b"h2"),
            "DoH upstream must negotiate HTTP/2"
        );
        attempt.stage(Stage::RequestWrite);
        let (sender, connection) = h2::client::Builder::new()
            .max_header_list_size(16 * 1024)
            .handshake(stream)
            .await?;
        let transaction = async {
            attempt.stage(Stage::Wait);
            let mut sender = sender.ready().await?;
            let host = if self.spec.host.contains(':') {
                format!("[{}]", self.spec.host)
            } else {
                self.spec.host.clone()
            };
            let request = http::Request::builder()
                .method("POST")
                .uri(format!(
                    "https://{host}:{}{}",
                    self.spec.port, self.spec.path
                ))
                .header("content-type", "application/dns-message")
                .header("accept", "application/dns-message")
                .body(())?;
            let mut outbound = query.clone();
            outbound.metadata.id = 0;
            attempt.stage(Stage::RequestWrite);
            let (response, mut body) = sender.send_request(request, false)?;
            body.send_data(
                Bytes::from(protocol::encode_upstream(&outbound, true)?),
                true,
            )?;
            attempt.stage(Stage::ResponseHeaders);
            let response = response.await?;
            if response.status() != http::StatusCode::OK {
                attempt.http_status(response.status().as_u16());
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
            let age = http_age(response.headers());
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            attempt.stage(Stage::ResponseRead);
            while let Some(chunk) = body.data().await {
                let chunk = chunk?;
                if bytes.len() + chunk.len() > protocol::MAX_MESSAGE {
                    attempt.reason(super::diagnostics::Reason::ProtocolInvalid);
                }
                ensure!(
                    bytes.len() + chunk.len() <= protocol::MAX_MESSAGE,
                    "DoH response too large"
                );
                bytes.extend_from_slice(&chunk);
                body.flow_control().release_capacity(chunk.len())?;
            }
            attempt.stage(Stage::Decode);
            let mut response = protocol::decode(&bytes)?;
            attempt.stage(Stage::Validate);
            ensure!(
                protocol::matches_response(&outbound, &response) && !response.truncation,
                "invalid DoH response"
            );
            apply_http_age(&mut response, age);
            response.metadata.id = query.id;
            Ok(response)
        };
        tokio::pin!(connection);
        tokio::pin!(transaction);
        tokio::select! { biased; result = &mut transaction => result, result = &mut connection => { result?; transaction.await } }
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
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from((*self.tls).clone())?;
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
        let connection = self.quic_connection(address, attempt).await?;
        // Retain the endpoint owner for this entire request. Bootstrap rotation
        // may replace the cached owner while this stream is still in flight.
        attempt.stage(Stage::Wait);
        let (mut send, mut recv) = connection.connection.open_bi().await?;
        let mut outbound = query.clone();
        outbound.metadata.id = 0;
        // EDNS TCP keepalive is hop-specific and prohibited on DoQ (RFC 9250).
        if let Some(edns) = &mut outbound.edns {
            edns.options_mut()
                .remove(hickory_proto::rr::rdata::opt::EdnsCode::from(11));
        }
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
