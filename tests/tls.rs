mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::Identity;
use hickory_proto::{
    op::{Message, OpCode, ResponseCode},
    rr::RecordType,
};
use parins::{
    ingress::Ingress,
    protocol,
    tls::{self, ClientSettings, TlsFiles},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsConnector;

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
    common::query(99, "example.test.", RecordType::A)
}

fn connector(cert: &Identity) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(cert.der.clone()).unwrap();
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"dot".to_vec()];
    TlsConnector::from(Arc::new(config))
}

async fn listener(
    cert: &Identity,
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
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default())),
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
    let cert = Identity::new(&["localhost"]);
    let (address, stop, task, connections) = listener(&cert, 2).await;
    let stream = TcpStream::connect(address).await.unwrap();
    let mut client = connector(&cert)
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
    let cert = Identity::new(&["localhost"]);
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
