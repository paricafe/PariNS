//! Production-contract fixtures: verified encrypted upstream, missing ECS,
//! Padding, real UDP ingress, and a consumed clean-snapshot restart.
use bytes::Bytes;
use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{
            A,
            opt::{EdnsCode, EdnsOption},
        },
    },
};
use parins::{
    cache::persistence::semantic_fingerprint, cache_persistence::CachePersistence, config::Config,
    ecs, protocol, server::Server,
};
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::oneshot,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

struct Mock {
    address: SocketAddr,
    count: Arc<AtomicUsize>,
    ca: std::path::PathBuf,
    _directory: tempfile::TempDir,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn answer(query: &Message) -> Message {
    assert!(ecs::subnet(query).is_some());
    let mut answer = protocol::error_response(query, ResponseCode::NoError);
    ecs::set_subnet(&mut answer, None);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    answer
        .edns
        .get_or_insert_with(Edns::new)
        .options_mut()
        .insert(EdnsOption::Unknown(12, vec![7; 700]));
    answer
}

async fn mock(http: bool) -> Mock {
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, certificate.cert.pem()).unwrap();
    let mut tls =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap();
    tls.alpn_protocols = vec![if http {
        b"h2".to_vec()
    } else {
        b"dot".to_vec()
    }];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                stream=listener.accept() => {
                    let (stream,_)=stream.unwrap();
                    let acceptor=acceptor.clone();
                    let count=observed.clone();
                    connections.spawn(async move {
                        let mut stream=acceptor.accept(stream).await.unwrap();
                        if http {
                            let mut connection=h2::server::handshake(stream).await.unwrap();
                            while let Some(Ok((request,mut respond)))=connection.accept().await {
                                let mut body=request.into_body();
                                let mut bytes=Vec::new();
                                while let Some(data)=body.data().await { let data=data.unwrap(); bytes.extend_from_slice(&data); body.flow_control().release_capacity(data.len()).unwrap(); }
                                count.fetch_add(1,Ordering::SeqCst);
                                let answer=answer(&protocol::decode(&bytes).unwrap()).to_vec().unwrap();
                                respond.send_response(http::Response::builder().status(200).header("content-type","application/dns-message").body(()).unwrap(),false).unwrap()
                                    .send_data(Bytes::from(answer),true).unwrap();
                            }
                        } else {
                            while let Ok(length)=stream.read_u16().await {
                                let mut bytes=vec![0;length as usize];
                                stream.read_exact(&mut bytes).await.unwrap();
                                count.fetch_add(1,Ordering::SeqCst);
                                let answer=answer(&protocol::decode(&bytes).unwrap()).to_vec().unwrap();
                                stream.write_u16(answer.len() as u16).await.unwrap();
                                stream.write_all(&answer).await.unwrap();
                                stream.flush().await.unwrap();
                            }
                        }
                    });
                }
            }
        }
    });
    Mock {
        address,
        count,
        ca,
        _directory: directory,
        task,
    }
}

async fn request(address: SocketAddr, source: &str, id: u16) -> Message {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("encrypted-cache.test.").unwrap(),
        RecordType::A,
    ));
    ecs::set_subnet(&mut query, Some(source.parse().unwrap()));
    query
        .edns
        .as_mut()
        .unwrap()
        .options_mut()
        .insert(EdnsOption::Unknown(12, vec![5; usize::from(id)]));
    socket
        .send_to(&query.to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let (length, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let response = protocol::decode(&buffer[..length]).unwrap();
    assert_eq!(response.id, id);
    assert_eq!(response.answers.len(), 1);
    assert!(!response.truncation);
    assert!(
        response
            .edns
            .as_ref()
            .unwrap()
            .option(EdnsCode::Padding)
            .is_none(),
        "plaintext final hop must remove upstream padding"
    );
    response
}

#[tokio::test]
async fn verified_dot_and_https_missing_ecs_padding_cache_and_clean_restart() {
    for http in [false, true] {
        let upstream = mock(http).await;
        let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.listen = "127.0.0.1:0".parse().unwrap();
        config.ecs.enabled = true;
        config.upstreams.servers = vec![format!(
            "{}://{}{}",
            if http { "https" } else { "tls" },
            upstream.address,
            if http { "/dns-query" } else { "" }
        )];
        config.upstreams.ca_file = Some(upstream.ca.clone());
        let directory = tempfile::tempdir().unwrap();
        let persistence = CachePersistence::new(directory.path());
        let flag = AtomicBool::new(false);
        persistence
            .consume_startup(&config.cache.persistence, &flag)
            .unwrap();
        let server = Server::bind(config.clone()).await.unwrap();
        let resolver = server.resolver().clone();
        let address = server.local_addr().unwrap();
        let (stop, stopping) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopping.await;
        }));
        for id in [1, 2] {
            let response = request(address, "127.0.0.1/32", id).await;
            assert_eq!(ecs::subnet(&response).unwrap().scope_prefix(), 24);
            assert_eq!(upstream.count.load(Ordering::SeqCst), 1);
        }
        for id in [3, 4] {
            let response = request(address, "127.0.0.0/16", id).await;
            assert_eq!(ecs::subnet(&response).unwrap().scope_prefix(), 16);
            assert_eq!(upstream.count.load(Ordering::SeqCst), 2);
        }
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(resolver.is_quiescent());
        let fingerprint = semantic_fingerprint(&config, &resolver.policy_digest()).unwrap();
        assert_eq!(
            persistence
                .save_terminal(
                    &resolver.cache(),
                    &fingerprint,
                    &config.cache.persistence,
                    SystemTime::now(),
                    Instant::now(),
                    &flag
                )
                .unwrap()
                .saved,
            1
        );
        let owner = CachePersistence::new(directory.path());
        let candidate = owner
            .consume_startup(&config.cache.persistence, &flag)
            .unwrap();
        let server = Server::bind(config.clone()).await.unwrap();
        let report = candidate.restore_into(
            &server.resolver().cache(),
            &fingerprint,
            SystemTime::now(),
            Instant::now(),
            &flag,
        );
        assert_eq!(report.restored, 1, "{report:?}");
        let address = server.local_addr().unwrap();
        let (stop, stopping) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopping.await;
        }));
        let response = request(address, "127.0.0.0/16", 5).await;
        assert!(response.answers[0].ttl < 60);
        assert_eq!(
            upstream.count.load(Ordering::SeqCst),
            2,
            "restart must remain a cache hit"
        );
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
