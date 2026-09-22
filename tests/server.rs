use std::{net::SocketAddr, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{config::Config, protocol, server::Server};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

fn query() -> Message {
    let mut query = Message::new(99, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query.add_query(Query::query(
        Name::from_ascii("example.test.").unwrap(),
        RecordType::A,
    ));
    query
}

fn answer(query: &Message, count: usize) -> Message {
    let mut response = protocol::error_response(query, ResponseCode::NoError);
    for _ in 0..count {
        response.add_answer(Record::from_rdata(
            query.queries[0].name().clone(),
            60,
            RData::A(A::new(192, 0, 2, 1)),
        ));
    }
    response
}

fn config(upstream: SocketAddr) -> Config {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.listen.set_port(0);
    config.upstream = upstream;
    config.query_timeout_ms = 200;
    config.tcp_io_timeout_ms = 100;
    config.shutdown_grace_ms = 300;
    config
}

// Encode/decode TCP frames independently of the production framing helpers.
async fn write(stream: &mut TcpStream, query: &Message) {
    let bytes = query.to_vec().unwrap();
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&bytes).await.unwrap();
}

async fn read(stream: &mut TcpStream) -> Message {
    timeout(Duration::from_secs(2), async {
        let length = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; length as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        protocol::decode(&bytes).unwrap()
    })
    .await
    .unwrap()
}

async fn udp_query(address: SocketAddr, query: &Message) -> (Message, usize) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query.to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 65535];
    let length = timeout(Duration::from_secs(2), socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    (protocol::decode(&buffer[..length]).unwrap(), length)
}

async fn start(
    config: Config,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<anyhow::Result<()>>,
) {
    let server = Server::bind(config).await.unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    (address, stop, task)
}

async fn finish(stop: oneshot::Sender<()>, task: JoinHandle<anyhow::Result<()>>) {
    stop.send(()).unwrap();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn udp_truncation_tcp_fallback_and_connection_reuse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let udp = UdpSocket::bind(upstream).await.unwrap();
    let mock = tokio::spawn(async move {
        for _ in 0..3 {
            let mut buffer = [0; 4096];
            let (length, peer) = udp.recv_from(&mut buffer).await.unwrap();
            let q = protocol::decode(&buffer[..length]).unwrap();
            let mut truncated = answer(&q, 0);
            truncated.metadata.truncation = true;
            udp.send_to(&truncated.to_vec().unwrap(), peer)
                .await
                .unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let tcp_query = read(&mut stream).await;
            assert_eq!(tcp_query.id, q.id);
            assert_eq!(tcp_query.queries, q.queries);
            write(&mut stream, &answer(&tcp_query, 100)).await;
        }
    });
    let (address, stop, task) = start(config(upstream)).await;
    let (reply, length) = udp_query(address, &query()).await;
    assert!(reply.truncation);
    assert_eq!(reply.id, 99);
    assert!(length <= 512);
    assert!(reply.answers.is_empty());
    let mut client = TcpStream::connect(address).await.unwrap();
    for id in [100, 101] {
        let mut q = query();
        q.metadata.id = id;
        write(&mut client, &q).await;
        let reply = read(&mut client).await;
        assert!(!reply.truncation);
        assert_eq!(reply.id, id);
        assert_eq!(reply.answers.len(), 100);
    }
    finish(stop, task).await;
    mock.await.unwrap();
    // Listeners and an idle reusable TCP client are closed on shutdown.
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let _udp = UdpSocket::bind(address).await.unwrap();
    let _tcp = TcpListener::bind(address).await.unwrap();
}

#[tokio::test]
async fn malformed_and_unsupported_requests_do_not_reach_upstream() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (address, stop, task) = start(config(upstream.local_addr().unwrap())).await;
    for (kind, expected) in [
        (RecordType::AXFR, ResponseCode::Refused),
        (RecordType::IXFR, ResponseCode::Refused),
        (RecordType::ANY, ResponseCode::Refused),
    ] {
        let mut q = query();
        q.queries[0].set_query_type(kind);
        assert_eq!(udp_query(address, &q).await.0.response_code, expected);
    }
    let mut client = TcpStream::connect(address).await.unwrap();
    let mut q = query();
    q.queries.clear();
    write(&mut client, &q).await;
    assert_eq!(read(&mut client).await.response_code, ResponseCode::FormErr);
    let mut buffer = [0; 512];
    assert!(
        matches!(upstream.try_recv(&mut buffer), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    );
    finish(stop, task).await;
}

#[tokio::test]
async fn partial_frame_and_connection_budget_are_bounded() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(upstream.local_addr().unwrap());
    cfg.max_tcp_connections = 1;
    cfg.tcp_io_timeout_ms = 200;
    let (address, stop, task) = start(cfg).await;
    let mut first = TcpStream::connect(address).await.unwrap();
    // Get a local response to prove this connection has been admitted.
    let mut q = query();
    q.queries[0].set_query_type(RecordType::AXFR);
    write(&mut first, &q).await;
    assert_eq!(read(&mut first).await.response_code, ResponseCode::Refused);
    let mut second = TcpStream::connect(address).await.unwrap();
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(1), second.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(closed, Ok(0) | Err(_)));
    // A partial length prefix must not keep the admitted connection alive forever.
    first.write_all(&[0]).await.unwrap();
    let closed = timeout(Duration::from_secs(1), first.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(closed, Ok(0) | Err(_)));
    finish(stop, task).await;
}

#[tokio::test]
async fn shutdown_drains_an_inflight_udp_query() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (address, stop, task) = start(config(upstream.local_addr().unwrap())).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&query().to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let (length, peer) = timeout(Duration::from_secs(1), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let q = protocol::decode(&buffer[..length]).unwrap();
    stop.send(()).unwrap();
    upstream
        .send_to(&answer(&q, 1).to_vec().unwrap(), peer)
        .await
        .unwrap();
    let length = timeout(Duration::from_secs(1), client.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        protocol::decode(&buffer[..length]).unwrap().answers.len(),
        1
    );
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn tcp_fallback_shares_the_udp_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let udp = UdpSocket::bind(upstream).await.unwrap();
    let (address, stop, task) = start(config(upstream)).await;
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = udp.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..length]).unwrap();
        tokio::time::sleep(Duration::from_millis(130)).await;
        let mut response = answer(&q, 0);
        response.metadata.truncation = true;
        udp.send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read(&mut stream).await;
        // After the original 200 ms deadline, but within a reset 200 ms timer.
        tokio::time::sleep(Duration::from_millis(130)).await;
        response.metadata.truncation = false;
        let bytes = response.to_vec().unwrap();
        let _ = stream.write_all(&(bytes.len() as u16).to_be_bytes()).await;
        let _ = stream.write_all(&bytes).await;
    });
    assert_eq!(
        udp_query(address, &query()).await.0.response_code,
        ResponseCode::ServFail
    );
    finish(stop, task).await;
    mock.await.unwrap();
}

#[tokio::test]
async fn udp_and_tcp_share_the_inflight_budget() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(upstream.local_addr().unwrap());
    cfg.max_inflight = 1;
    cfg.query_timeout_ms = 1000;
    let (address, stop, task) = start(cfg).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&query().to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    let (length, peer) = timeout(Duration::from_secs(1), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let pending = protocol::decode(&buffer[..length]).unwrap();
    let mut tcp = TcpStream::connect(address).await.unwrap();
    write(&mut tcp, &query()).await;
    assert_eq!(read(&mut tcp).await.response_code, ResponseCode::ServFail);
    assert!(
        matches!(upstream.try_recv(&mut buffer), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    );
    upstream
        .send_to(&answer(&pending, 1).to_vec().unwrap(), peer)
        .await
        .unwrap();
    timeout(Duration::from_secs(1), client.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    finish(stop, task).await;
}

#[tokio::test]
async fn shutdown_grace_cancels_stalled_queries() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(upstream.local_addr().unwrap());
    cfg.query_timeout_ms = 5000;
    cfg.shutdown_grace_ms = 30;
    let (address, stop, task) = start(cfg).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&query().to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut buffer = [0; 4096];
    timeout(Duration::from_secs(1), upstream.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    stop.send(()).unwrap();
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _udp = UdpSocket::bind(address).await.unwrap();
}

#[tokio::test]
async fn mismatched_tcp_fallback_response_returns_servfail() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let udp = UdpSocket::bind(upstream).await.unwrap();
    let mock = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = udp.recv_from(&mut buffer).await.unwrap();
        let q = protocol::decode(&buffer[..length]).unwrap();
        let mut truncated = answer(&q, 0);
        truncated.metadata.truncation = true;
        udp.send_to(&truncated.to_vec().unwrap(), peer)
            .await
            .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let q = read(&mut stream).await;
        let mut invalid = answer(&q, 1);
        invalid.metadata.id = q.id.wrapping_add(1);
        write(&mut stream, &invalid).await;
    });
    let (address, stop, task) = start(config(upstream)).await;
    assert_eq!(
        udp_query(address, &query()).await.0.response_code,
        ResponseCode::ServFail
    );
    finish(stop, task).await;
    mock.await.unwrap();
}
