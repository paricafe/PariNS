use std::{net::SocketAddr, time::Duration};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RecordType},
};
use parins::{
    protocol,
    scheduler::{Client, Settings},
};
use tokio::{
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
    time::timeout,
};

struct Mock(UdpSocket);

impl Mock {
    async fn new() -> Self {
        Self(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }
    fn address(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }
    async fn receive(&self) -> (Message, SocketAddr) {
        let mut buffer = [0; 65535];
        let (length, peer) = timeout(Duration::from_secs(2), self.0.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        (Message::from_vec(&buffer[..length]).unwrap(), peer)
    }
    async fn reply(&self, query: &Message, peer: SocketAddr, code: ResponseCode) {
        let response = protocol::error_response(query, code).to_vec().unwrap();
        self.0.send_to(&response, peer).await.unwrap();
    }
    async fn quiet(&self) {
        assert!(
            timeout(Duration::from_millis(30), self.0.recv_from(&mut [0; 65535]))
                .await
                .is_err()
        );
    }
}

fn query(name: &str) -> Message {
    let mut message = Message::new(42, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(Name::from_ascii(name).unwrap(), RecordType::A));
    message
}

fn client(primary: &Mock, secondary: &Mock, delay: u64, budget: usize) -> Client {
    Client::new(
        primary.address(),
        None,
        Some(Settings {
            secondary: secondary.address(),
            hedge_after_ms: delay,
            max_extra_inflight: budget,
        }),
    )
    .unwrap()
}

fn exchange(client: Client, name: &'static str) -> JoinHandle<anyhow::Result<Message>> {
    tokio::spawn(async move { client.exchange(&query(name)).await })
}

async fn result(task: JoinHandle<anyhow::Result<Message>>) -> Message {
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn disabled_scheduler_and_fast_primary_never_fan_out() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    for client in [
        Client::new(primary.address(), None, None).unwrap(),
        client(&primary, &secondary, 1000, 1),
    ] {
        let task = exchange(client, "example.test.");
        let (query, peer) = primary.receive().await;
        primary.reply(&query, peer, ResponseCode::NoError).await;
        assert_eq!(result(task).await.response_code, ResponseCode::NoError);
        secondary.quiet().await;
    }
}

#[tokio::test]
async fn primary_negative_and_refused_are_authoritative_results_not_failures() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    let client = client(&primary, &secondary, 1000, 1);
    for code in [
        ResponseCode::NXDomain,
        ResponseCode::NoError,
        ResponseCode::Refused,
    ] {
        let task = exchange(client.clone(), "negative.test.");
        let (query, peer) = primary.receive().await;
        primary.reply(&query, peer, code).await;
        assert_eq!(result(task).await.response_code, code);
        secondary.quiet().await;
    }
}

#[tokio::test]
async fn timer_hedge_can_win_with_nxdomain_and_cancels_primary_socket() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    let task = exchange(client(&primary, &secondary, 10, 1), "hedge.test.");
    let (_, primary_peer) = primary.receive().await;
    let (query, secondary_peer) = secondary.receive().await;
    secondary
        .reply(&query, secondary_peer, ResponseCode::NXDomain)
        .await;
    let response = result(task).await;
    assert_eq!(response.response_code, ResponseCode::NXDomain);
    assert_eq!(response.id, 42);
    // The losing UDP socket is no longer owned by any detached operation.
    let _released = UdpSocket::bind(primary_peer).await.unwrap();
}

#[tokio::test]
async fn primary_servfail_and_io_error_start_secondary_without_waiting_for_hedge_timer() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    for io_error in [false, true] {
        let task = exchange(client(&primary, &secondary, 60_000, 1), "retry.test.");
        let (query, peer) = primary.receive().await;
        if io_error {
            // A truncated UDP answer requires TCP; this verified-unused port
            // produces a real local connect error rather than a DNS failure.
            let listener = TcpListener::bind(primary.address()).await.unwrap();
            drop(listener);
            let mut truncated = protocol::error_response(&query, ResponseCode::NoError);
            truncated.metadata.truncation = true;
            primary
                .0
                .send_to(&truncated.to_vec().unwrap(), peer)
                .await
                .unwrap();
        } else {
            primary.reply(&query, peer, ResponseCode::ServFail).await;
        }
        let (query, peer) = secondary.receive().await;
        secondary.reply(&query, peer, ResponseCode::NoError).await;
        assert_eq!(result(task).await.response_code, ResponseCode::NoError);
    }
}

#[tokio::test]
async fn failed_hedge_does_not_preempt_primary_and_primary_winner_cancels_secondary() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    for secondary_fails in [false, true] {
        let task = exchange(client(&primary, &secondary, 10, 1), "primary.test.");
        let (primary_query, primary_peer) = primary.receive().await;
        let (secondary_query, secondary_peer) = secondary.receive().await;
        if secondary_fails {
            secondary
                .reply(&secondary_query, secondary_peer, ResponseCode::ServFail)
                .await;
        }
        primary
            .reply(&primary_query, primary_peer, ResponseCode::NoError)
            .await;
        assert_eq!(result(task).await.response_code, ResponseCode::NoError);
        let _released = UdpSocket::bind(secondary_peer).await.unwrap();
    }
}

#[tokio::test]
async fn both_servfail_keep_primary_response() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    let task = exchange(client(&primary, &secondary, 1000, 1), "failure.test.");
    let (query, peer) = primary.receive().await;
    primary.reply(&query, peer, ResponseCode::ServFail).await;
    let (query, peer) = secondary.receive().await;
    secondary.reply(&query, peer, ResponseCode::ServFail).await;
    assert_eq!(result(task).await.response_code, ResponseCode::ServFail);
}

#[tokio::test]
async fn hedge_budget_is_global_across_clones_and_released_on_cancellation() {
    let primary = Mock::new().await;
    let secondary = Mock::new().await;
    let client = client(&primary, &secondary, 10, 1);
    let one = exchange(client.clone(), "one.test.");
    let two = exchange(client.clone(), "two.test.");
    let (_, first_peer) = primary.receive().await;
    let (_, second_peer) = primary.receive().await;
    let (_, hedge_peer) = secondary.receive().await;
    secondary.quiet().await;
    one.abort();
    two.abort();
    assert!(one.await.unwrap_err().is_cancelled());
    assert!(two.await.unwrap_err().is_cancelled());
    let _released = [
        UdpSocket::bind(first_peer).await.unwrap(),
        UdpSocket::bind(second_peer).await.unwrap(),
        UdpSocket::bind(hedge_peer).await.unwrap(),
    ];
    let third = exchange(client, "three.test.");
    primary.receive().await;
    let (query, peer) = secondary.receive().await;
    secondary.reply(&query, peer, ResponseCode::NoError).await;
    assert_eq!(result(third).await.response_code, ResponseCode::NoError);
}

#[test]
fn replica_settings_validate_addresses_and_finite_limits() {
    let primary = "127.0.0.1:53".parse().unwrap();
    let valid = Settings {
        secondary: "127.0.0.2:53".parse().unwrap(),
        hedge_after_ms: 10,
        max_extra_inflight: 1,
    };
    assert!(valid.validate(primary).is_ok());
    for address in [
        "127.0.0.1:53",
        "[::ffff:127.0.0.1]:53",
        "127.0.0.2:0",
        "0.0.0.0:53",
        "224.0.0.1:53",
        "255.255.255.255:53",
        "[::ffff:224.0.0.1]:53",
    ] {
        assert!(
            Settings {
                secondary: address.parse().unwrap(),
                ..valid.clone()
            }
            .validate(primary)
            .is_err()
        );
    }
    for delay in [0, 60_001] {
        assert!(
            Settings {
                hedge_after_ms: delay,
                ..valid.clone()
            }
            .validate(primary)
            .is_err()
        );
    }
    for budget in [0, 65_537] {
        assert!(
            Settings {
                max_extra_inflight: budget,
                ..valid.clone()
            }
            .validate(primary)
            .is_err()
        );
    }
}
