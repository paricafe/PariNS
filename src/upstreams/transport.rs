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

use super::{Endpoint, Protocol, Settings, valid_address};
use crate::{protocol, transport::tcp};

pub(super) struct Client {
    pub spec: Endpoint,
    bootstrap: Vec<SocketAddr>,
    addresses: Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    tls: Arc<ClientConfig>,
    dot: Option<crate::tls::Upstream>,
    listeners: Vec<SocketAddr>,
    quic: Mutex<Option<QuicConnection>>,
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
        query_timeout: Duration,
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
            .then(|| super::h3::Client::new(tls.clone(), query_timeout));
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

    async fn addresses(&self) -> Result<Vec<SocketAddr>> {
        if let Ok(ip) = self.spec.host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, self.spec.port)]);
        }
        let mut cached = self.addresses.lock().await;
        if let Some((_, addresses)) = cached
            .as_ref()
            .filter(|(expiry, _)| *expiry > Instant::now())
        {
            return Ok(addresses.clone());
        }
        // Explicit bootstrap, never the host's system resolver. The caller's
        // remaining overall deadline encloses lookup and every transport step.
        let name = Name::from_ascii(format!("{}.", self.spec.host))?;
        let mut addresses = Vec::new();
        let mut ttl = 3600;
        for server in &self.bootstrap {
            for qtype in [RecordType::A, RecordType::AAAA] {
                let mut query = Message::new(0, MessageType::Query, OpCode::Query);
                query.metadata.recursion_desired = true;
                query.add_query(Query::query(name.clone(), qtype));
                let Ok(Ok(response)) = timeout(
                    Duration::from_millis(500),
                    crate::upstream::exchange(&query, *server),
                )
                .await
                else {
                    continue;
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
                    valid_address(address)?;
                    super::not_self(address, &self.listeners)?;
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

    pub async fn exchange(&self, query: &Message) -> Result<super::Exchange> {
        let mut error = anyhow::anyhow!("no upstream address");
        for address in self.addresses().await? {
            let response = match self.spec.protocol {
                Protocol::Udp => crate::upstream::exchange(query, address).await,
                Protocol::Tcp => crate::upstream::exchange_tcp(query, address).await,
                Protocol::Tls => {
                    self.dot
                        .as_ref()
                        .expect("DoT client")
                        .exchange(query, address)
                        .await
                }
                Protocol::Https => {
                    if let Some(h3) = &self.h3
                        && let Ok(response) = h3.exchange(&self.spec, query, address).await
                    {
                        Ok(response)
                    } else {
                        self.https(query, address).await
                    }
                }
                Protocol::Quic => self.quic(query, address).await,
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

    async fn https(&self, query: &Message, address: SocketAddr) -> Result<Message> {
        let socket = TcpStream::connect(address).await?;
        let stream = TlsConnector::from(self.tls.clone())
            .connect(ServerName::try_from(self.spec.host.clone())?, socket)
            .await?;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(b"h2"),
            "DoH upstream must negotiate HTTP/2"
        );
        let (sender, connection) = h2::client::Builder::new()
            .max_header_list_size(16 * 1024)
            .handshake(stream)
            .await?;
        let transaction = async {
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
            let (response, mut body) = sender.send_request(request, false)?;
            body.send_data(Bytes::from(outbound.to_vec()?), true)?;
            let response = response.await?;
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
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = chunk?;
                ensure!(
                    bytes.len() + chunk.len() <= protocol::MAX_MESSAGE,
                    "DoH response too large"
                );
                bytes.extend_from_slice(&chunk);
                body.flow_control().release_capacity(chunk.len())?;
            }
            let mut response = protocol::decode(&bytes)?;
            ensure!(
                protocol::matches_response(&outbound, &response) && !response.truncation,
                "invalid DoH response"
            );
            response.metadata.id = query.id;
            Ok(response)
        };
        tokio::pin!(connection);
        tokio::pin!(transaction);
        tokio::select! { biased; result = &mut transaction => result, result = &mut connection => { result?; transaction.await } }
    }

    async fn quic_connection(&self, address: SocketAddr) -> Result<quinn::Connection> {
        let mut cached = self.quic.lock().await;
        if let Some(current) = cached.as_ref().filter(|c| {
            c.connection.remote_address() == address && c.connection.close_reason().is_none()
        }) {
            return Ok(current.connection.clone());
        }
        *cached = None;
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
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
        let connection = endpoint.connect(address, &self.spec.host)?.await?;
        *cached = Some(QuicConnection {
            endpoint,
            connection: connection.clone(),
        });
        Ok(connection)
    }

    async fn quic(&self, query: &Message, address: SocketAddr) -> Result<Message> {
        let connection = self.quic_connection(address).await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut outbound = query.clone();
        outbound.metadata.id = 0;
        // EDNS TCP keepalive is hop-specific and prohibited on DoQ (RFC 9250).
        if let Some(edns) = &mut outbound.edns {
            edns.options_mut()
                .remove(hickory_proto::rr::rdata::opt::EdnsCode::from(11));
        }
        tcp::write_frame(&mut send, &outbound.to_vec()?).await?;
        send.finish()?;
        let frame = recv.read_to_end(protocol::MAX_MESSAGE + 2).await?;
        ensure!(
            frame.len() >= 14
                && usize::from(u16::from_be_bytes([frame[0], frame[1]])) == frame.len() - 2,
            "invalid DoQ response length"
        );
        let mut response = protocol::decode(&frame[2..])?;
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
