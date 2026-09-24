//! DNS-over-HTTPS (HTTP/2), with socket-derived identity and bounded request work.
use std::{net::IpAddr, sync::Arc};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use h2::{Reason, server::SendResponse};
use http::{Request, Response};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};
use tokio_rustls::{TlsAcceptor, rustls};

use crate::{ingress::Ingress, metrics::Counter};

pub const PATH: &str = "/dns-query";
pub(crate) fn alt_svc(port: Option<u16>) -> String {
    port.map_or_else(|| "clear".into(), |port| format!("h3=\":{port}\"; ma=300"))
}
const MAX_DNS: usize = 65535;

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
            if encoded.len() > MAX_DNS.div_ceil(3) * 4 {
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
    if bytes.len() > MAX_DNS {
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
    h3_port: Option<u16>,
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
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined { break Err(error.into()); }
            },
            accepted = listener.accept() => {
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
                connections.spawn(async move {
                    let _permit = permit;
                    let _source = source;
                    let mut stop = ingress.stop.clone();
                    let tls = tokio::select! {
                        _ = stop.changed() => return,
                        result = timeout(ingress.io_timeout, acceptor.accept(socket)) => {
                            let Ok(Ok(tls)) = result else { return };
                            tls
                        }
                    };
                    if tls.get_ref().1.alpn_protocol() != Some(b"h2") { return }
                    let _ = connection(tls, peer.ip(), ingress, h3_port).await;
                });
            }
        }
    };
    drop(listener);
    if timeout(ingress.shutdown_grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        ingress.resolver.force_shutdown();
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    outcome
}

async fn connection(
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    peer: IpAddr,
    ingress: Ingress,
    h3_port: Option<u16>,
) -> anyhow::Result<()> {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(ingress.max_streams as u32)
        .max_header_list_size(8192);
    let mut connection = timeout(ingress.io_timeout, builder.handshake(tls)).await??;
    let permits = Arc::new(Semaphore::new(ingress.max_streams));
    let mut requests = JoinSet::new();
    let mut stop = ingress.stop.clone();
    let mut stopping = *stop.borrow();
    if stopping {
        connection.graceful_shutdown();
    }
    loop {
        tokio::select! {
            _ = stop.changed(), if !stopping => {
                stopping = true;
                connection.graceful_shutdown();
            }
            Some(_) = requests.join_next(), if !requests.is_empty() => {},
            accepted = timeout(ingress.io_timeout, connection.accept()) => {
                let Ok(Some(Ok((request, mut respond)))) = accepted else { break };
                if stopping { respond.send_reset(Reason::REFUSED_STREAM); continue }
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(Counter::EncryptedRejected);
                    respond.send_reset(Reason::REFUSED_STREAM);
                    continue
                };
                let ingress = ingress.clone();
                requests.spawn(async move {
                    let _permit = permit;
                    // One total deadline includes body collection, resolution and response flow control.
                    let _ = timeout(ingress.io_timeout, request_task(request, respond, peer, ingress, h3_port)).await;
                });
            }
        }
    }
    // JoinSet drop aborts remaining request work; there are no detached collectors.
    if *stop.borrow() && !requests.is_empty() {
        ingress.resolver.force_shutdown();
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    Ok(())
}

async fn request_task(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    peer: IpAddr,
    ingress: Ingress,
    h3_port: Option<u16>,
) -> anyhow::Result<()> {
    let (parts, mut stream) = request.into_parts();
    let mut body = Vec::new();
    let mut status = None;
    while let Some(chunk) = stream.data().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_DNS {
            status = Some(413);
            break;
        }
        body.extend_from_slice(&chunk);
        stream.flow_control().release_capacity(chunk.len())?;
    }
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

async fn send(
    respond: &mut SendResponse<Bytes>,
    status: u16,
    bytes: Vec<u8>,
    h3_port: Option<u16>,
) -> anyhow::Result<()> {
    let mut builder = Response::builder()
        .status(status)
        .header("cache-control", "no-store")
        .header("alt-svc", alt_svc(h3_port));
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
