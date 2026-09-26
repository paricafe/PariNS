//! DNS datagrams retain their local destination at the listener boundary.
use std::{io, net::SocketAddr};

use tokio::net::UdpSocket;

pub(crate) struct Socket(UdpSocket);

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod packet_info;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use packet_info::ReplySource;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) struct ReplySource;

impl Socket {
    pub(crate) async fn bind(address: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(address).await?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        packet_info::enable(&socket, address)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if address.ip().is_unspecified() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "wildcard DNS UDP requires packet-info support; bind a specific local address",
            ));
        }
        Ok(Self(socket))
    }

    /// None is a consumed datagram with truncated payload/control information.
    pub(crate) async fn recv(
        &self,
        bytes: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr, ReplySource)>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            packet_info::recv(&self.0, bytes).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let (length, peer) = self.0.recv_from(bytes).await?;
            Ok(Some((length, peer, ReplySource)))
        }
    }

    pub(crate) async fn reply(
        &self,
        bytes: &[u8],
        peer: SocketAddr,
        source: ReplySource,
    ) -> io::Result<usize> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            packet_info::send(&self.0, bytes, peer, source).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = source;
            self.0.send_to(bytes, peer).await
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn truncated_datagrams_and_cancelled_reads_do_not_corrupt_the_next_reply() {
        for (bind, client_bind, ip) in [
            ("0.0.0.0:0", "127.0.0.1:0", "127.0.0.1"),
            ("[::]:0", "[::1]:0", "::1"),
        ] {
            let socket = Socket::bind(bind.parse().unwrap()).await.unwrap();
            let target =
                SocketAddr::new(ip.parse().unwrap(), socket.0.local_addr().unwrap().port());
            let client = UdpSocket::bind(client_bind).await.unwrap();
            let mut bytes = [0; 4];
            assert!(
                timeout(Duration::from_millis(10), socket.recv(&mut bytes))
                    .await
                    .is_err()
            );
            client.send_to(b"too long", target).await.unwrap();
            assert!(
                timeout(Duration::from_secs(1), socket.recv(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            client.send_to(b"next", target).await.unwrap();
            let (length, peer, source) = timeout(Duration::from_secs(1), socket.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(&bytes[..length], b"next");
            socket.reply(&bytes[..length], peer, source).await.unwrap();
            let (length, source) = timeout(Duration::from_secs(1), client.recv_from(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(source, target);
            assert_eq!(&bytes[..length], b"next");
        }
    }
}
