use std::{net::SocketAddr, sync::Arc, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{
    config::Config,
    protocol,
    server::Server,
    tls::{ListenerConfig, TlsFiles},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsConnector;

pub(super) const WAIT: Duration = Duration::from_secs(3);

pub(super) struct Certificate {
    pub(super) directory: tempfile::TempDir,
    pub(super) files: TlsFiles,
    pub(super) der: rustls::pki_types::CertificateDer<'static>,
}

impl Certificate {
    pub(super) fn new() -> Self {
        Self::with_eku(vec![])
    }

    pub(super) fn with_eku(usages: Vec<rcgen::ExtendedKeyUsagePurpose>) -> Self {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
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
        }
    }
    pub(super) fn listener(&self) -> ListenerConfig {
        ListenerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            files: self.files.clone(),
        }
    }
    pub(super) async fn connect(
        &self,
        address: SocketAddr,
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut roots = RootCertStore::empty();
        roots.add(self.der.clone()).unwrap();
        let mut client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        client.alpn_protocols = vec![b"dot".to_vec()];
        timeout(
            WAIT,
            TlsConnector::from(Arc::new(client)).connect(
                ServerName::try_from("localhost").unwrap(),
                TcpStream::connect(address).await.unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }
}

pub(super) fn config(upstream: SocketAddr) -> Config {
    let mut config = Config::parse(include_str!("../../parins.example.toml")).unwrap();
    config.listen.set_port(0);
    config.upstreams.servers = vec![upstream.to_string()];
    config.query_timeout_ms = 1000;
    config.tcp_io_timeout_ms = 1000;
    config.shutdown_grace_ms = 200;
    config
}

pub(super) fn query(id: u16) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("runtime.test.").unwrap(),
        RecordType::A,
    ));
    query
}

pub(super) fn answer(query: &Message) -> Message {
    let mut answer = protocol::error_response(query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    answer
}

pub(super) async fn udp(address: SocketAddr, id: u16) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(id).to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let length = timeout(WAIT, socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    protocol::decode(&buffer[..length]).unwrap()
}

pub(super) async fn write(stream: &mut (impl AsyncWrite + Unpin), message: &Message) {
    let bytes = message.to_vec().unwrap();
    stream.write_u16(bytes.len() as u16).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
}

pub(super) async fn read(stream: &mut (impl AsyncRead + Unpin)) -> Message {
    timeout(WAIT, async {
        let length = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; usize::from(length)];
        stream.read_exact(&mut bytes).await.unwrap();
        protocol::decode(&bytes).unwrap()
    })
    .await
    .unwrap()
}

pub(super) fn run(server: Server) -> (oneshot::Sender<()>, JoinHandle<anyhow::Result<()>>) {
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    (stop, task)
}

pub(super) async fn finish(stop: oneshot::Sender<()>, task: JoinHandle<anyhow::Result<()>>) {
    stop.send(()).unwrap();
    timeout(WAIT, task).await.unwrap().unwrap().unwrap();
}
