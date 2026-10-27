//! DNS-over-HTTPS (HTTP/2), with socket-derived identity and bounded request work.
//!
//! Shutdown: only request streams with a complete query (END_STREAM received, so
//! a GET whose query is in the URL is complete once its headers arrive) can force
//! a stop. Streams still receiving a body are refused at once, like unfinished
//! handshakes. Peer close notification (the two-phase GOAWAY) uses at most half
//! the shutdown grace. An error exit outside a stop drains without forcing.
//!
//! Failed and timed-out TLS handshakes and connections without h2 are counted
//! only on `/metrics`.
use std::{net::IpAddr, sync::Arc};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use h2::{Reason, server::SendResponse};
use http::{Request, Response};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};
use tokio_rustls::{TlsAcceptor, rustls};

use crate::{
    ingress::Ingress,
    metrics::{Counter, Diagnostic},
    protocol::MAX_MESSAGE,
    resolver::ForcedShutdown,
};

pub const PATH: &str = "/dns-query";

/// The DoH HTTP/3 port while that listener is serving, `None` while it is not
/// (for example while it retries a bind). Each HTTP/2 response asks, so Alt-Svc
/// never sends clients to an HTTP/3 endpoint that is down.
pub type H3Port = Arc<dyn Fn() -> Option<u16> + Send + Sync>;

pub(crate) fn alt_svc(port: Option<u16>) -> String {
    port.map_or_else(|| "clear".into(), |port| format!("h3=\":{port}\"; ma=300"))
}

/// Shared HTTP/2 and HTTP/3 wire contract. No proxy header is used for identity.
pub fn request_bytes(
    method: &str,
    path_and_query: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Vec<u8>, u16> {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    if path != PATH {
        return Err(404);
    }
    let bytes = match method {
        "GET" => {
            if !body.is_empty() {
                return Err(400);
            }
            let encoded = query.strip_prefix("dns=").ok_or(400u16)?;
            if encoded.len() > MAX_MESSAGE.div_ceil(3) * 4 {
                return Err(413);
            }
            URL_SAFE_NO_PAD.decode(encoded).map_err(|_| 400u16)?
        }
        "POST" => {
            if !query.is_empty() {
                return Err(400);
            }
            if !content_type
                .is_some_and(|value| value.eq_ignore_ascii_case("application/dns-message"))
            {
                return Err(415);
            }
            body.to_vec()
        }
        _ => return Err(405),
    };
    if bytes.len() > MAX_MESSAGE {
        return Err(413);
    }
    if bytes.len() < 12 {
        return Err(400);
    }
    Ok(bytes)
}

pub async fn serve(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    ingress: Ingress,
    h3_port: Option<H3Port>,
) -> anyhow::Result<()> {
    let mut stop = ingress.stop.clone();
    let mut connections = JoinSet::new();
    let acceptor = TlsAcceptor::from(tls);
    let outcome = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            Some(result) = connections.join_next(), if !connections.is_empty() => ingress.settle("DoH", result),
            accepted = crate::transport::accept::accept(&listener) => {
                let (socket, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error.into()),
                };
                let Ok(permit) = ingress.connections.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(Counter::ConnectionsRejected);
                    continue
                };
                let Some(source) = ingress.admit_connection(peer.ip()) else {
                    ingress.resolver.metrics().inc(Counter::ConnectionsRejected);
                    continue;
                };
                let acceptor = acceptor.clone();
                let ingress = ingress.clone();
                let h3_port = h3_port.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    let mut stop = ingress.stop.clone();
                    let metrics = ingress.resolver.metrics();
                    let tls = tokio::select! {
                        _ = stop.changed() => return,
                        result = timeout(ingress.io_timeout, acceptor.accept(socket)) => match result {
                            Ok(Ok(tls)) => tls,
                            Ok(Err(error)) => return metrics.record(handshake_failure(&error)),
                            Err(_) => return metrics.record(Diagnostic::DohHandshakeTimeouts),
                        }
                    };
                    if tls.get_ref().1.alpn_protocol() != Some(b"h2") {
                        return metrics.record(Diagnostic::DohAlpnMismatches);
                    }
                    let _ = connection(tls, peer.ip(), ingress, h3_port).await;
                });
            }
        }
    };
    drop(listener);
    ingress
        .drain(&mut connections, ForcedShutdown::DohDrain)
        .await;
    outcome
}

/// rustls ends the handshake of a client whose offered ALPN lacks h2; a client
/// without ALPN completes it and is counted after the handshake.
fn handshake_failure(error: &std::io::Error) -> Diagnostic {
    match error
        .get_ref()
        .and_then(|error| error.downcast_ref::<rustls::Error>())
    {
        Some(rustls::Error::NoApplicationProtocol) => Diagnostic::DohAlpnMismatches,
        _ => Diagnostic::DohHandshakeFailures,
    }
}

async fn connection(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    peer: IpAddr,
    ingress: Ingress,
    h3_port: Option<H3Port>,
) -> anyhow::Result<()> {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(ingress.max_streams as u32)
        .max_header_list_size(8192);
    let mut stop = ingress.stop.clone();
    let mut connection = tokio::select! {
        _ = stop.changed() => return Ok(()),
        handshake = timeout(ingress.io_timeout, builder.handshake(tls)) => handshake??,
    };
    let permits = Arc::new(Semaphore::new(ingress.max_streams));
    let mut requests = JoinSet::new();
    // Peer close notification uses at most half of the shutdown grace, so an
    // idle peer that never acknowledges GOAWAY cannot force the stop.
    let close_window = ingress.shutdown_grace / 2;
    let mut close_by = stop
        .borrow()
        .then(|| tokio::time::Instant::now() + close_window);
    if close_by.is_some() {
        connection.graceful_shutdown();
    }
    loop {
        if let Some(close_by) = close_by {
            while requests.try_join_next().is_some() {}
            if requests.is_empty() {
                // Only requests with a complete query hold a stopping connection.
                // A responsive peer completes the two-phase GOAWAY; a silent one
                // is dropped.
                let _ = tokio::time::timeout_at(close_by, async {
                    while let Some(Ok((_, mut respond))) = connection.accept().await {
                        respond.send_reset(Reason::REFUSED_STREAM);
                    }
                })
                .await;
                break;
            }
        }
        let stopping = close_by.is_some();
        tokio::select! {
            _ = stop.changed(), if !stopping => {
                close_by = Some(tokio::time::Instant::now() + close_window);
                connection.graceful_shutdown();
            }
            Some(result) = requests.join_next(), if !requests.is_empty() => ingress.settle("DoH", result),
            accepted = timeout(ingress.io_timeout, connection.accept()) => {
                let Ok(Some(Ok((request, mut respond)))) = accepted else { break };
                if stopping { respond.send_reset(Reason::REFUSED_STREAM); continue }
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(Counter::EncryptedRejected);
                    respond.send_reset(Reason::REFUSED_STREAM);
                    continue
                };
                let ingress = ingress.clone();
                let h3_port = h3_port.clone();
                requests.spawn(async move {
                    let _permit = permit;
                    // One total deadline includes body collection, resolution and response flow control.
                    let _ = timeout(ingress.io_timeout, request_task(request, respond, peer, ingress, h3_port)).await;
                });
            }
        }
    }
    if *stop.borrow() {
        // During a stop, complete queries finish even if the peer has gone;
        // the listener grace bounds this wait and forces the stop on expiry.
        while requests.join_next().await.is_some() {}
    }
    // Abort the remaining request tasks and wait for them, so no request work
    // outlives the listener.
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    Ok(())
}

async fn request_task(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    peer: IpAddr,
    ingress: Ingress,
    h3_port: Option<H3Port>,
) -> anyhow::Result<()> {
    let (parts, mut stream) = request.into_parts();
    // Biased: a query received in full before the stop is still answered.
    let (body, status) = tokio::select! {
        biased;
        collected = collect(&mut stream) => collected?,
        () = ingress.stopped() => {
            // No resolver work yet: the peer may retry this stream elsewhere.
            respond.send_reset(Reason::REFUSED_STREAM);
            return Ok(());
        }
    };
    let path = parts.uri.path_and_query().map_or("", |path| path.as_str());
    let content_type = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let bytes = match status.map_or_else(
        || request_bytes(parts.method.as_str(), path, content_type, &body),
        Err,
    ) {
        Ok(bytes) => bytes,
        Err(status) => return send(&mut respond, status, Vec::new(), h3_port).await,
    };
    let response = tokio::select! {
        response = ingress.handle_with_transport(&bytes, peer, "doh") => response,
        _ = std::future::poll_fn(|cx| respond.poll_reset(cx)) => return Ok(()),
    };
    match response {
        Some(bytes) => send(&mut respond, 200, bytes, h3_port).await,
        None => send(&mut respond, 400, Vec::new(), h3_port).await,
    }
}

/// Reads the body until END_STREAM; `Some(413)` stops early at the size limit.
async fn collect(stream: &mut h2::RecvStream) -> anyhow::Result<(Vec<u8>, Option<u16>)> {
    let mut body = Vec::new();
    while let Some(chunk) = stream.data().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_MESSAGE {
            return Ok((body, Some(413)));
        }
        body.extend_from_slice(&chunk);
        stream.flow_control().release_capacity(chunk.len())?;
    }
    Ok((body, None))
}

async fn send(
    respond: &mut SendResponse<Bytes>,
    status: u16,
    bytes: Vec<u8>,
    h3_port: Option<H3Port>,
) -> anyhow::Result<()> {
    let mut builder = Response::builder()
        .status(status)
        .header("cache-control", "no-store")
        .header("alt-svc", alt_svc(h3_port.and_then(|port| port())));
    if status == 200 {
        builder = builder.header("content-type", "application/dns-message");
    }
    if status == 405 {
        builder = builder.header("allow", "GET, POST");
    }
    let mut stream = respond.send_response(builder.body(())?, bytes.is_empty())?;
    let mut bytes = Bytes::from(bytes);
    while !bytes.is_empty() {
        stream.reserve_capacity(bytes.len());
        let capacity = std::future::poll_fn(|cx| stream.poll_capacity(cx))
            .await
            .ok_or_else(|| anyhow::anyhow!("closed HTTP stream"))??;
        if capacity == 0 {
            continue;
        }
        let chunk = bytes.split_to(capacity.min(bytes.len()));
        stream.send_data(chunk, bytes.is_empty())?;
    }
    Ok(())
}
