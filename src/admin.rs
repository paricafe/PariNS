//! Read-only loopback liveness and fixed-cardinality Prometheus exposition.
use std::fmt::Write;

use anyhow::{Result, ensure};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

use crate::{
    ingress::Ingress,
    metrics::{Counter, Metrics},
};

/// Concurrent snapshots are approximate, as in the existing JSON exporter.
pub fn render(metrics: &Metrics) -> String {
    let snapshot = metrics.snapshot();
    let mut output = String::new();
    for (name, value) in snapshot.counters {
        let _ = writeln!(
            output,
            "# HELP parins_{name}_total Total {name}.\n# TYPE parins_{name}_total counter\nparins_{name}_total {value}"
        );
    }
    for (name, value) in [
        ("request", snapshot.request_inflight),
        ("upstream", snapshot.upstream_inflight),
    ] {
        let _ = writeln!(
            output,
            "# HELP parins_{name}_inflight Active {name} operations.\n# TYPE parins_{name}_inflight gauge\nparins_{name}_inflight {value}"
        );
    }
    for (name, histogram) in [
        ("request", snapshot.request_latency),
        ("upstream", snapshot.upstream_latency),
    ] {
        let metric = format!("parins_{name}_duration_seconds");
        let _ = writeln!(
            output,
            "# HELP {metric} Duration of {name} operations in seconds.\n# TYPE {metric} histogram"
        );
        for bucket in histogram.buckets {
            let bound = bucket.upper_bound_micros.map_or_else(
                || "+Inf".into(),
                |value| format!("{:.6}", value as f64 / 1_000_000.0),
            );
            let _ = writeln!(output, "{metric}_bucket{{le=\"{bound}\"}} {}", bucket.count);
        }
        let _ = writeln!(
            output,
            "{metric}_count {}\n{metric}_sum {:.6}",
            histogram.count,
            histogram.sum_micros as f64 / 1_000_000.0
        );
    }
    output
}

pub async fn serve(listener: TcpListener, ingress: Ingress) -> Result<()> {
    ensure!(
        listener.local_addr()?.ip().is_loopback(),
        "admin listener must bind a loopback address"
    );
    let mut stop = ingress.stop.clone();
    let mut tasks = JoinSet::new();
    let outcome = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined { break Err(error.into()); }
            },
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error.into()),
                };
                let Ok(permit) = ingress.connections.clone().try_acquire_owned() else {
                    ingress.resolver.metrics().inc(Counter::ConnectionsRejected);
                    continue
                };
                let ingress = ingress.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = connection(stream, ingress).await;
                });
            }
        }
    };
    drop(listener);
    if timeout(ingress.shutdown_grace, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    outcome
}

async fn connection(mut stream: TcpStream, mut ingress: Ingress) -> Result<()> {
    let request = tokio::select! {
        _ = ingress.stop.changed() => return Ok(()),
        result = timeout(ingress.io_timeout, read_request(&mut stream)) => result?,
    };
    let (status, kind, body) = match request {
        Ok(path) if path == "/healthz" => (200, "text/plain; charset=utf-8", "ok\n".into()),
        Ok(path) if path == "/metrics" => (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            render(ingress.resolver.metrics()),
        ),
        Ok(_) => (404, "text/plain; charset=utf-8", "not found\n".into()),
        Err(status) => (status, "text/plain; charset=utf-8", String::new()),
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Bad Request",
    };
    let allow = if status == 405 { "Allow: GET\r\n" } else { "" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{allow}\r\n{body}",
        body.len()
    );
    timeout(ingress.io_timeout, async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    })
    .await??;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> Result<String, u16> {
    let mut buffer = [0; 8192];
    let mut used = 0;
    let end = loop {
        let count = stream.read(&mut buffer[used..]).await.map_err(|_| 400u16)?;
        if count == 0 {
            return Err(400);
        }
        used += count;
        if let Some(end) = buffer[..used]
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
        {
            break end;
        }
        if used == buffer.len() {
            return Err(431);
        }
    };
    let header = std::str::from_utf8(&buffer[..end]).map_err(|_| 400u16)?;
    let mut lines = header.split("\r\n");
    let mut request = lines.next().ok_or(400u16)?.split(' ');
    let method = request.next().ok_or(400u16)?;
    let path = request
        .next()
        .filter(|path| path.starts_with('/'))
        .ok_or(400u16)?;
    if !matches!(request.next(), Some("HTTP/1.0" | "HTTP/1.1")) || request.next().is_some() {
        return Err(400);
    }
    if method != "GET" {
        return Err(405);
    }
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| 400u16)?;
        http::header::HeaderValue::from_str(value.trim()).map_err(|_| 400u16)?;
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err(400);
        }
    }
    // One request per connection, never accept a body or HTTP pipelining.
    if used != end + 4 {
        return Err(400);
    }
    Ok(path.to_owned())
}
