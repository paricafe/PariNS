use std::{
    io::{self, IoSlice, IoSliceMut},
    net::SocketAddr,
    os::fd::AsRawFd,
};

use nix::{
    libc::{cmsghdr, in_pktinfo, in6_pktinfo},
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, SockaddrStorage, cmsg_space, recvmsg,
        sendmsg, setsockopt, sockopt,
    },
};
use tokio::{io::Interest, net::UdpSocket};

pub(crate) enum ReplySource {
    V4(in_pktinfo),
    V6(in6_pktinfo),
}

// Native ancillary alignment, fixed stack capacity: no per-packet heap buffer.
#[repr(C)]
struct ControlBuffer {
    alignment: [cmsghdr; 0],
    bytes: [u8; cmsg_space::<in_pktinfo>() + cmsg_space::<in6_pktinfo>()],
}

pub(super) fn enable(socket: &UdpSocket, address: SocketAddr) -> io::Result<()> {
    if address.is_ipv6() {
        setsockopt(socket, sockopt::Ipv6RecvPacketInfo, &true)?;
        // Dual-stack sockets report mapped destinations through IPV6_PKTINFO;
        // macOS rejects IP_PKTINFO on an AF_INET6 socket.
    } else {
        setsockopt(socket, sockopt::Ipv4PacketInfo, &true)?;
    }
    Ok(())
}

pub(super) async fn recv(
    socket: &UdpSocket,
    bytes: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr, ReplySource)>> {
    socket
        .async_io(Interest::READABLE, || {
            let mut control = ControlBuffer {
                alignment: [],
                bytes: [0; cmsg_space::<in_pktinfo>() + cmsg_space::<in6_pktinfo>()],
            };
            let mut buffers = [IoSliceMut::new(bytes)];
            let message = recvmsg::<SockaddrStorage>(
                socket.as_raw_fd(),
                &mut buffers,
                Some(&mut control.bytes),
                MsgFlags::empty(),
            )?;
            if message
                .flags
                .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
            {
                return Ok(None);
            }
            let peer = message
                .address
                .and_then(|address| {
                    address
                        .as_sockaddr_in()
                        .map(|address| SocketAddr::V4((*address).into()))
                        .or_else(|| {
                            address
                                .as_sockaddr_in6()
                                .map(|address| SocketAddr::V6((*address).into()))
                        })
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "UDP peer address missing")
                })?;
            let mut source = None;
            for control in message.cmsgs()? {
                match control {
                    ControlMessageOwned::Ipv4PacketInfo(mut info) => {
                        // Receive ipi_addr is the actual header destination;
                        // send ipi_spec_dst explicitly selects the reply source.
                        info.ipi_spec_dst = info.ipi_addr;
                        source = Some(ReplySource::V4(info));
                    }
                    ControlMessageOwned::Ipv6PacketInfo(info) => {
                        // Preserve both address and interface (including scopes).
                        source = Some(ReplySource::V6(info));
                    }
                    _ => {}
                }
            }
            Ok(source.map(|source| (message.bytes, peer, source)))
        })
        .await
}

pub(super) async fn send(
    socket: &UdpSocket,
    bytes: &[u8],
    peer: SocketAddr,
    source: ReplySource,
) -> io::Result<usize> {
    let destination = SockaddrStorage::from(peer);
    let control = match &source {
        ReplySource::V4(info) => ControlMessage::Ipv4PacketInfo(info),
        ReplySource::V6(info) => ControlMessage::Ipv6PacketInfo(info),
    };
    socket
        .async_io(Interest::WRITABLE, || {
            sendmsg(
                socket.as_raw_fd(),
                &[IoSlice::new(bytes)],
                &[control],
                MsgFlags::empty(),
                Some(&destination),
            )
            .map_err(Into::into)
        })
        .await
}
