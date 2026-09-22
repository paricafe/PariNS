//! Local authenticated DoT transport benchmark, not a production capacity claim.
//! cargo run --release --example bench_dot -- [queries=300] [concurrency=8]
//! Runs fresh then pooled with identical input and no DNS response cache; repeat
//! trials to account for first-run warmup and compare medians, not one sample.
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{
    protocol,
    tls::{self, ClientSettings, PoolSettings, TlsFiles, Upstream},
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

#[derive(Default)]
struct Counts {
    connections: AtomicU64,
    handshakes: AtomicU64,
    full_handshakes: AtomicU64,
    answers: AtomicU64,
}

struct Mock {
    address: SocketAddr,
    counts: Arc<Counts>,
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}

impl Mock {
    async fn start(files: &TlsFiles, concurrency: usize) -> Result<Self> {
        let acceptor = TlsAcceptor::from(tls::server_config(files, &[b"dot"])?);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let counts = Arc::new(Counts::default());
        let counter = counts.clone();
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let outcome = async {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => break,
                        done = connections.join_next(), if !connections.is_empty() => {
                            done.context("missing connection")??;
                        }
                        accepted = listener.accept(), if connections.len() < concurrency * 2 => {
                            let (stream, _) = accepted?;
                            stream.set_nodelay(true)?;
                            counter.connections.fetch_add(1, Ordering::Relaxed);
                            let acceptor = acceptor.clone();
                            let counter = counter.clone();
                            connections.spawn(async move {
                                let mut stream = match timeout(Duration::from_secs(5), acceptor.accept(stream)).await {
                                    Ok(Ok(stream)) => stream,
                                    _ => return,
                                };
                                counter.handshakes.fetch_add(1, Ordering::Relaxed);
                                if stream.get_ref().1.handshake_kind() != Some(rustls::HandshakeKind::Resumed) {
                                    counter.full_handshakes.fetch_add(1, Ordering::Relaxed);
                                }
                                loop {
                                    let result = timeout(Duration::from_secs(5), async {
                                        let length = stream.read_u16().await?;
                                        ensure!(length >= 12, "short DNS frame");
                                        let mut wire = vec![0; usize::from(length)];
                                        stream.read_exact(&mut wire).await?;
                                        let query = protocol::decode(&wire)?;
                                        ensure!(query.queries.len() == 1, "invalid benchmark query");
                                        let mut response = protocol::error_response(&query, ResponseCode::NoError);
                                        response.add_answer(Record::from_rdata(query.queries[0].name().clone(), 60, RData::A(A::new(192, 0, 2, 1))));
                                        let answer = response.to_vec()?;
                                        stream.write_u16(answer.len() as u16).await?;
                                        stream.write_all(&answer).await?;
                                        stream.flush().await?;
                                        counter.answers.fetch_add(1, Ordering::Relaxed);
                                        Ok::<_, anyhow::Error>(())
                                    }).await;
                                    if !matches!(result, Ok(Ok(()))) { break; }
                                }
                            });
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            outcome
        });
        Ok(Self {
            address,
            counts,
            stop,
            task,
        })
    }

    async fn close(self) -> Result<()> {
        let _ = self.stop.send(true);
        self.task.await?
    }
}

fn query(index: usize) -> Result<Message> {
    let mut query = Message::new(index as u16, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query.add_query(Query::query(
        Name::from_ascii(format!("q{index}.bench.test."))?,
        RecordType::A,
    ));
    Ok(query)
}

async fn run(queries: usize, concurrency: usize, pooled: bool) -> Result<()> {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let directory = tempfile::tempdir()?;
    let files = TlsFiles {
        cert_file: directory.path().join("cert.pem"),
        key_file: directory.path().join("key.pem"),
    };
    std::fs::write(&files.cert_file, generated.cert.pem())?;
    std::fs::write(&files.key_file, generated.signing_key.serialize_pem())?;
    let settings = ClientSettings {
        server_name: "localhost".into(),
        ca_file: Some(files.cert_file.clone()),
    };
    let client = if pooled {
        Upstream::with_pool(
            &settings,
            &PoolSettings {
                enabled: true,
                max_connections: concurrency,
                idle_timeout_ms: 30_000,
            },
        )?
    } else {
        Upstream::new(&settings)?
    };
    let mock = Mock::start(&files, concurrency).await?;
    let result = timeout(Duration::from_secs(60), async {
        let mut running = JoinSet::new();
        let mut next = 0;
        let mut samples = Vec::with_capacity(queries);
        let mut errors = 0;
        let started = Instant::now();
        while next < queries || !running.is_empty() {
            while next < queries && running.len() < concurrency {
                let query = query(next)?;
                let client = client.clone();
                let address = mock.address;
                running.spawn(async move {
                    let started = Instant::now();
                    let result = timeout(Duration::from_secs(5), async {
                        let response = client.exchange(&query, address).await?;
                        ensure!(
                            response.id == query.id
                                && response.queries == query.queries
                                && response.response_code == ResponseCode::NoError
                                && response.answers.len() == 1
                                && response.answers[0].data == RData::A(A::new(192, 0, 2, 1)),
                            "incorrect DNS answer"
                        );
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                    (
                        started.elapsed().as_micros() as u64,
                        !matches!(result, Ok(Ok(()))),
                    )
                });
                next += 1;
            }
            match running.join_next().await.context("missing query task")? {
                Ok((micros, failed)) => {
                    samples.push(micros);
                    errors += usize::from(failed);
                }
                Err(_) => errors += 1,
            }
        }
        let elapsed = started.elapsed();
        samples.sort_unstable();
        let percentile = |p: usize| {
            samples
                .get((samples.len() * p).div_ceil(100).saturating_sub(1))
                .copied()
                .unwrap_or(0)
        };
        println!(
            "{}",
            json!({
                "scenario": if pooled { "pooled-dot" } else { "fresh-dot" },
                "queries": queries, "concurrency": concurrency,
                "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
                "qps": queries as f64 / elapsed.as_secs_f64(), "errors": errors,
                "p50_us": percentile(50), "p95_us": percentile(95), "p99_us": percentile(99),
                "connections": mock.counts.connections.load(Ordering::Relaxed),
                "handshakes": mock.counts.handshakes.load(Ordering::Relaxed),
                "full_handshakes": mock.counts.full_handshakes.load(Ordering::Relaxed),
                "answers": mock.counts.answers.load(Ordering::Relaxed),
                "server_tcp_nodelay": true, "artificial_delay_ms": 0,
            })
        );
        ensure!(errors == 0, "benchmark requests failed");
        let connections = mock.counts.connections.load(Ordering::Relaxed);
        let handshakes = mock.counts.handshakes.load(Ordering::Relaxed);
        ensure!(
            mock.counts.answers.load(Ordering::Relaxed) == queries as u64,
            "missing upstream answers"
        );
        ensure!(connections == handshakes, "incomplete TLS handshake");
        if pooled {
            ensure!(
                (1..=concurrency as u64).contains(&connections),
                "pooled connection budget exceeded"
            );
        } else {
            ensure!(
                connections == queries as u64,
                "fresh connection count differs from queries"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("benchmark exceeded 60 second deadline")
    .and_then(|result| result);
    drop(client);
    let cleanup = mock.close().await;
    result?;
    cleanup
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        arguments.len() <= 2,
        "usage: bench_dot [queries] [concurrency]"
    );
    let queries = arguments
        .first()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(300);
    let concurrency = arguments
        .get(1)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(8);
    ensure!((1..=10000).contains(&queries), "queries must be 1..=10000");
    ensure!(
        (1..=64).contains(&concurrency),
        "concurrency must be 1..=64"
    );
    run(queries, concurrency, false).await?;
    run(queries, concurrency, true).await
}
