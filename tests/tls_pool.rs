//! Local TLS peers exercise pool ownership; no Internet or checked-in private keys.
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{
    config::Config,
    protocol,
    resolver::Resolver,
    tls::{self, ClientSettings, PoolSettings, TlsFiles, Upstream},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use tokio_rustls::TlsAcceptor;

const DEADLINE: Duration = Duration::from_secs(3);

struct Certificate {
    _directory: tempfile::TempDir,
    files: TlsFiles,
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
        }
    }

    fn settings(&self) -> ClientSettings {
        ClientSettings {
            server_name: "localhost".into(),
            ca_file: Some(self.files.cert_file.clone()),
        }
    }

    fn upstream(&self, max_connections: usize) -> Upstream {
        Upstream::with_pool(&self.settings(), &pool(max_connections)).unwrap()
    }
}

fn pool(max_connections: usize) -> PoolSettings {
    PoolSettings {
        enabled: true,
        max_connections,
        idle_timeout_ms: 30_000,
    }
}

fn query(id: u16) -> Message {
    let mut query = Message::new(id, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii(format!("q{id}.example.test.")).unwrap(),
        RecordType::A,
    ));
    query
}

enum Action {
    Reply,
    WrongId,
    Truncated,
    Partial(oneshot::Sender<()>),
    CloseAfterReply(oneshot::Sender<()>),
}

struct Request {
    connection: usize,
    reply: oneshot::Sender<Action>,
}

struct Peer {
    address: SocketAddr,
    accepted: Arc<AtomicUsize>,
    requests: mpsc::UnboundedReceiver<Request>,
    task: JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Dropping the accept loop also drops its JoinSet, aborting every connection.
        self.task.abort();
    }
}

impl Peer {
    async fn new(cert: &Certificate) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(tls::server_config(&cert.files, &[b"dot"]).unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        let (events, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (tcp, _) = accepted.unwrap();
                        let connection = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        let acceptor = acceptor.clone();
                        let events = events.clone();
                        connections.spawn(async move {
                            let Ok(mut stream) = acceptor.accept(tcp).await else { return; };
                            loop {
                                let Ok(length) = stream.read_u16().await else { return; };
                                let mut wire = vec![0; usize::from(length)];
                                if stream.read_exact(&mut wire).await.is_err() { return; }
                                let query = protocol::decode(&wire).unwrap();
                                let (reply, action) = oneshot::channel();
                                if events.send(Request { connection, reply }).is_err() { return; }
                                let Ok(action) = action.await else { return; };
                                let mut response = protocol::error_response(&query, ResponseCode::NoError);
                                if matches!(action, Action::WrongId) {
                                    response.metadata.id = query.id.wrapping_add(1);
                                }
                                if matches!(action, Action::Truncated) {
                                    response.metadata.truncation = true;
                                }
                                let wire = response.to_vec().unwrap();
                                if stream.write_u16(wire.len() as u16).await.is_err() { return; }
                                if let Action::Partial(sent) = action {
                                    if stream.write_all(&wire[..5]).await.is_err() { return; }
                                    if stream.flush().await.is_err() { return; }
                                    let _ = sent.send(());
                                    // Await closure instead of completing the response.
                                    let _ = stream.read_u8().await;
                                    return;
                                }
                                if stream.write_all(&wire).await.is_err() { return; }
                                if stream.flush().await.is_err() { return; }
                                if let Action::CloseAfterReply(closed) = action {
                                    let _ = stream.shutdown().await;
                                    drop(stream);
                                    let _ = closed.send(());
                                    return;
                                }
                            }
                        });
                    }
                    joined = connections.join_next(), if !connections.is_empty() => {
                        joined.unwrap().unwrap();
                    }
                }
            }
        });
        Self {
            address,
            accepted,
            requests,
            task,
        }
    }

    async fn request(&mut self) -> Request {
        timeout(DEADLINE, self.requests.recv())
            .await
            .expect("upstream query deadline")
            .expect("fixture stopped")
    }

    async fn reply(&mut self, action: Action) -> usize {
        let request = self.request().await;
        request
            .reply
            .send(action)
            .unwrap_or_else(|_| panic!("client abandoned query"));
        request.connection
    }

    fn count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn exchange(
    upstream: &Upstream,
    address: SocketAddr,
    id: u16,
) -> JoinHandle<anyhow::Result<Message>> {
    let upstream = upstream.clone();
    tokio::spawn(async move {
        timeout(DEADLINE, upstream.exchange(&query(id), address))
            .await
            .expect("exchange deadline")
    })
}

async fn success(task: JoinHandle<anyhow::Result<Message>>, id: u16) {
    let response = timeout(DEADLINE, task).await.unwrap().unwrap().unwrap();
    assert_eq!(response.id, id);
    assert_eq!(response.queries, query(id).queries);
}

#[tokio::test]
async fn sequential_clones_reuse_authenticated_connection_and_restore_each_id() {
    let cert = Certificate::new();
    let mut peer = Peer::new(&cert).await;
    let upstream = cert.upstream(1);
    for id in [10, 11, 12] {
        let task = exchange(&upstream.clone(), peer.address, id);
        assert_eq!(peer.reply(Action::Reply).await, 1);
        success(task, id).await;
    }
    assert_eq!(peer.count(), 1);
}

#[tokio::test]
async fn disabled_pool_and_legacy_constructor_always_open_fresh_connections() {
    let cert = Certificate::new();
    for upstream in [
        Upstream::new(&cert.settings()).unwrap(),
        Upstream::with_pool(&cert.settings(), &PoolSettings::default()).unwrap(),
    ] {
        let mut peer = Peer::new(&cert).await;
        for id in [20, 21] {
            let task = exchange(&upstream, peer.address, id);
            peer.reply(Action::Reply).await;
            success(task, id).await;
        }
        assert_eq!(peer.count(), 2);
    }
}

#[tokio::test]
async fn clone_concurrency_is_bounded_and_canceled_waiters_do_not_consume_slots() {
    let cert = Certificate::new();
    for cap in [1, 2] {
        let mut peer = Peer::new(&cert).await;
        let upstream = cert.upstream(cap);
        let mut running = Vec::new();
        let mut blocked = Vec::new();
        for n in 0..cap {
            running.push(exchange(&upstream, peer.address, 30 + n as u16));
            blocked.push(peer.request().await);
        }
        // A caller-owned deadline must encompass time waiting for a pooled slot.
        assert!(
            timeout(
                Duration::from_millis(80),
                upstream.clone().exchange(&query(39), peer.address)
            )
            .await
            .is_err()
        );
        assert_eq!(peer.count(), cap);
        assert!(peer.requests.try_recv().is_err());
        for request in blocked {
            request
                .reply
                .send(Action::Reply)
                .unwrap_or_else(|_| panic!("query canceled"));
        }
        for (n, task) in running.into_iter().enumerate() {
            success(task, 30 + n as u16).await;
        }
        let task = exchange(&upstream, peer.address, 40);
        peer.reply(Action::Reply).await;
        success(task, 40).await;
        assert_eq!(peer.count(), cap);
    }
}

#[tokio::test]
async fn queued_query_uses_whichever_connection_becomes_available_first() {
    let cert = Certificate::new();
    let mut peer = Peer::new(&cert).await;
    let upstream = cert.upstream(2);
    let first = exchange(&upstream, peer.address, 41);
    let held_first = peer.request().await;
    let second = exchange(&upstream, peer.address, 42);
    let held_second = peer.request().await;
    assert_ne!(held_first.connection, held_second.connection);

    let waiting_query = query(43);
    let mut waiting = Box::pin(upstream.exchange(&waiting_query, peer.address));
    // Actually register the waiter while both slots are occupied; no scheduler
    // delay or sleep is used to guess whether checkout has already happened.
    assert!(futures_util::poll!(&mut waiting).is_pending());
    held_second
        .reply
        .send(Action::Reply)
        .unwrap_or_else(|_| panic!("second query canceled"));
    success(second, 42).await;

    let available = timeout(DEADLINE, async {
        tokio::select! {
            request = peer.request() => request,
            _ = &mut waiting => panic!("waiting query completed before peer replied"),
        }
    })
    .await
    .expect("queued query ignored the free slot while another slot stayed busy");
    assert_eq!(available.connection, held_second.connection);
    available
        .reply
        .send(Action::Reply)
        .unwrap_or_else(|_| panic!("waiting query canceled"));
    let response = timeout(DEADLINE, waiting).await.unwrap().unwrap();
    assert_eq!(response.id, 43);
    assert_eq!(peer.count(), 2);

    held_first
        .reply
        .send(Action::Reply)
        .unwrap_or_else(|_| panic!("first query canceled"));
    success(first, 41).await;
}

#[tokio::test]
async fn expired_idle_connections_are_replaced_on_next_borrow() {
    let cert = Certificate::new();
    let mut peer = Peer::new(&cert).await;
    let settings = PoolSettings {
        idle_timeout_ms: 25,
        ..pool(1)
    };
    let upstream = Upstream::with_pool(&cert.settings(), &settings).unwrap();
    let first = exchange(&upstream, peer.address, 50);
    peer.reply(Action::Reply).await;
    success(first, 50).await;
    sleep(Duration::from_millis(50)).await;
    let second = exchange(&upstream, peer.address, 51);
    assert_eq!(peer.reply(Action::Reply).await, 2);
    success(second, 51).await;
    assert_eq!(peer.count(), 2);
}

#[tokio::test]
async fn cancellation_discards_borrowed_connection_including_partial_response() {
    let cert = Certificate::new();
    for partial in [false, true] {
        let mut peer = Peer::new(&cert).await;
        let upstream = cert.upstream(1);
        let warm = exchange(&upstream, peer.address, 60);
        peer.reply(Action::Reply).await;
        success(warm, 60).await;
        let canceled = exchange(&upstream, peer.address, 61);
        let request = peer.request().await;
        assert_eq!(request.connection, 1);
        if partial {
            let (sent, received) = oneshot::channel();
            request
                .reply
                .send(Action::Partial(sent))
                .unwrap_or_else(|_| panic!("query canceled"));
            timeout(DEADLINE, received).await.unwrap().unwrap();
        } else {
            // Keep the server alive until the client has abandoned the outstanding reply.
            canceled.abort();
            assert!(canceled.await.unwrap_err().is_cancelled());
            drop(request);
            let next = exchange(&upstream, peer.address, 62);
            assert_eq!(peer.reply(Action::Reply).await, 2);
            success(next, 62).await;
            continue;
        }
        canceled.abort();
        assert!(canceled.await.unwrap_err().is_cancelled());
        let next = exchange(&upstream, peer.address, 62);
        assert_eq!(peer.reply(Action::Reply).await, 2);
        success(next, 62).await;
    }
}

#[tokio::test]
async fn idle_peer_close_reconnects_once_but_invalid_dns_never_retries() {
    let cert = Certificate::new();
    let mut peer = Peer::new(&cert).await;
    let upstream = cert.upstream(1);
    let first = exchange(&upstream, peer.address, 70);
    let (closed, received) = oneshot::channel();
    peer.reply(Action::CloseAfterReply(closed)).await;
    timeout(DEADLINE, received).await.unwrap().unwrap();
    success(first, 70).await;
    let second = exchange(&upstream, peer.address, 71);
    assert_eq!(peer.reply(Action::Reply).await, 2);
    success(second, 71).await;

    for (id, action) in [(72, Action::WrongId), (74, Action::Truncated)] {
        let bad = exchange(&upstream, peer.address, id);
        peer.reply(action).await;
        assert!(timeout(DEADLINE, bad).await.unwrap().unwrap().is_err());
        assert!(peer.requests.try_recv().is_err(), "invalid DNS was retried");
        let previous = peer.count();
        let next = exchange(&upstream, peer.address, id + 1);
        peer.reply(Action::Reply).await;
        success(next, id + 1).await;
        assert_eq!(peer.count(), previous + 1, "invalid stream was retained");
    }
}

#[tokio::test]
async fn pooled_connections_are_not_reused_for_a_different_endpoint() {
    let cert = Certificate::new();
    let mut first = Peer::new(&cert).await;
    let mut second = Peer::new(&cert).await;
    let upstream = cert.upstream(1);
    for id in [80, 81, 82] {
        let peer = if id == 81 { &mut second } else { &mut first };
        let task = exchange(&upstream, peer.address, id);
        peer.reply(Action::Reply).await;
        success(task, id).await;
    }
    assert_eq!(first.count(), 2);
    assert_eq!(second.count(), 1);
}

#[tokio::test]
async fn pooling_preserves_certificate_authentication_without_retries() {
    let cert = Certificate::new();
    for wrong_name in [false, true] {
        let mut peer = Peer::new(&cert).await;
        let mut settings = cert.settings();
        if wrong_name {
            settings.server_name = "wrong.example.test".into();
        } else {
            settings.ca_file = None;
        }
        let upstream = Upstream::with_pool(&settings, &pool(1)).unwrap();
        let task = exchange(&upstream, peer.address, 90);
        assert!(timeout(DEADLINE, task).await.unwrap().unwrap().is_err());
        assert_eq!(peer.count(), 1);
        assert!(peer.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn resolver_query_deadline_includes_pool_wait_without_opening_an_extra_connection() {
    let cert = Certificate::new();
    let mut peer = Peer::new(&cert).await;
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.upstreams = None;
    config.upstream = Some(peer.address);
    config.upstream_tls = Some(cert.settings());
    config.upstream_pool = pool(1);
    config.query_timeout_ms = 100;
    config.cache.enabled = false;
    config.coalescing.enabled = false;
    let resolver = Resolver::try_from_config(&config).unwrap();
    let client = "192.0.2.1".parse().unwrap();
    let first_wire = query(100).to_vec().unwrap();
    let mut held = Box::pin(resolver.resolve(&first_wire, client));
    let request = tokio::select! {
        request = peer.request() => request,
        _ = &mut held => panic!("first request completed without a response"),
    };
    // Do not poll the holder while the second future runs: its borrowed stream
    // stays busy even after its own deadline is ready. This tests the waiting
    // caller's deadline independently of the holder's timeout/cancellation.
    let response = timeout(
        DEADLINE,
        resolver.resolve(&query(101).to_vec().unwrap(), client),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.message.response_code, ResponseCode::ServFail);
    assert_eq!(response.message.id, 101);
    assert_eq!(peer.count(), 1);
    assert!(peer.requests.try_recv().is_err());
    drop(held);
    drop(request);

    let next_wire = query(102).to_vec().unwrap();
    let mut next = Box::pin(resolver.resolve(&next_wire, client));
    let request = tokio::select! {
        request = peer.request() => request,
        _ = &mut next => panic!("replacement request completed without a response"),
    };
    assert_eq!(request.connection, 2);
    request
        .reply
        .send(Action::Reply)
        .unwrap_or_else(|_| panic!("query canceled"));
    let response = timeout(DEADLINE, next).await.unwrap().unwrap();
    assert_eq!(response.message.response_code, ResponseCode::NoError);
    assert_eq!(response.message.id, 102);
}
