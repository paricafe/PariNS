use std::time::Duration;

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{protocol, resolver::Resolver};
use tokio::net::UdpSocket;

fn query(name: &str) -> Message {
    let mut query = Message::new(42, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    query
}

fn answer(query: &Message) -> Message {
    let mut response = protocol::error_response(query, ResponseCode::NoError);
    response.metadata.authentic_data = true;
    response.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    response
}

#[tokio::test]
async fn ignores_unrelated_responses_and_restores_client_id() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::new(upstream.local_addr().unwrap(), Duration::from_secs(1));
    let task = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
        let query = protocol::decode(&buffer[..length]).unwrap();
        let valid = answer(&query);
        let mut invalid = valid.clone();
        invalid.metadata.id = invalid.id.wrapping_add(1);
        upstream
            .send_to(&invalid.to_vec().unwrap(), peer)
            .await
            .unwrap();
        invalid = valid.clone();
        invalid.queries[0].set_name(Name::from_ascii("wrong.test.").unwrap());
        upstream
            .send_to(&invalid.to_vec().unwrap(), peer)
            .await
            .unwrap();
        // A matching answer from the wrong socket must also be ignored.
        let rogue = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut forged = valid.clone();
        forged.metadata.response_code = ResponseCode::NXDomain;
        rogue
            .send_to(&forged.to_vec().unwrap(), peer)
            .await
            .unwrap();
        upstream
            .send_to(&valid.to_vec().unwrap(), peer)
            .await
            .unwrap();
    });
    let query = query("example.test.");
    let reply = resolver.resolve(&query.to_vec().unwrap()).await.unwrap();
    assert_eq!(reply.message.id, 42);
    assert_eq!(reply.message.response_code, ResponseCode::NoError);
    assert_eq!(reply.message.answers.len(), 1);
    assert!(!reply.message.authentic_data);
    task.await.unwrap();
}

#[tokio::test]
async fn silent_upstream_returns_servfail_within_deadline() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::new(upstream.local_addr().unwrap(), Duration::from_millis(40));
    let request = query("timeout.test.").to_vec().unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(1), resolver.resolve(&request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.message.response_code, ResponseCode::ServFail);
    assert_eq!(reply.message.id, 42);
}

#[tokio::test]
async fn concurrent_identical_ids_do_not_cross_queries() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let resolver = Resolver::new(upstream.local_addr().unwrap(), Duration::from_secs(1));
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut buffer = [0; 4096];
        for _ in 0..2 {
            let (length, peer) = upstream.recv_from(&mut buffer).await.unwrap();
            requests.push((protocol::decode(&buffer[..length]).unwrap(), peer));
        }
        for (query, peer) in requests.into_iter().rev() {
            upstream
                .send_to(&answer(&query).to_vec().unwrap(), peer)
                .await
                .unwrap();
        }
    });
    let a = query("a.test.").to_vec().unwrap();
    let b = query("b.test.").to_vec().unwrap();
    let (a, b) = tokio::join!(resolver.resolve(&a), resolver.resolve(&b));
    assert_eq!(a.unwrap().message.answers[0].name.to_ascii(), "a.test.");
    assert_eq!(b.unwrap().message.answers[0].name.to_ascii(), "b.test.");
    task.await.unwrap();
}
