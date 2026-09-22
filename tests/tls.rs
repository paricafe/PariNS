mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{
    ingress::Ingress,
    protocol,
    tls::{self, ClientSettings, TlsFiles, Upstream},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

// Independent framing in the client fixture; transport helpers remain private.
mod tcp {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    pub async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
        let length = stream.read_u16().await?;
        let mut bytes = vec![0; usize::from(length)];
        stream.read_exact(&mut bytes).await?;
        Ok(bytes)
    }
    pub async fn write_frame(
        stream: &mut (impl AsyncWrite + Unpin),
        bytes: &[u8],
    ) -> std::io::Result<()> {
        stream
            .write_all(&(bytes.len() as u16).to_be_bytes())
            .await?;
        stream.write_all(bytes).await
    }
}

fn query() -> Message {
    let mut query = Message::new(99, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("example.test.").unwrap(),
        RecordType::A,
    ));
    query
}

struct Certificate {
    _directory: tempfile::TempDir,
    files: TlsFiles,
    der: rustls::pki_types::CertificateDer<'static>,
}

impl Certificate {
    fn new() -> Self {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            cert_file: directory.path().join("cert.pem"),
            key_file: directory.path().join("key.pem"),
        };
        std::fs::write(&files.cert_file, generated.cert.pem()).unwrap();
        std::fs::write(&files.key_file, generated.signing_key.serialize_pem()).unwrap();
        Self {
            _directory: directory,
            files,
            der: generated.cert.der().clone(),
        }
    }

    fn settings(&self) -> ClientSettings {
        ClientSettings {
            server_name: "localhost".into(),
            ca_file: Some(self.files.cert_file.clone()),
        }
    }

    fn connector(&self) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots.add(self.der.clone()).unwrap();
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"dot".to_vec()];
        TlsConnector::from(Arc::new(config))
    }
}

async fn upstream_mock(
    cert: &Certificate,
    invalid_id: bool,
    alpn: &[&[u8]],
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(tls::server_config(&cert.files, alpn).unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // Certificate rejection is an expected outcome in authentication tests.
        let Ok(mut stream) = acceptor.accept(stream).await else {
            return;
        };
        let Ok(bytes) = tcp::read_frame(&mut stream).await else {
            return;
        };
        let query = protocol::decode(&bytes).unwrap();
        let mut response = protocol::error_response(&query, ResponseCode::NoError);
        if invalid_id {
            response.metadata.id = query.id.wrapping_add(1);
        }
        tcp::write_frame(&mut stream, &response.to_vec().unwrap())
            .await
            .unwrap();
        stream.flush().await.unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn authenticated_dot_upstream_restores_id_and_accepts_legacy_no_alpn() {
    let cert = Certificate::new();
    for alpn in [&[&b"dot"[..]][..], &[][..]] {
        let (address, task) = upstream_mock(&cert, false, alpn).await;
        let response = timeout(
            Duration::from_secs(2),
            Upstream::new(&cert.settings())
                .unwrap()
                .exchange(&query(), address),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.id, query().id);
        assert_eq!(response.queries, query().queries);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn dot_upstream_rejects_wrong_identity_unknown_ca_and_unrelated_dns() {
    let cert = Certificate::new();
    for mode in 0..3 {
        let mut settings = cert.settings();
        if mode == 0 {
            settings.server_name = "different.test".into();
        }
        if mode == 1 {
            settings.ca_file = None;
        }
        let (address, task) = upstream_mock(&cert, mode == 2, &[b"dot"]).await;
        assert!(
            timeout(
                Duration::from_secs(2),
                Upstream::new(&settings)
                    .unwrap()
                    .exchange(&query(), address)
            )
            .await
            .unwrap()
            .is_err()
        );
        task.await.unwrap();
    }
}

async fn listener(
    cert: &Certificate,
    slots: usize,
) -> (
    SocketAddr,
    watch::Sender<bool>,
    JoinHandle<anyhow::Result<()>>,
    Arc<Semaphore>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = watch::channel(false);
    let connections = Arc::new(Semaphore::new(slots));
    let ingress = Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default()).unwrap()),
        resolver: Arc::new(common::resolver(
            "127.0.0.1:9".parse().unwrap(),
            Duration::from_millis(100),
        )),
        queries: Arc::new(Semaphore::new(2)),
        connections: connections.clone(),
        stop: stopped,
        io_timeout: Duration::from_millis(150),
        shutdown_grace: Duration::from_millis(100),
        max_streams: 8,
    };
    let task = tokio::spawn(tls::serve(
        listener,
        tls::server_config(&cert.files, &[b"dot"]).unwrap(),
        ingress,
    ));
    (address, stop, task, connections)
}

#[tokio::test]
async fn dot_listener_reuses_connection_rejects_short_frame_and_shuts_down() {
    let cert = Certificate::new();
    let (address, stop, task, connections) = listener(&cert, 2).await;
    let stream = TcpStream::connect(address).await.unwrap();
    let mut client = cert
        .connector()
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .unwrap();
    // Unsupported opcodes are handled locally, proving the shared resolver path without Internet IO.
    for id in [42, 43] {
        let mut query = query();
        query.metadata.id = id;
        query.metadata.op_code = OpCode::Update;
        tcp::write_frame(&mut client, &query.to_vec().unwrap())
            .await
            .unwrap();
        client.flush().await.unwrap();
        let response = protocol::decode(
            &timeout(Duration::from_secs(2), tcp::read_frame(&mut client))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.response_code, ResponseCode::NotImp);
    }
    client.write_all(&[0, 1, 0]).await.unwrap();
    client.flush().await.unwrap();
    // TLS close without close_notify may surface EOF as an error; either outcome closes this connection.
    let read = timeout(Duration::from_secs(1), client.read_u8())
        .await
        .unwrap();
    assert!(read.is_err());
    stop.send(true).unwrap();
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(connections.available_permits(), 2);
    TcpListener::bind(address).await.unwrap();
}

#[tokio::test]
async fn unfinished_handshakes_hold_connection_budget_and_shutdown_is_bounded() {
    let cert = Certificate::new();
    let (address, stop, task, connections) = listener(&cert, 1).await;
    let _idle = TcpStream::connect(address).await.unwrap();
    timeout(Duration::from_secs(1), async {
        while connections.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut rejected = TcpStream::connect(address).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), rejected.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    stop.send(true).unwrap();
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(connections.available_permits(), 1);
}

#[test]
fn tls_configuration_rejects_missing_material_and_unknown_settings() {
    let directory = tempfile::tempdir().unwrap();
    let files = TlsFiles {
        cert_file: directory.path().join("missing"),
        key_file: directory.path().join("missing-key"),
    };
    assert!(tls::server_config(&files, &[b"dot"]).is_err());
    assert!(
        toml::from_str::<ClientSettings>("server_name = 'localhost'\ninsecure = true").is_err()
    );
    assert!(
        toml::from_str::<tls::ListenerConfig>(
            "listen = '127.0.0.1:853'\ncert_file = 'c'\nkey_file = 'k'"
        )
        .is_ok()
    );
}

#[tokio::test]
async fn certificate_reload_changes_fresh_handshakes_and_keeps_existing_connections() {
    let first = Certificate::new();
    let second = Certificate::new();
    assert_ne!(first.der, second.der);
    let (config, identity) = tls::reloading_server_config(&first.files, &[b"dot"]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(config);
    let server = tokio::spawn(async move {
        let mut clients = tokio::task::JoinSet::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            clients.spawn(async move {
                let mut stream = acceptor.accept(stream).await.unwrap();
                while let Ok(byte) = stream.read_u8().await {
                    stream.write_u8(byte).await.unwrap();
                    stream.flush().await.unwrap();
                }
            });
        }
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
    });
    let mut existing = first
        .connector()
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        existing.get_ref().1.peer_certificates().unwrap()[0],
        first.der
    );

    std::fs::copy(&second.files.cert_file, &first.files.cert_file).unwrap();
    std::fs::copy(&second.files.key_file, &first.files.key_file).unwrap();
    identity.install(identity.prepare().unwrap());
    // A fresh client config cannot resume the old TLS session.
    let replacement = second
        .connector()
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        replacement.get_ref().1.peer_certificates().unwrap()[0],
        second.der
    );
    existing.write_u8(42).await.unwrap();
    existing.flush().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), existing.read_u8())
            .await
            .unwrap()
            .unwrap(),
        42
    );
    assert_eq!(
        existing.get_ref().1.peer_certificates().unwrap()[0],
        first.der
    );

    // Neither malformed files nor a syntactically valid mismatched key can publish a candidate.
    std::fs::write(&first.files.key_file, "not a PEM key").unwrap();
    assert!(identity.prepare().is_err());
    let mismatched = Certificate::new();
    std::fs::copy(&mismatched.files.key_file, &first.files.key_file).unwrap();
    assert!(identity.prepare().is_err());
    let after_failure = second
        .connector()
        .connect(
            ServerName::try_from("localhost").unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        after_failure.get_ref().1.peer_certificates().unwrap()[0],
        second.der
    );
    drop(existing);
    drop(replacement);
    drop(after_failure);
    timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}
