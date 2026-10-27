//! Read-only loopback liveness and fixed-cardinality Prometheus exposition.
//! Its connections have their own small budget, separate from DNS.
use std::{fmt::Write, sync::Arc};

use anyhow::{Result, ensure};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

use crate::{ingress::Ingress, metrics::Metrics};

/// Renders one metrics snapshot. Counters are read one at a time, so a scrape
/// taken under load can be slightly out of step across counters.
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
    let diagnostics: Vec<_> = metrics
        .diagnostics()
        .map(|(name, help, value)| (name, help, value, false))
        .collect();
    output + &render_extra(&diagnostics)
}

/// Counters and gauges outside the persisted snapshot: listener diagnostics,
/// the counters of the recursive resolution source and the prefetch counters
/// of the current cache generation.
fn render_extra(metrics: &[(&str, &str, u64, bool)]) -> String {
    let mut output = String::new();
    for (name, help, value, gauge) in metrics {
        let (metric, kind) = if *gauge {
            (format!("parins_{name}"), "gauge")
        } else {
            (format!("parins_{name}_total"), "counter")
        };
        let _ = writeln!(
            output,
            "# HELP {metric} {help}\n# TYPE {metric} {kind}\n{metric} {value}"
        );
    }
    output
}

/// Concurrent loopback health and metrics connections. A budget of its own keeps
/// probes answering while DNS connections fill the shared limit, and keeps
/// probes from taking DNS connection slots.
const MAX_CONNECTIONS: usize = 8;

pub async fn serve(listener: TcpListener, ingress: Ingress) -> Result<()> {
    ensure!(
        listener.local_addr()?.ip().is_loopback(),
        "admin listener must bind a loopback address"
    );
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut stop = ingress.stop.clone();
    let mut tasks = JoinSet::new();
    let outcome = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            Some(result) = tasks.join_next(), if !tasks.is_empty() => ingress.settle("admin", result),
            accepted = crate::transport::accept::accept(&listener) => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error.into()),
                };
                // Excess probes are closed at once; DNS rejection counters stay DNS-only.
                let Ok(permit) = connections.clone().try_acquire_owned() else {
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
            render(ingress.resolver.metrics())
                + &render_extra(&ingress.resolver.recursion_metrics())
                + &render_extra(&ingress.resolver.refresh_metrics()),
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

#[cfg(test)]
mod tests {
    use super::render_extra;

    #[test]
    fn extra_metrics_render_counters_and_gauges() {
        let text = render_extra(&[
            ("recursion_started", "Started.", 3, false),
            ("recursion_outbound_inflight", "In flight.", 1, true),
        ]);
        assert!(text.contains(
            "# TYPE parins_recursion_started_total counter\nparins_recursion_started_total 3\n"
        ));
        assert!(text.contains("# TYPE parins_recursion_outbound_inflight gauge\nparins_recursion_outbound_inflight 1\n"));
    }
}
