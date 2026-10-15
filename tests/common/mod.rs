// Each integration-test crate uses only some of these fixtures.
#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query},
    rr::{Name, RecordType},
};
use parins::tls::{ClientSettings, TlsFiles};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

pub fn resolver(upstream: SocketAddr, timeout: Duration) -> parins::resolver::Resolver {
    let mut config =
        parins::config::Config::parse(include_str!("../../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![upstream.to_string()];
    config.query_timeout_ms = timeout.as_millis().try_into().unwrap();
    parins::resolver::Resolver::from_config(&config)
}

/// One question with no header flags or EDNS; callers add what they test.
pub fn query(id: u16, name: &str, kind: RecordType) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(Name::from_ascii(name).unwrap(), kind));
    query
}

/// A self-signed identity for `names`, also written as `cert.pem`/`key.pem`
/// in a private temporary directory that lives as long as this value.
pub struct Identity {
    pub directory: tempfile::TempDir,
    pub files: TlsFiles,
    pub der: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn new(names: &[&str]) -> Self {
        let generated = rcgen::generate_simple_self_signed(
            names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            cert_file: directory.path().join("cert.pem"),
            key_file: directory.path().join("key.pem"),
        };
        std::fs::write(&files.cert_file, generated.cert.pem()).unwrap();
        std::fs::write(&files.key_file, generated.signing_key.serialize_pem()).unwrap();
        Self {
            directory,
            files,
            der: generated.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()),
        }
    }

    /// An independent in-memory server configuration offering `alpn`.
    pub fn server_config(&self, alpn: &[&[u8]]) -> Arc<rustls::ServerConfig> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![self.der.clone()], self.key.clone_key().into())
        .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Arc::new(config)
    }

    /// DoT client settings that trust only this certificate.
    pub fn client_settings(&self, server_name: &str) -> ClientSettings {
        ClientSettings {
            server_name: server_name.into(),
            ca_file: Some(self.files.cert_file.clone()),
        }
    }
}
