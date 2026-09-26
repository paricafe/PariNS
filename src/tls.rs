//! Verified TLS configuration and DNS-over-TLS adapters (RFC 7858 / RFC 8310).
//! No opportunistic plaintext fallback. The resolver owns upstream deadlines.

use std::{
    fmt,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, ensure};
use hickory_proto::op::Message;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::{ingress::Ingress, metrics::Counter, protocol, transport::tcp};

mod pool;
pub use pool::PoolSettings;
mod certificates;
pub use certificates::{
    CertificateRole, CertificateSet, CertificateSources, CertificateSummary,
    PreparedCertificateSet, RoleSummary,
};
type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    pub listen: SocketAddr,
    #[serde(flatten)]
    pub files: TlsFiles,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSettings {
    pub server_name: String,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

pub fn server_config(files: &TlsFiles, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>> {
    Ok(reloading_server_config(files, alpn)?.0)
}

pub fn server_config_with_key(key: Arc<CertifiedKey>, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(FixedIdentity(key)));
    config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct FixedIdentity(Arc<CertifiedKey>);

impl ResolvesServerCert for FixedIdentity {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// A stable certificate resolver shared by all cloned server configurations.
/// File IO happens during preparation, never on a TLS handshake's hot path.
#[derive(Clone, Debug)]
pub struct Identity {
    files: TlsFiles,
    key: Arc<RwLock<Arc<CertifiedKey>>>,
}

impl Identity {
    pub fn from_key(files: &TlsFiles, key: Arc<CertifiedKey>) -> Self {
        Self {
            files: files.clone(),
            key: Arc::new(RwLock::new(key)),
        }
    }

    pub fn server_config(&self, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>> {
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(self.clone()));
        config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
        Ok(Arc::new(config))
    }

    pub fn prepare(&self) -> Result<Arc<CertifiedKey>> {
        load_identity(&self.files)
    }

    pub fn install(&self, key: Arc<CertifiedKey>) {
        // This lock never encloses fallible work or invokes caller code.
        *self.key.write().unwrap_or_else(|error| error.into_inner()) = key;
    }
}

impl ResolvesServerCert for Identity {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.key
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
    }
}

pub fn reloading_server_config(
    files: &TlsFiles,
    alpn: &[&[u8]],
) -> Result<(Arc<ServerConfig>, Identity)> {
    let identity = Identity::from_key(files, load_identity(files)?);
    let config = identity.server_config(alpn)?;
    Ok((config, identity))
}

pub fn load_identity(files: &TlsFiles) -> Result<Arc<CertifiedKey>> {
    let certificate =
        certificates::read_pem_file(&files.cert_file).context("read TLS certificate")?;
    let private_key =
        certificates::read_pem_file(&files.key_file).context("read TLS private key")?;
    validate_pem_identity(&certificate, &private_key)
}

/// Only the chosen management identity needs a browser host. Dates and key
/// consistency are validated by the shared preparation boundary for all roles.
pub fn validate_management_identity(key: &CertifiedKey, host: &str) -> Result<()> {
    let name = ServerName::try_from(host.to_owned()).context("invalid management TLS name")?;
    let leaf = key.cert.first().context("TLS certificate chain is empty")?;
    let end_entity =
        webpki::EndEntityCert::try_from(leaf).context("invalid management TLS certificate")?;
    end_entity
        .verify_is_valid_for_subject_name(&name)
        .map_err(|_| ManagementNameMismatch)?;
    Ok(())
}

#[derive(Debug)]
pub struct ManagementNameMismatch;

impl fmt::Display for ManagementNameMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("management public_host does not match certificate SAN")
    }
}

impl std::error::Error for ManagementNameMismatch {}

/// Validate an imported identity before any secret is persisted. Parsing and
/// key consistency use the same provider as file-backed listener identities.
pub fn validate_pem_identity(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<Arc<CertifiedKey>> {
    ensure!(
        certificate_pem.len() <= certificates::MAX_PEM_BYTES
            && private_key_pem.len() <= certificates::MAX_PEM_BYTES,
        "TLS PEM exceeds the 64 KiB size limit"
    );
    let certificates = CertificateDer::pem_slice_iter(certificate_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("invalid PEM certificate chain")?;
    ensure!(!certificates.is_empty(), "PEM certificate chain is empty");
    let mut keys = PrivateKeyDer::pem_slice_iter(private_key_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("invalid PEM private key")?;
    ensure!(
        keys.len() == 1,
        "provide exactly one unencrypted PEM private key"
    );
    checked_identity(certificates, keys.remove(0))
}

fn checked_identity(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<CertifiedKey>> {
    for (index, certificate) in certificates.iter().enumerate() {
        let (remaining, parsed) = X509Certificate::from_der(certificate.as_ref())
            .map_err(|_| anyhow::anyhow!("invalid TLS certificate chain"))?;
        ensure!(remaining.is_empty(), "trailing data in TLS certificate");
        ensure!(
            parsed.validity().is_valid(),
            "TLS certificate is not currently valid"
        );
        if index == 0 {
            let usage = parsed
                .extended_key_usage()
                .context("invalid TLS certificate extended key usage")?;
            // rustls-webpki's server_auth usage accepts an absent EKU, but an
            // explicit EKU must contain serverAuth (anyEKU alone is not enough).
            ensure!(
                usage.is_none_or(|usage| usage.value.server_auth),
                "TLS certificate leaf does not allow server authentication"
            );
        }
    }
    let key = CertifiedKey::from_der(certificates, key, &rustls::crypto::ring::default_provider())?;
    // from_der permits providers that cannot determine key consistency; ours must prove it.
    key.keys_match()?;
    Ok(Arc::new(key))
}

#[derive(Clone)]
pub struct Upstream {
    connector: TlsConnector,
    server_name: ServerName<'static>,
    pool: Option<Arc<pool::Pool>>,
}

impl Upstream {
    pub fn new(settings: &ClientSettings) -> Result<Self> {
        Self::with_pool(settings, &PoolSettings::default())
    }

    pub fn with_pool(settings: &ClientSettings, pool: &PoolSettings) -> Result<Self> {
        pool.validate()?;
        let server_name = ServerName::try_from(settings.server_name.clone())
            .context("invalid TLS upstream server name")?;
        let mut roots = RootCertStore::empty();
        if let Some(path) = &settings.ca_file {
            for cert in CertificateDer::pem_file_iter(path).context("open TLS CA file")? {
                roots.add(cert?)?;
            }
            ensure!(!roots.is_empty(), "TLS CA file has no certificates");
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"dot".to_vec()];
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
            pool: pool.enabled.then(|| Arc::new(pool::Pool::new(pool))),
        })
    }

    pub async fn exchange(&self, query: &Message, address: SocketAddr) -> Result<Message> {
        self.exchange_inner(query, address, None).await
    }

    pub(crate) async fn exchange_observed(
        &self,
        query: &Message,
        address: SocketAddr,
        scope: &crate::upstreams::diagnostics::AttemptScope,
    ) -> Result<Message> {
        self.exchange_inner(query, address, Some(scope)).await
    }

    async fn exchange_inner(
        &self,
        query: &Message,
        address: SocketAddr,
        scope: Option<&crate::upstreams::diagnostics::AttemptScope>,
    ) -> Result<Message> {
        use crate::upstreams::diagnostics::{ActualProtocol, Stage};
        let mut attempt = scope.map(|scope| scope.start(Some(ActualProtocol::Dot)));
        let result = async {
            let Some(pool) = &self.pool else {
                return transaction(
                    &mut self.connect(address, &mut attempt).await?,
                    query,
                    &mut attempt,
                )
                .await;
            };
            client_stage(&mut attempt, Stage::Wait);
            let mut slot = pool.checkout(address).await;
            // Never leave a borrowed/partially consumed stream in shared state. A
            // cancelled query drops it along with this guard, leaving None.
            let cached = slot.take().filter(|idle| {
                idle.address == address && idle.returned.elapsed() < pool.idle_timeout
            });
            let reused = cached.is_some();
            let mut stream = match cached {
                Some(idle) => idle.stream,
                None => self.connect(address, &mut attempt).await?,
            };
            let response = match transaction(&mut stream, query, &mut attempt).await {
                Ok(response) => response,
                Err(error) if reused && closed_connection(&error) => {
                    // A server may close an idle connection at any time. Retry only
                    // this transport closure, once, within the original query budget.
                    drop(stream);
                    if let Some(attempt) = &mut attempt {
                        attempt.finish(&Err::<(), _>(error));
                    }
                    ensure!(
                        scope.is_none_or(|scope| scope.remaining()),
                        "DoT deadline exhausted before reconnect"
                    );
                    attempt = scope.map(|scope| scope.start(Some(ActualProtocol::Dot)));
                    stream = self.connect(address, &mut attempt).await?;
                    transaction(&mut stream, query, &mut attempt).await?
                }
                Err(error) => return Err(error),
            };
            *slot = Some(pool::Idle {
                stream,
                address,
                returned: tokio::time::Instant::now(),
            });
            Ok(response)
        }
        .await;
        if let Some(attempt) = &mut attempt {
            attempt.finish(&result);
        }
        result
    }

    async fn connect(
        &self,
        address: SocketAddr,
        attempt: &mut Option<crate::upstreams::diagnostics::Attempt>,
    ) -> Result<ClientStream> {
        use crate::upstreams::diagnostics::Stage;
        client_stage(attempt, Stage::Connect);
        let stream = TcpStream::connect(address).await?;
        client_stage(attempt, Stage::TlsHandshake);
        let stream = self
            .connector
            .connect(self.server_name.clone(), stream)
            .await?;
        client_stage(attempt, Stage::Validate);
        // Legacy DoT servers may omit ALPN; an explicitly different protocol is never accepted.
        ensure!(
            stream
                .get_ref()
                .1
                .alpn_protocol()
                .is_none_or(|p| p == b"dot"),
            "unexpected DoT ALPN"
        );
        Ok(stream)
    }
}

fn closed_connection(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
        )
    })
}

fn client_stage(
    attempt: &mut Option<crate::upstreams::diagnostics::Attempt>,
    stage: crate::upstreams::diagnostics::Stage,
) {
    if let Some(attempt) = attempt {
        attempt.stage(stage);
    }
}

async fn transaction(
    stream: &mut ClientStream,
    query: &Message,
    attempt: &mut Option<crate::upstreams::diagnostics::Attempt>,
) -> Result<Message> {
    use crate::upstreams::diagnostics::Stage;
    let mut outbound = query.clone();
    outbound.metadata.id = rand::random();
    client_stage(attempt, Stage::RequestWrite);
    tcp::write_frame(stream, &protocol::encode_upstream(&outbound, true)?).await?;
    stream.flush().await?;
    client_stage(attempt, Stage::ResponseRead);
    let wire = tcp::read_frame(stream).await?;
    client_stage(attempt, Stage::Decode);
    let mut response = protocol::decode(&wire)?;
    client_stage(attempt, Stage::Validate);
    ensure!(
        protocol::matches_response(&outbound, &response),
        "unrelated DoT upstream response"
    );
    ensure!(!response.truncation, "truncated DoT upstream response");
    response.metadata.id = query.id;
    Ok(response)
}

pub async fn serve(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    mut ingress: Ingress,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    let mut tasks = JoinSet::new();
    let outcome = loop {
        if *ingress.stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = ingress.stop.changed() => break Ok(()),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined { break Err(error.into()); }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(value) => value,
                    Err(error) => break Err(error.into()),
                };
                let Ok(permit) = ingress.connections.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(Counter::ConnectionsRejected);
                    continue;
                };
                let Some(source) = ingress.admit_connection(peer.ip()) else {
                    ingress.resolver.metrics().inc(Counter::ConnectionsRejected);
                    continue;
                };
                let acceptor = acceptor.clone();
                let ingress = ingress.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    // Connection admission precedes TLS work, so slow handshakes are bounded too.
                    let _ = connection(stream, peer, acceptor, ingress).await;
                });
            }
        }
    };
    drop(listener);
    if timeout(ingress.shutdown_grace, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        ingress.resolver.force_shutdown();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    outcome
}

async fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    mut ingress: Ingress,
) -> Result<()> {
    let mut stream = tokio::select! {
        biased;
        _ = ingress.stop.changed() => return Ok(()),
        result = timeout(ingress.io_timeout, acceptor.accept(stream)) => result??,
    };
    ensure!(
        stream
            .get_ref()
            .1
            .alpn_protocol()
            .is_none_or(|p| p == b"dot"),
        "unexpected DoT ALPN"
    );
    loop {
        if *ingress.stop.borrow() {
            return Ok(());
        }
        let bytes = tokio::select! {
            biased;
            _ = ingress.stop.changed() => return Ok(()),
            result = timeout(ingress.io_timeout, tcp::read_frame(&mut stream)) => result??,
        };
        // Once admitted, a query can finish during the server's bounded shutdown grace.
        let Some(response) = ingress
            .handle_with_transport(&bytes, peer.ip(), "dot")
            .await
        else {
            return Ok(());
        };
        timeout(ingress.io_timeout, async {
            tcp::write_frame(&mut stream, &response).await?;
            stream.flush().await
        })
        .await??;
    }
}
