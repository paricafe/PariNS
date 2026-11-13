// Shared fixtures for DNS integration tests.
#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query},
    rr::{Name, RecordType},
};
use parins::tls::{ClientSettings, ListenerConfig, TlsFiles};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

pub fn resolver(upstream: SocketAddr, timeout: Duration) -> parins::resolver::Resolver {
    let mut config =
        parins::config::Config::parse(include_str!("../../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![upstream.to_string()];
    config.query_timeout_ms = timeout.as_millis().try_into().unwrap();
    parins::resolver::Resolver::from_config(&config)
}

pub fn query(id: u16, name: &str, kind: RecordType) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(Name::from_ascii(name).unwrap(), kind));
    query
}

pub struct Identity {
    pub directory: tempfile::TempDir,
    pub files: TlsFiles,
    pub der: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn new(names: &[&str]) -> Self {
        Self::with_usages(names, Vec::new())
    }

    pub fn with_usages(names: &[&str], usages: Vec<rcgen::ExtendedKeyUsagePurpose>) -> Self {
        let mut params =
            rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap();
        params.extended_key_usages = usages;
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            cert_file: directory.path().join("cert.pem"),
            key_file: directory.path().join("key.pem"),
        };
        std::fs::write(&files.cert_file, certificate.pem()).unwrap();
        std::fs::write(&files.key_file, key.serialize_pem()).unwrap();
        Self {
            directory,
            files,
            der: certificate.der().clone(),
            key: PrivatePkcs8KeyDer::from(key.serialize_der()),
        }
    }

    pub fn listener(&self) -> ListenerConfig {
        ListenerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            files: self.files.clone(),
        }
    }

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

    pub fn client_settings(&self, server_name: &str) -> ClientSettings {
        ClientSettings {
            server_name: server_name.into(),
            ca_file: Some(self.files.cert_file.clone()),
        }
    }
}
