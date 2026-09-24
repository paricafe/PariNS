//! A single upstream transaction. The caller owns the overall deadline.

use std::net::SocketAddr;

use anyhow::{Result, ensure};
use hickory_proto::op::Message;
use tokio::net::{TcpStream, UdpSocket};

use crate::protocol::{self, MAX_MESSAGE};
use crate::transport::tcp;
use crate::upstreams::diagnostics::{ActualProtocol, Attempt, AttemptScope, Stage};

fn stage(attempt: &mut Option<Attempt>, stage: Stage) {
    if let Some(attempt) = attempt {
        attempt.stage(stage);
    }
}

pub(crate) async fn exchange_tcp_observed(
    query: &Message,
    address: SocketAddr,
    scope: Option<&AttemptScope>,
) -> Result<Message> {
    exchange_tcp_inner(query, address, scope, true).await
}

async fn exchange_tcp_inner(
    query: &Message,
    address: SocketAddr,
    scope: Option<&AttemptScope>,
    randomize_id: bool,
) -> Result<Message> {
    let mut attempt = scope.map(|s| s.start(Some(ActualProtocol::Tcp)));
    let result = async {
        stage(&mut attempt, Stage::Connect);
        let mut stream = TcpStream::connect(address).await?;
        let mut outbound = query.clone();
        if randomize_id {
            outbound.metadata.id = rand::random();
        }
        stage(&mut attempt, Stage::RequestWrite);
        tcp::write_frame(
            &mut stream,
            &protocol::encode_hop(&outbound, false, false, 128, MAX_MESSAGE)?,
        )
        .await?;
        stage(&mut attempt, Stage::ResponseRead);
        let wire = tcp::read_frame(&mut stream).await?;
        stage(&mut attempt, Stage::Decode);
        let mut response = protocol::decode(&wire)?;
        stage(&mut attempt, Stage::Validate);
        ensure!(
            protocol::matches_response(&outbound, &response) && !response.truncation,
            "invalid upstream TCP response"
        );
        response.metadata.id = query.id;
        Ok(response)
    }
    .await;
    if let Some(attempt) = &mut attempt {
        attempt.finish(&result);
    }
    result
}

pub async fn exchange(query: &Message, address: SocketAddr) -> Result<Message> {
    exchange_observed(query, address, None).await
}

pub(crate) async fn exchange_observed(
    query: &Message,
    address: SocketAddr,
    scope: Option<&AttemptScope>,
) -> Result<Message> {
    let mut attempt = scope.map(|s| s.start(Some(ActualProtocol::Udp)));
    let result = async {
        stage(&mut attempt, Stage::Connect);
        let bind = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(address).await?;
        let mut outbound = query.clone();
        outbound.metadata.id = rand::random();
        let wire = protocol::encode_hop(&outbound, false, false, 128, MAX_MESSAGE)?;
        stage(&mut attempt, Stage::RequestWrite);
        socket.send(&wire).await?;
        let mut buffer = vec![0; MAX_MESSAGE];
        loop {
            stage(&mut attempt, Stage::ResponseRead);
            let length = socket.recv(&mut buffer).await?;
            stage(&mut attempt, Stage::Decode);
            let Ok(mut response) = protocol::decode(&buffer[..length]) else {
                continue;
            };
            stage(&mut attempt, Stage::Validate);
            if !protocol::matches_response(&outbound, &response) {
                continue;
            }
            if response.truncation {
                if let Some(attempt) = &mut attempt {
                    attempt.finish(&Ok::<_, anyhow::Error>(()));
                }
                ensure!(
                    scope.is_none_or(AttemptScope::remaining),
                    "upstream deadline exhausted before TCP"
                );
                response = exchange_tcp_inner(&outbound, address, scope, false).await?;
            }
            response.metadata.id = query.id;
            return Ok(response);
        }
    }
    .await;
    if let Some(attempt) = &mut attempt {
        attempt.finish(&result);
    }
    result
}
