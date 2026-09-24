use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{
    config::Config,
    upstreams::{
        Mode, Pool, Settings,
        diagnostics::{ActualProtocol, Operation, Outcome, Reason, Stage},
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    time::Instant,
};

fn query() -> Message {
    let mut message = Message::new(321, MessageType::Query, OpCode::Query);
    message.add_query(Query::query(
        Name::from_ascii("diagnostics.test.").unwrap(),
        RecordType::A,
    ));
    message
}
fn pool(servers: Vec<String>, mode: Mode, bootstrap: Vec<std::net::SocketAddr>) -> Pool {
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.listen = "127.0.0.1:1053".parse().unwrap();
    config.upstreams = Settings {
        servers,
        mode,
        bootstrap,
        ..Settings::default()
    };
    Pool::new(&config.upstreams, &config).unwrap()
}
#[tokio::test]
async fn absolute_deadline_retains_current_stage_and_exhaustion_sends_nothing() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pool = pool(
        vec![socket.local_addr().unwrap().to_string()],
        Mode::Weighted,
        vec![],
    );
    let operation = Operation::new(true, None);
    let start = Instant::now();
    assert!(
        pool.exchange_observed(&query(), start + Duration::from_millis(40), &operation)
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 1);
    let attempt = &trace.attempts[0];
    assert_eq!(attempt.protocol, Some(ActualProtocol::Udp));
    assert_eq!(attempt.stage, Stage::ResponseRead);
    assert_eq!(attempt.outcome, Outcome::Failed);
    assert_eq!(attempt.reason, Some(Reason::Deadline));
    socket.recv_from(&mut [0; 4096]).await.unwrap();
    let exhausted = Operation::new(true, None);
    assert!(
        pool.exchange_observed(&query(), Instant::now(), &exhausted)
            .await
            .is_err()
    );
    assert!(exhausted.trace().unwrap().attempts.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), socket.recv_from(&mut [0; 4096]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bootstrap_owns_remaining_budget_and_cannot_start_endpoint_after_it_expires() {
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pool = pool(
        vec!["https://diagnostics.test".into()],
        Mode::Weighted,
        vec![bootstrap.local_addr().unwrap()],
    );
    let operation = Operation::new(true, None);
    let start = Instant::now();
    assert!(
        pool.exchange_observed(&query(), start + Duration::from_millis(35), &operation)
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 1);
    assert_eq!(trace.attempts[0].protocol, None);
    assert_eq!(trace.attempts[0].stage, Stage::Bootstrap);
    assert_eq!(trace.attempts[0].reason, Some(Reason::Deadline));
}

#[tokio::test]
async fn parallel_winner_cancels_loser_without_counting_a_failure() {
    let winner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let loser = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pool = pool(
        vec![
            winner.local_addr().unwrap().to_string(),
            loser.local_addr().unwrap().to_string(),
        ],
        Mode::Parallel,
        vec![],
    );
    let server = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        let (len, peer) = winner.recv_from(&mut bytes).await.unwrap();
        let mut response = Message::from_vec(&bytes[..len]).unwrap();
        response.metadata.message_type = MessageType::Response;
        winner
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    let counted = Arc::new(AtomicUsize::new(0));
    let observed = counted.clone();
    let operation = Operation::new(
        true,
        Some(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        })),
    );
    assert!(
        pool.exchange_observed(
            &query(),
            Instant::now() + Duration::from_secs(1),
            &operation
        )
        .await
        .is_ok()
    );
    server.await.unwrap();
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 2);
    assert_eq!(counted.load(Ordering::Relaxed), 2);
    assert_eq!(
        trace
            .attempts
            .iter()
            .filter(|a| a.outcome == Outcome::Succeeded)
            .count(),
        1
    );
    assert_eq!(trace.attempts.iter().filter(|a|a.outcome==Outcome::Cancelled && a.reason==Some(Reason::CallerCancelled)).count(),1);
    assert!(trace.attempts.iter().all(|a| a.outcome != Outcome::Failed));
}

#[tokio::test]
async fn shutdown_abort_is_terminal_once_and_disabled_trace_keeps_aggregates() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pool = Arc::new(pool(
        vec![socket.local_addr().unwrap().to_string()],
        Mode::Weighted,
        vec![],
    ));
    let operation = Operation::new(false, None);
    let worker_pool = pool.clone();
    let worker_operation = operation.clone();
    let task = tokio::spawn(async move {
        worker_pool
            .exchange_observed(
                &query(),
                Instant::now() + Duration::from_secs(1),
                &worker_operation,
            )
            .await
    });
    socket.recv_from(&mut [0; 4096]).await.unwrap();
    pool.shutdown();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(operation.trace().is_none());
    let counts = pool.diagnostics_snapshot();
    let rows = counts["slots"][0]["counts"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["outcome"], "cancelled");
    assert_eq!(rows[0]["reason"], "shutdown");
    assert_eq!(rows[0]["count"], 1);
}

#[tokio::test]
async fn valid_servfail_is_transport_success_and_bad_tcp_wire_is_decode_failure() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let good = pool(
        vec![socket.local_addr().unwrap().to_string()],
        Mode::Weighted,
        vec![],
    );
    let server = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        let (len, peer) = socket.recv_from(&mut bytes).await.unwrap();
        let mut response = Message::from_vec(&bytes[..len]).unwrap();
        response.metadata.message_type = MessageType::Response;
        response.metadata.response_code = ResponseCode::ServFail;
        socket
            .send_to(&response.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    let operation = Operation::new(true, None);
    let result = good
        .exchange_observed(
            &query(),
            Instant::now() + Duration::from_secs(1),
            &operation,
        )
        .await
        .unwrap();
    assert_eq!(result.message.response_code, ResponseCode::ServFail);
    assert_eq!(
        operation.trace().unwrap().attempts[0].outcome,
        Outcome::Succeeded
    );
    server.await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad = pool(
        vec![format!("tcp://{}", listener.local_addr().unwrap())],
        Mode::Weighted,
        vec![],
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let n = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; n as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        stream.write_u16(12).await.unwrap();
        stream.write_all(&[255; 12]).await.unwrap();
    });
    let operation = Operation::new(true, None);
    assert!(
        bad.exchange_observed(
            &query(),
            Instant::now() + Duration::from_secs(1),
            &operation
        )
        .await
        .is_err()
    );
    server.await.unwrap();
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts[0].stage, Stage::Decode);
    assert_eq!(trace.attempts[0].reason, Some(Reason::ProtocolInvalid));
}

#[tokio::test]
async fn multiple_bootstrap_addresses_create_separate_protocol_attempts() {
    use hickory_proto::rr::{
        RData, Record,
        rdata::{A, AAAA},
    };
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let bootstrap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pool = pool(
        vec![format!(
            "tcp://diagnostics.test:{}",
            listener.local_addr().unwrap().port()
        )],
        Mode::Weighted,
        vec![bootstrap.local_addr().unwrap()],
    );
    let bootstrap_task = tokio::spawn(async move {
        for _ in 0..2 {
            let mut bytes = [0; 4096];
            let (len, peer) = bootstrap.recv_from(&mut bytes).await.unwrap();
            let mut response = Message::from_vec(&bytes[..len]).unwrap();
            response.metadata.message_type = MessageType::Response;
            if response.queries[0].query_type() == RecordType::A {
                response.add_answer(Record::from_rdata(
                    response.queries[0].name().clone(),
                    60,
                    RData::A(A::new(127, 0, 0, 1)),
                ));
            } else {
                response.add_answer(Record::from_rdata(
                    response.queries[0].name().clone(),
                    60,
                    RData::AAAA(AAAA(std::net::Ipv6Addr::LOCALHOST)),
                ));
            }
            bootstrap
                .send_to(&response.to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let n = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; n as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let mut response = Message::from_vec(&bytes).unwrap();
        response.metadata.message_type = MessageType::Response;
        let wire = response.to_vec().unwrap();
        stream.write_u16(wire.len() as u16).await.unwrap();
        stream.write_all(&wire).await.unwrap();
    });
    let operation = Operation::new(true, None);
    assert!(
        pool.exchange_observed(
            &query(),
            Instant::now() + Duration::from_secs(1),
            &operation
        )
        .await
        .is_ok()
    );
    server.await.unwrap();
    bootstrap_task.await.unwrap();
    let trace = operation.trace().unwrap();
    assert_eq!(trace.attempts.len(), 3);
    assert_eq!(trace.attempts[0].protocol, None);
    assert_eq!(trace.attempts[1].stage, Stage::Connect);
    assert_eq!(trace.attempts[1].reason, Some(Reason::ConnectIo));
    assert_eq!(trace.attempts[2].outcome, Outcome::Succeeded);
    assert!(trace.attempts.iter().all(|a| a.slot == 0));
}
