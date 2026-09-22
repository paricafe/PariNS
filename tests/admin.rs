use std::{net::SocketAddr, sync::Arc, time::Duration};

use parins::{
    admin,
    ingress::Ingress,
    metrics::{Counter, Metrics, Timer},
    resolver::Resolver,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};

#[test]
fn exposition_has_fixed_counters_gauges_and_cumulative_histograms() {
    let metrics = Metrics::default();
    metrics.inc(Counter::CacheHits);
    let mut completed = metrics.track(Timer::Request);
    completed.complete();
    drop(completed);
    let active = metrics.track(Timer::Upstream);
    let output = admin::render(&metrics);
    assert!(output.contains("# TYPE parins_cache_hits_total counter\nparins_cache_hits_total 1\n"));
    assert!(output.contains("parins_request_inflight 0\n"));
    assert!(output.contains("parins_upstream_inflight 1\n"));
    assert!(output.contains("# TYPE parins_request_duration_seconds histogram\n"));
    assert!(output.contains("parins_request_duration_seconds_bucket{le=\"0.001000\"}"));
    assert!(output.contains("parins_request_duration_seconds_bucket{le=\"+Inf\"} 1\n"));
    assert!(output.contains("parins_request_duration_seconds_count 1\n"));
    assert!(output.contains("parins_request_duration_seconds_sum "));
    // Histogram boundaries are the only labels; DNS names and peer addresses never become series.
    for line in output.lines().filter(|line| line.contains('{')) {
        assert!(line.contains("_bucket{le=\""));
    }
    drop(active);
}

fn ingress(stop: watch::Receiver<bool>) -> Ingress {
    Ingress {
        source_limits: Arc::new(parins::limits::Limiter::new(&Default::default()).unwrap()),
        resolver: Arc::new(Resolver::new(
            "127.0.0.1:9".parse().unwrap(),
            Duration::from_millis(100),
        )),
        queries: Arc::new(Semaphore::new(1)),
        connections: Arc::new(Semaphore::new(4)),
        stop,
        io_timeout: Duration::from_millis(150),
        shutdown_grace: Duration::from_millis(200),
        max_streams: 1,
    }
}

struct Server {
    address: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<anyhow::Result<()>>,
    ingress: Ingress,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = watch::channel(false);
        let ingress = ingress(stopped);
        let task = tokio::spawn(admin::serve(listener, ingress.clone()));
        Self {
            address,
            stop,
            task,
            ingress,
        }
    }

    async fn request(&self, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(self.address).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = String::new();
        timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    }

    async fn finish(self) {
        self.stop.send(true).unwrap();
        timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(self.ingress.connections.available_permits(), 4);
        let _listener = TcpListener::bind(self.address).await.unwrap();
    }
}

#[tokio::test]
async fn loopback_health_and_metrics_are_read_only_and_no_store() {
    let server = Server::start().await;
    server.ingress.resolver.metrics().inc(Counter::CacheHits);
    let health = server
        .request(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    assert!(health.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(health.contains("Content-Length: 3\r\n"));
    assert!(health.ends_with("\r\n\r\nok\n"));
    let metrics = server
        .request(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    assert!(metrics.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(metrics.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n"));
    assert!(metrics.contains("Cache-Control: no-store\r\n"));
    assert!(metrics.contains("parins_cache_hits_total 1\n"));
    for (request, status) in [
        ("POST /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n", 405),
        ("GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n", 404),
        (
            "GET /metrics?secret=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            404,
        ),
        (
            "GET /healthz HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            400,
        ),
        ("GET /healthz HTTP/1.1\r\nContent-Length: 1\r\n\r\nx", 400),
    ] {
        let response = server.request(request.as_bytes()).await;
        assert!(response.starts_with(&format!("HTTP/1.1 {status} ")));
    }
    server.finish().await;
}

#[tokio::test]
async fn oversized_headers_and_idle_clients_cannot_hold_shutdown() {
    let server = Server::start().await;
    let mut oversized = b"GET /metrics HTTP/1.1\r\nX-Padding: ".to_vec();
    oversized.resize(8192, b'a');
    let response = server.request(&oversized).await;
    assert!(response.starts_with("HTTP/1.1 431 "));
    let mut idle = TcpStream::connect(server.address).await.unwrap();
    idle.write_all(b"GET /healthz HTTP/1.1\r\n").await.unwrap();
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(2), idle.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    // A new partial header is cooperatively interrupted by shutdown, not left detached.
    let mut partial = TcpStream::connect(server.address).await.unwrap();
    partial.write_all(b"GET ").await.unwrap();
    server.finish().await;
    let closed = timeout(Duration::from_secs(2), partial.read(&mut byte))
        .await
        .unwrap();
    // The kernel can reset a socket closed with unread request bytes during shutdown.
    assert!(
        matches!(closed, Ok(0))
            || closed.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
    );
}

#[tokio::test]
async fn wildcard_admin_binding_is_rejected_before_accepting_requests() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let (_stop, stopped) = watch::channel(false);
    let result = admin::serve(listener, ingress(stopped)).await;
    assert!(result.unwrap_err().to_string().contains("loopback"));
}
