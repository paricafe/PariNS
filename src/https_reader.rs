//! Private pinned HTTPS transport. Callers retain their distinct URL permissions.
use bytes::Bytes;
use futures_util::Stream;
use http::{HeaderMap, StatusCode, Uri, header};
use hyper::body::Body;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpStream, lookup_host},
    time::timeout,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const HEADER_LIMIT: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderError {
    pub code: &'static str,
    pub retry_after_unix: Option<u64>,
}
impl ReaderError {
    pub(crate) fn new(code: &'static str) -> Self {
        Self {
            code,
            retry_after_unix: None,
        }
    }
}
impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for ReaderError {}

#[derive(Clone)]
pub(crate) struct HttpsReader {
    tls: Arc<rustls::ClientConfig>,
}
impl HttpsReader {
    pub fn new() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports TLS")
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self { tls: Arc::new(tls) }
    }

    pub(crate) async fn request(
        &self,
        uri: Uri,
        headers: HeaderMap,
    ) -> Result<WireResponse, ReaderError> {
        let host = validate_https_uri(&uri)?.to_owned();
        let port = uri.port_u16().unwrap_or(443);
        timeout(CONNECT_TIMEOUT, async {
            // Pin the checked addresses to the actual socket: no second DNS lookup.
            let addresses: Vec<_> = lookup_host((host.as_str(), port))
                .await
                .map_err(|_| ReaderError::new("network_error"))?
                .take(17)
                .collect();
            validate_addresses(&addresses)?;
            let mut connected = None;
            for address in addresses {
                if let Ok(socket) = TcpStream::connect(address).await {
                    verify_peer(
                        address,
                        socket
                            .peer_addr()
                            .map_err(|_| ReaderError::new("network_error"))?,
                    )?;
                    connected = Some(socket);
                    break;
                }
            }
            let socket = connected.ok_or_else(|| ReaderError::new("network_error"))?;
            let stream = self.tls_connect(host, socket).await?;
            let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
                .max_buf_size(32 * 1024)
                .handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| ReaderError::new("network_error"))?;
            let task = tokio::spawn(async move {
                let _ = connection.await;
            });
            let guard = Connection(task.abort_handle());
            let mut request = http::Request::builder()
                .method("GET")
                .uri(uri.path_and_query().map_or("/", |p| p.as_str()))
                .header(
                    header::HOST,
                    uri.authority().expect("validated URI").as_str(),
                )
                .header(header::CONNECTION, "close");
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let request = request
                .body(axum::body::Body::empty())
                .map_err(|_| ReaderError::new("invalid_request"))?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|_| ReaderError::new("network_error"))?;
            let (parts, mut body) = response.into_parts();
            let stream = futures_util::stream::poll_fn(move |cx| {
                loop {
                    match Pin::new(&mut body).poll_frame(cx) {
                        std::task::Poll::Ready(Some(Ok(frame))) => {
                            if let Ok(data) = frame.into_data() {
                                return std::task::Poll::Ready(Some(Ok(data)));
                            }
                        }
                        std::task::Poll::Ready(Some(Err(_))) => {
                            return std::task::Poll::Ready(Some(Err(ReaderError::new(
                                "network_error",
                            ))));
                        }
                        std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                        std::task::Poll::Pending => return std::task::Poll::Pending,
                    }
                }
            });
            validate_headers(&parts.headers)?;
            Ok(WireResponse {
                status: parts.status,
                headers: parts.headers,
                body: Box::pin(stream),
                _connection: Some(guard),
            })
        })
        .await
        .map_err(|_| ReaderError::new("connect_timeout"))?
    }

    async fn tls_connect(
        &self,
        host: String,
        socket: TcpStream,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, ReaderError> {
        let name = rustls::pki_types::ServerName::try_from(host)
            .map_err(|_| ReaderError::new("destination_not_allowed"))?;
        tokio_rustls::TlsConnector::from(self.tls.clone())
            .connect(name, socket)
            .await
            .map_err(|_| ReaderError::new("tls_error"))
    }
}
pub(crate) struct Connection(tokio::task::AbortHandle);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(crate) struct WireResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Pin<Box<dyn Stream<Item = Result<Bytes, ReaderError>> + Send>>,
    pub(crate) _connection: Option<Connection>,
}

pub(crate) fn validate_https_uri(uri: &Uri) -> Result<&str, ReaderError> {
    let authority = uri
        .authority()
        .ok_or_else(|| ReaderError::new("destination_not_allowed"))?;
    let host = uri
        .host()
        .ok_or_else(|| ReaderError::new("destination_not_allowed"))?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || host.is_empty()
        || uri.port_u16() == Some(0)
    {
        return Err(ReaderError::new("destination_not_allowed"));
    }
    Ok(host.trim_start_matches('[').trim_end_matches(']'))
}
pub(crate) fn validate_headers(headers: &HeaderMap) -> Result<(), ReaderError> {
    let total = headers.iter().try_fold(2usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())?
            .checked_add(4)
    });
    if total.is_none_or(|total| total > HEADER_LIMIT) {
        return Err(ReaderError::new("response_headers_too_large"));
    }
    Ok(())
}
pub(crate) fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), ReaderError> {
    if addresses.is_empty()
        || addresses.len() > 16
        || addresses.iter().any(|a| !global_address(a.ip()))
    {
        return Err(ReaderError::new("destination_not_allowed"));
    }
    Ok(())
}
fn verify_peer(expected: SocketAddr, actual: SocketAddr) -> Result<(), ReaderError> {
    if expected != actual {
        return Err(ReaderError::new("destination_not_allowed"));
    }
    Ok(())
}
pub(crate) fn global_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => {
            let [x, y, z, _] = a.octets();
            !(x == 0
                || x == 10
                || x == 127
                || x >= 224
                || (x == 100 && (64..=127).contains(&y))
                || (x == 169 && y == 254)
                || (x == 172 && (16..=31).contains(&y))
                || (x == 192
                    && (y == 168
                        || (y == 0 && z == 0)
                        || (y == 0 && z == 2)
                        || (y == 88 && z == 99)))
                || (x == 198 && (y == 18 || y == 19 || (y == 51 && z == 100)))
                || (x == 203 && y == 0 && z == 113))
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_resolved_addresses_and_actual_peer_must_match_public_pin() {
        let public = "8.8.8.8:443".parse().unwrap();
        let private = "127.0.0.1:443".parse().unwrap();
        assert!(validate_addresses(&[public]).is_ok());
        assert!(validate_addresses(&[public, private]).is_err());
        assert!(validate_addresses(&[]).is_err());
        assert!(validate_addresses(&[public; 17]).is_err());
        assert!(verify_peer(public, public).is_ok());
        assert!(verify_peer(public, private).is_err());
    }

    #[tokio::test]
    async fn real_tls_rejects_untrusted_self_signed_fixture() {
        let generated = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![generated.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(socket)
                .await
                .is_err()
        });
        let client = HttpsReader::new();
        let socket = TcpStream::connect(address).await.unwrap();
        assert_eq!(
            client
                .tls_connect("example.com".into(), socket)
                .await
                .unwrap_err()
                .code,
            "tls_error"
        );
        assert!(server.await.unwrap());
    }
}
