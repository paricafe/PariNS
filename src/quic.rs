//! Bounded DoQ and HTTP/3 adapters. DNS semantics stay in the shared ingress.
//!
//! QUIC listener ownership includes Quinn's drivers and the final socket release
//! (see `quic::lifecycle`). Server retains this cleanup across adapter cancellation,
//! and a completed shutdown permits immediate rebinding of the same address.
//!
//! Admission: an Initial from an address Quinn has not validated is answered with a
//! stateless Retry and takes no connection slot or source budget, so spoofed
//! sources cannot hold handshakes open. Only a validated Incoming (its Retry or
//! NEW_TOKEN token proved the address) takes the global slot and the per-source
//! budget before the handshake starts.
//!
//! Shutdown: only request streams with a complete query (a DoQ stream whose FIN
//! arrived, an HTTP/3 request whose body ended) can force a stop. Unfinished
//! handshakes and streams still receiving their query are cancelled at once and
//! recorded as `handshake_shutdown` or `stream_shutdown`. Peer close notification (QUIC close, like the
//! HTTP/2 GOAWAY acknowledgement in `doh`) uses at most half the shutdown grace, then
//! the connection is dropped. Any force records its first bounded cause, and a
//! terminal stop that cannot publish a clean cache snapshot logs that cause. An
//! error exit outside a stop drains within the same grace without forcing, so the
//! shared upstream pool keeps serving while the listener is restarted.
use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use bytes::{Buf, Bytes};
use quinn::{Connection, Endpoint, VarInt};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};

use crate::{ingress::Ingress, resolver::ForcedShutdown};
pub mod diagnostics;
/// RFC 9250 transaction cancellation code.
const DOQ_REQUEST_CANCELLED: VarInt = VarInt::from_u32(3);
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

/// `max_streams` is the validated `max_inflight`, capped at 1024.
pub fn bind(
    address: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    max_streams: usize,
    protocol: Protocol,
) -> Result<Listener> {
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
    let listener = match protocol {
        Protocol::Doq => "DoQ",
        Protocol::H3 => "DoH3",
    };
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => ingress.settle(listener, result),
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if !incoming.remote_address_validated() {
                    // Quinn guarantees may_retry() for an unvalidated address. A
                    // Retry is not counted: `incoming` stays one per client
                    // connection, and the persisted counter set stays fixed.
                    if let Err(error) = incoming.retry() {
                        error.into_incoming().refuse();
                    }
                    continue;
                }
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
                    // An unfinished handshake owns no resolver work: stopping drops it
                    // (recorded as HandshakeShutdown) and endpoint close finishes it.
                    let mut stop = context.stop.clone();
                    let handshake = tokio::select! {
                        _ = stop.changed() => return,
                        handshake = timeout(context.io_timeout,incoming) => handshake,
                    };
                    let connection = match handshake {
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
    // Close notification ends at most half a grace after the stop (less if the
    // drain took longer): an unresponsive peer must not hold the listener until
    // the server-wide grace forces the stop.
    let close_by = Instant::now() + ingress.shutdown_grace / 2;
    ingress
        .drain(&mut connections, ForcedShutdown::QuicDrain)
        .await;
    endpoint.close(VarInt::from_u32(0), b"shutdown");
    // Owned drivers are joined afterwards by the endpoint owner.
    let _ = timeout_at(close_by, endpoint.wait_idle()).await;
    Ok(())
}

async fn doq_connection(connection: Connection, ingress: Ingress) {
    let peer = connection.remote_address().ip();
    let mut stop = ingress.stop.clone();
    let mut streams = JoinSet::new();
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            Some(result) = streams.join_next(), if !streams.is_empty() => ingress.settle("DoQ", result),
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
    drain_streams(&ingress, &mut streams).await;
    connection.close(VarInt::from_u32(0), b"closed");
}

/// During a stop only the listener drain counts the grace: it forces the stop
/// and aborts this connection on expiry. A connection that ends outside a stop
/// drains its streams within the grace without forcing.
async fn drain_streams(ingress: &Ingress, streams: &mut JoinSet<()>) {
    if *ingress.stop.borrow() {
        while streams.join_next().await.is_some() {}
    } else {
        ingress.drain(streams, ForcedShutdown::QuicDrain).await;
    }
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
    // Biased: a frame whose FIN arrived before the stop is still answered.
    let read = tokio::select! {
        biased;
        read = recv.read_to_end(65537) => read,
        () = ingress.stopped() => {
            // No resolver work yet. Reset explicitly: dropping a SendStream would
            // finish it, which a client reads as an empty response. The guard
            // records StreamShutdown on drop.
            let _ = recv.stop(DOQ_REQUEST_CANCELLED);
            let _ = send.reset(DOQ_REQUEST_CANCELLED);
            return Ok(());
        }
    };
    let frame = match read {
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
    let (response, delivery) = tokio::select! {
        response = ingress.handle_for_delivery(&frame[2..], peer, "doq") => response,
        _ = send.stopped() => {guard.finish(Event::StreamPeerCancelled);return Ok(());},
    };
    *stage = 2;
    // The shared ingress never answers with EDNS TCP keepalive: requests drop it
    // (`protocol::request`) and the upstream pool strips it from answers.
    if let Some(mut response) = response {
        response[..2].copy_from_slice(&[0, 0]);
        send.write_all(&(response.len() as u16).to_be_bytes())
            .await?;
        send.write_all(&response).await?;
        send.finish()?;
        if let Some(delivery) = delivery {
            delivery.delivered();
        }
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
    let built = tokio::select! {
        _ = stop.changed() => return,
        built = timeout(
            ingress.io_timeout,
            builder.build::<_, Bytes>(h3_quinn::Connection::new(connection.clone())),
        ) => built,
    };
    let Ok(Ok(mut h3)) = built else {
        return;
    };
    let mut streams = JoinSet::new();
    while !*stop.borrow() {
        tokio::select! {
            _ = stop.changed() => break,
            Some(result) = streams.join_next(), if !streams.is_empty() => ingress.settle("DoH3", result),
            request = h3.accept(), if streams.len() < ingress.max_streams => {
                let Ok(Some(request)) = request else { break };
                let context = ingress.clone();
                let connection = connection.clone();
                streams.spawn(async move {
                    let mut guard = Guard::new(context.resolver.metrics().clone(),Protocol::H3,context.stop.clone(),true);
                    match timeout(context.io_timeout, h3_request(request, &context, peer, &connection, local_port)).await {
                        Ok(Ok(H3Outcome::Status(status))) => {
                            guard.event(match status { 200..=299=>Event::HttpResponse2xx,400..=499=>Event::HttpResponse4xx,_=>Event::HttpResponse5xx });
                            guard.finish(Event::StreamResponseHandedToTransport);
                        }
                        Ok(Ok(H3Outcome::ConnectionLost)) => guard.finish(Event::StreamConnectionLost),
                        // The guard records StreamShutdown on drop.
                        Ok(Ok(H3Outcome::Shutdown)) => {},
                        Ok(Err(error)) => guard.finish(diagnostics::h3_error(&error)),
                        Err(_) => guard.finish(Event::HttpRequestDeadline),
                    }
                });
            }
        }
    }
    drain_streams(&ingress, &mut streams).await;
    connection.close(VarInt::from_u32(0x100), b"closed");
}

enum H3Outcome {
    Status(u16),
    ConnectionLost,
    /// Cancelled at shutdown before the request (headers and body) was complete.
    Shutdown,
}

type H3Stream = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

async fn h3_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    ingress: &Ingress,
    peer: std::net::IpAddr,
    connection: &Connection,
    local_port: u16,
) -> Result<H3Outcome> {
    // Biased: a request received in full before the stop is still answered.
    // Dropping an unresolved request leaves its stream to the connection close.
    let (request, mut stream) = tokio::select! {
        biased;
        resolved = resolver.resolve_request() => resolved?,
        () = ingress.stopped() => return Ok(H3Outcome::Shutdown),
    };
    let body = tokio::select! {
        biased;
        body = h3_body(&mut stream) => body?,
        () = ingress.stopped() => {
            // No resolver work yet: the peer may retry this request elsewhere.
            stream.stop_sending(h3::error::Code::H3_REQUEST_REJECTED);
            stream.stop_stream(h3::error::Code::H3_REQUEST_REJECTED);
            return Ok(H3Outcome::Shutdown);
        }
    };
    let Some(body) = body else {
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
        return Ok(H3Outcome::Status(413));
    };
    let parsed = crate::doh::request_bytes(
        request.method().as_str(),
        request.uri().path_and_query().map_or("/", |p| p.as_str()),
        request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        &body,
    );
    let (status, response, delivery) = match parsed {
        // h3 0.0.8 does not expose response STOP_SENDING through RequestStream.
        // Whole-connection loss cancels immediately; a reset of only this H3
        // stream is observed at response write or the enclosing I/O deadline,
        // and the undelivered request then counts as cancelled.
        Ok(query) => match tokio::select! {
            handled = ingress.handle_for_delivery(&query, peer, "doh3") => handled,
            _ = connection.closed() => return Ok(H3Outcome::ConnectionLost),
        } {
            (Some(response), delivery) => (200, response, delivery),
            (None, delivery) => (400, Vec::new(), delivery),
        },
        Err(status) => (status, Vec::new(), None),
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
    if let Some(delivery) = delivery {
        delivery.delivered();
    }
    Ok(H3Outcome::Status(status))
}

/// Reads the body until FIN; `None` stops reading at the DNS message size limit.
async fn h3_body(stream: &mut H3Stream) -> Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    while let Some(mut data) = stream.recv_data().await? {
        if body.len() + data.remaining() > crate::protocol::MAX_MESSAGE {
            stream.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
            return Ok(None);
        }
        let count = data.remaining();
        body.extend_from_slice(&data.copy_to_bytes(count));
    }
    Ok(Some(body))
}
