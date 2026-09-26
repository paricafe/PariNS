//! Bounded DoQ and HTTP/3 adapters. DNS semantics stay in the shared ingress.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use bytes::{Buf, Bytes};
use quinn::{Connection, Endpoint, VarInt};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};

use crate::ingress::Ingress;
pub mod diagnostics;
mod lifecycle;
use diagnostics::{Event, Guard};
pub(crate) use lifecycle::ListenerRuntime;

pub struct Listener {
    socket: std::net::UdpSocket,
    config: quinn::ServerConfig,
    owner: lifecycle::Owner,
}

impl Listener {
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
    pub(crate) fn runtime(&self) -> Arc<ListenerRuntime> {
        self.owner.0.clone()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Doq,
    H3,
}

pub fn bind(
    address: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    max_streams: usize,
    protocol: Protocol,
) -> Result<Listener> {
    ensure!(
        (1..=1024).contains(&max_streams),
        "invalid QUIC stream limit"
    );
    let mut tls = (*tls).clone();
    tls.max_early_data_size = 0;
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    // No rebinding to a new peer identity after ECS provenance is established.
    config.migration(false);
    // Before the shared connection semaphore can admit handshakes, Quinn's
    // incoming queue and its buffered packets also need finite budgets.
    config.max_incoming(64);
    config.incoming_buffer_size(16 * 1024);
    config.incoming_buffer_size_total(1024 * 1024);
    let transport = Arc::get_mut(&mut config.transport).expect("unique transport config");
    transport.max_concurrent_bidi_streams(VarInt::from_u32(max_streams as u32));
    transport.max_concurrent_uni_streams(VarInt::from_u32(match protocol {
        Protocol::Doq => 0,
        Protocol::H3 => 3, // HTTP control + QPACK encoder/decoder
    }));
    transport.stream_receive_window(VarInt::from_u32(128 * 1024));
    transport.receive_window(VarInt::from_u32(1024 * 1024));
    transport.send_window(1024 * 1024);
    transport.datagram_receive_buffer_size(None);
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    // Defer background drivers until serve: failed or unused bind candidates
    // then release their sockets synchronously on drop.
    Ok(Listener {
        socket: std::net::UdpSocket::bind(address)?,
        config,
        owner: lifecycle::Owner(ListenerRuntime::new()),
    })
}

pub async fn serve(listener: Listener, protocol: Protocol, ingress: Ingress) -> Result<()> {
    let Listener {
        socket,
        config,
        owner,
    } = listener;
    let endpoint = Endpoint::new(Default::default(), Some(config), socket, owner.0.clone())?;
    let outcome = serve_endpoint(&endpoint, protocol, ingress).await;
    drop(endpoint);
    owner.0.shutdown().await;
    outcome
}

async fn serve_endpoint(endpoint: &Endpoint, protocol: Protocol, ingress: Ingress) -> Result<()> {
    let local_port = endpoint.local_addr()?.port();
    let mut stop = ingress.stop.clone();
    let mut connections = JoinSet::new();
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            _ = connections.join_next(), if !connections.is_empty() => {},
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                ingress.resolver.metrics().quic.inc(protocol,Event::Incoming);
                let Ok(permit) = ingress.connections.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(crate::metrics::Counter::ConnectionsRejected);
                    ingress.resolver.metrics().quic.inc(protocol,Event::AdmissionGlobalRejected);
                    incoming.refuse();
                    continue;
                };
                let Some(source) = ingress.admit_connection(incoming.remote_address().ip()) else {
                    ingress.resolver.metrics().quic.inc(protocol,Event::AdmissionSourceRejected);
                    ingress.resolver.metrics().inc(crate::metrics::Counter::ConnectionsRejected);
                    incoming.refuse();
                    continue;
                };
                let context = ingress.clone();
                let mut result = Guard::new(context.resolver.metrics().clone(),protocol,context.stop.clone(),false);
                connections.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    let connection = match timeout(context.io_timeout,incoming).await {
                        Ok(Ok(connection))=>{result.finish(Event::HandshakeEstablished);connection},
                        Ok(Err(error))=>{result.finish(diagnostics::handshake_error(&error));return;},
                        Err(_)=>{result.finish(Event::HandshakeApplicationDeadline);return;},
                    };
                    drop(result);
                    match protocol {
                        Protocol::Doq => doq_connection(connection, context).await,
                        Protocol::H3 => h3_connection(connection, context, local_port).await,
                    }
                });
            }
        }
    }
    let deadline = Instant::now() + ingress.shutdown_grace;
    drain(&mut connections, ingress.shutdown_grace, &ingress.resolver).await;
    endpoint.close(VarInt::from_u32(0), b"shutdown");
    // Notify peers only within the existing grace; owned drivers are then joined.
    let _ = timeout_at(deadline, endpoint.wait_idle()).await;
    Ok(())
}

async fn drain(tasks: &mut JoinSet<()>, grace: Duration, resolver: &crate::resolver::Resolver) {
    if timeout(grace, async { while tasks.join_next().await.is_some() {} })
        .await
        .is_err()
    {
        resolver.force_shutdown();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

async fn doq_connection(connection: Connection, ingress: Ingress) {
    let peer = connection.remote_address().ip();
    let mut stop = ingress.stop.clone();
    let mut streams = JoinSet::new();
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            _ = streams.join_next(), if !streams.is_empty() => {},
            stream = connection.accept_bi(), if streams.len() < ingress.max_streams => {
                let Ok((send, recv)) = stream else { break };
                let context = ingress.clone();
                let connection = connection.clone();
                streams.spawn(async move {
                    let mut guard = Guard::new(context.resolver.metrics().clone(),Protocol::Doq,context.stop.clone(),true);
                    let mut stage = 0;
                    match timeout(context.io_timeout, doq_request(send, recv, &context, peer, &mut guard, &mut stage)).await {
                        Ok(Ok(())) => {},
                        // A client can cancel its own stream without terminating
                        // unrelated transactions on this connection.
                        Ok(Err(error)) if error.is::<quinn::WriteError>() || error.is::<quinn::ClosedStream>() => {guard.finish(diagnostics::write_error(&error));},
                        failure => {
                            guard.finish(if failure.is_err() {match stage {0=>Event::StreamReadDeadline,1=>Event::StreamRequestDeadline,_=>Event::StreamWriteDeadline}} else {Event::StreamProtocolInvalid});
                            connection.close(VarInt::from_u32(2), b"invalid or incomplete DoQ request");
                        },
                    }
                });
            }
        }
    }
    drain(&mut streams, ingress.shutdown_grace, &ingress.resolver).await;
    connection.close(VarInt::from_u32(0), b"closed");
}

async fn doq_request(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    ingress: &Ingress,
    peer: std::net::IpAddr,
    guard: &mut Guard,
    stage: &mut u8,
) -> Result<()> {
    // read_to_end waits for FIN and rejects a second frame or oversized body.
    let frame = match recv.read_to_end(65537).await {
        Ok(frame) => frame,
        Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))) => {
            guard.finish(Event::StreamPeerCancelled);
            return Ok(());
        }
        Err(quinn::ReadToEndError::Read(quinn::ReadError::ConnectionLost(_))) => {
            guard.finish(Event::StreamConnectionLost);
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(frame.len() >= 14, "short DoQ frame");
    ensure!(
        usize::from(u16::from_be_bytes([frame[0], frame[1]])) == frame.len() - 2,
        "invalid DoQ length"
    );
    ensure!(frame[2..4] == [0, 0], "DoQ ID must be zero");
    ensure_no_keepalive(&frame[2..])?;
    guard.event(Event::StreamFullFrame);
    *stage = 1;
    let response = tokio::select! {
        response = ingress.handle_with_transport(&frame[2..], peer, "doq") => response,
        _ = send.stopped() => {guard.finish(Event::StreamPeerCancelled);return Ok(());},
    };
    *stage = 2;
    if let Some(mut response) = response {
        ensure_no_keepalive(&response)?;
        response[..2].copy_from_slice(&[0, 0]);
        send.write_all(&(response.len() as u16).to_be_bytes())
            .await?;
        send.write_all(&response).await?;
        send.finish()?;
        guard.finish(Event::StreamResponseHandedToTransport);
    } else {
        send.reset(VarInt::from_u32(2))?;
        guard.finish(Event::StreamNoResponse);
    }
    Ok(())
}

fn ensure_no_keepalive(bytes: &[u8]) -> Result<()> {
    let message = crate::protocol::decode(bytes)?;
    ensure!(
        !message.edns.as_ref().is_some_and(|edns| edns
            .options()
            .get(hickory_proto::rr::rdata::opt::EdnsCode::Keepalive)
            .is_some()),
        "TCP Keepalive is forbidden on DoQ"
    );
    Ok(())
}

async fn h3_connection(connection: Connection, ingress: Ingress, local_port: u16) {
    let peer = connection.remote_address().ip();
    let mut stop = ingress.stop.clone();
    let mut builder = h3::server::builder();
    builder.max_field_section_size(128 * 1024);
    let Ok(Ok(mut h3)) = timeout(
        ingress.io_timeout,
        builder.build::<_, Bytes>(h3_quinn::Connection::new(connection.clone())),
    )
    .await
    else {
        return;
    };
    let mut streams = JoinSet::new();
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            _ = streams.join_next(), if !streams.is_empty() => {},
            request = h3.accept(), if streams.len() < ingress.max_streams => {
                let Ok(Some(request)) = request else { break };
                let context = ingress.clone();
                let connection = connection.clone();
                streams.spawn(async move {
                    let mut guard = Guard::new(context.resolver.metrics().clone(),Protocol::H3,context.stop.clone(),true);
                    match timeout(context.io_timeout, async {
                        let (request, stream) = request.resolve_request().await?;
                        h3_request(request, stream, &context, peer, &connection, local_port).await
                    }).await {
                        Ok(Ok(Some(status))) => {
                            guard.event(match status { 200..=299=>Event::HttpResponse2xx,400..=499=>Event::HttpResponse4xx,_=>Event::HttpResponse5xx });
                            guard.finish(Event::StreamResponseHandedToTransport);
                        }
                        Ok(Ok(None)) => guard.finish(Event::StreamConnectionLost),
                        Ok(Err(_)) => guard.finish(Event::HttpRequestFailed),
                        Err(_) => guard.finish(Event::HttpRequestDeadline),
                    }
                });
            }
        }
    }
    drain(&mut streams, ingress.shutdown_grace, &ingress.resolver).await;
    connection.close(VarInt::from_u32(0x100), b"closed");
}

async fn h3_request(
    request: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    ingress: &Ingress,
    peer: std::net::IpAddr,
    connection: &Connection,
    local_port: u16,
) -> Result<Option<u16>> {
    let mut body = Vec::new();
    while let Some(mut data) = stream.recv_data().await? {
        if body.len() + data.remaining() > 65535 {
            stream.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
            stream
                .send_response(
                    http::Response::builder()
                        .status(413)
                        .header("cache-control", "no-store")
                        .header("alt-svc", crate::doh::alt_svc(Some(local_port)))
                        .body(())?,
                )
                .await?;
            stream.finish().await?;
            return Ok(Some(413));
        }
        let count = data.remaining();
        body.extend_from_slice(&data.copy_to_bytes(count));
    }
    let parsed = crate::doh::request_bytes(
        request.method().as_str(),
        request.uri().path_and_query().map_or("/", |p| p.as_str()),
        request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        &body,
    );
    let (status, response) = match parsed {
        // h3 0.0.8 does not expose response STOP_SENDING through RequestStream.
        // Whole-connection loss cancels immediately; a reset of only this H3
        // stream is observed at response write or the enclosing I/O deadline.
        Ok(query) => match tokio::select! {
            response = ingress.handle_with_transport(&query, peer, "doh3") => response,
            _ = connection.closed() => return Ok(None),
        } {
            Some(response) => (200, response),
            None => (400, Vec::new()),
        },
        Err(status) => (status, Vec::new()),
    };
    let mut headers = http::Response::builder()
        .status(status)
        .header("cache-control", "no-store")
        .header("alt-svc", crate::doh::alt_svc(Some(local_port)));
    if status == 200 {
        headers = headers.header("content-type", "application/dns-message");
    }
    stream.send_response(headers.body(())?).await?;
    if !response.is_empty() {
        stream.send_data(Bytes::from(response)).await?;
    }
    stream.finish().await?;
    Ok(Some(status))
}
