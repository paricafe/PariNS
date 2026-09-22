//! Verified TLS configuration and DNS-over-TLS adapters (RFC 7858 / RFC 8310).
//! No opportunistic plaintext fallback. The resolver owns upstream deadlines.

use std::{
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
use serde::Deserialize;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::{ingress::Ingress, metrics::Counter, protocol, transport::tcp};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    pub listen: SocketAddr,
    #[serde(flatten)]
    pub files: TlsFiles,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSettings {
    pub server_name: String,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

pub fn server_config(files: &TlsFiles, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>> {
    Ok(reloading_server_config(files, alpn)?.0)
}

/// A stable certificate resolver shared by all cloned server configurations.
/// File IO happens during preparation, never on a TLS handshake's hot path.
#[derive(Clone, Debug)]
pub struct Identity {
    files: TlsFiles,
    key: Arc<RwLock<Arc<CertifiedKey>>>,
}

impl Identity {
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
    let identity = Identity {
        files: files.clone(),
        key: Arc::new(RwLock::new(load_identity(files)?)),
    };
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(identity.clone()));
    config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
    Ok((Arc::new(config), identity))
}

fn load_identity(files: &TlsFiles) -> Result<Arc<CertifiedKey>> {
    let certificates = CertificateDer::pem_file_iter(&files.cert_file)
        .context("open TLS certificate")?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        !certificates.is_empty(),
        "TLS certificate file has no certificates"
    );
    let key = PrivateKeyDer::from_pem_file(&files.key_file).context("load TLS private key")?;
    let key = CertifiedKey::from_der(certificates, key, &rustls::crypto::ring::default_provider())?;
    // from_der permits providers that cannot determine key consistency; ours must prove it.
    key.keys_match()?;
    Ok(Arc::new(key))
}

#[derive(Clone)]
pub struct Upstream {
    connector: TlsConnector,
    server_name: ServerName<'static>,
}

impl Upstream {
    pub fn new(settings: &ClientSettings) -> Result<Self> {
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
        })
    }

    pub async fn exchange(&self, query: &Message, address: SocketAddr) -> Result<Message> {
        let stream = TcpStream::connect(address).await?;
        let mut stream = self
            .connector
            .connect(self.server_name.clone(), stream)
            .await?;
        // Legacy DoT servers may omit ALPN; an explicitly different protocol is never accepted.
        ensure!(
            stream
                .get_ref()
                .1
                .alpn_protocol()
                .is_none_or(|p| p == b"dot"),
            "unexpected DoT ALPN"
        );
        let mut outbound = query.clone();
        outbound.metadata.id = rand::random();
        tcp::write_frame(&mut stream, &outbound.to_vec()?).await?;
        stream.flush().await?;
        let mut response = protocol::decode(&tcp::read_frame(&mut stream).await?)?;
        ensure!(
            protocol::matches_response(&outbound, &response),
            "unrelated DoT upstream response"
        );
        ensure!(!response.truncation, "truncated DoT upstream response");
        response.metadata.id = query.id;
        Ok(response)
    }
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
                let acceptor = acceptor.clone();
                let ingress = ingress.clone();
                tasks.spawn(async move {
                    let _permit = permit;
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
        let Some(response) = ingress.handle(&bytes, peer.ip()).await else {
            return Ok(());
        };
        timeout(ingress.io_timeout, async {
            tcp::write_frame(&mut stream, &response).await?;
            stream.flush().await
        })
        .await??;
    }
}
