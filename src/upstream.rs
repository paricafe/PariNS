//! A single upstream transaction. The caller owns the overall deadline.

use std::net::SocketAddr;

use anyhow::{Result, bail};
use hickory_proto::op::Message;
use tokio::net::UdpSocket;

use crate::protocol::{self, MAX_MESSAGE};

pub async fn exchange(query: &Message, address: SocketAddr) -> Result<Message> {
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(address).await?;
    let mut outbound = query.clone();
    outbound.metadata.id = rand::random();
    socket.send(&outbound.to_vec()?).await?;
    let mut buffer = vec![0; MAX_MESSAGE];
    loop {
        let length = socket.recv(&mut buffer).await?;
        let Ok(mut response) = protocol::decode(&buffer[..length]) else {
            continue;
        };
        if !protocol::matches_response(&outbound, &response) {
            continue;
        }
        if response.truncation {
            bail!("upstream requires TCP (not implemented yet)");
        }
        response.metadata.id = query.id;
        return Ok(response);
    }
}
