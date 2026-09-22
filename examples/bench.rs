//! Local synthetic baseline; these measurements are not production capacity claims.
//! cargo run --release --example bench -- [queries=1000] [concurrency=32]
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
use parins::{config::Config, protocol, resolver::Resolver, upstreams::Mode};
use serde_json::json;
use tokio::{
    net::UdpSocket,
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::sleep,
};

struct Mock {
    address: SocketAddr,
    count: Arc<AtomicU64>,
    stop: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}

impl Mock {
    async fn start(delay_ms: u64, concurrency: usize) -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let address = socket.local_addr()?;
        let count = Arc::new(AtomicU64::new(0));
        let counter = count.clone();
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut buffer = [0; 65535];
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    done = requests.join_next(), if !requests.is_empty() => { done.expect("nonempty request set")??; },
                    received = socket.recv_from(&mut buffer), if requests.len() < concurrency * 4 => {
                        let (length, peer) = received?;
                        counter.fetch_add(1, Ordering::Relaxed);
                        let query = protocol::decode(&buffer[..length])?;
                        let mut response = protocol::error_response(&query, ResponseCode::NoError);
                        response.add_answer(Record::from_rdata(query.queries[0].name().clone(), 60, RData::A(A::new(192, 0, 2, 1))));
                        let wire = response.to_vec()?;
                        let socket = socket.clone();
                        requests.spawn(async move {
                            if delay_ms != 0 { sleep(Duration::from_millis(delay_ms)).await; }
                            socket.send_to(&wire, peer).await?;
                            Ok::<_, anyhow::Error>(())
                        });
                    }
                }
            }
            while let Some(result) = requests.join_next().await {
                result??;
            }
            Ok(())
        });
        Ok(Self {
            address,
            count,
            stop,
            task,
        })
    }

    async fn close(self) -> Result<()> {
        let _ = self.stop.send(true);
        self.task.await?
    }
}

fn query(index: usize, warm: bool) -> Result<Message> {
    let mut message = Message::new(index as u16, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(
        Name::from_ascii(if warm {
            "warm.bench.test.".into()
        } else {
            format!("q{index}.bench.test.")
        })?,
        RecordType::A,
    ));
    Ok(message)
}

async fn resolve(resolver: &Resolver, query: Message) -> Result<()> {
    let wire = query.to_vec()?;
    let reply = resolver
        .resolve(&wire, "127.0.0.1".parse()?)
        .await
        .context("resolver dropped request")?;
    // Exercise both DNS wire directions, including the per-caller transaction ID.
    let response = protocol::decode(&reply.message.to_vec()?)?;
    ensure!(
        response.id == query.id
            && response.queries == query.queries
            && response.response_code == ResponseCode::NoError
            && response.answers.len() == 1
            && response.answers[0].data == RData::A(A::new(192, 0, 2, 1)),
        "incorrect DNS answer"
    );
    Ok(())
}

async fn scenario(name: &str, queries: usize, concurrency: usize) -> Result<()> {
    let warm = name == "warm-cache";
    let parallel = name == "cold-parallel";
    // Identical artificial primary latency makes cold-single vs cold-parallel a
    // scheduler comparison. The warm scenario times cache reads after priming.
    let delay_ms = if warm { 0 } else { 20 };
    let primary = Mock::start(delay_ms, concurrency).await?;
    let secondary = match Mock::start(2, concurrency).await {
        Ok(mock) => mock,
        Err(error) => {
            primary.close().await?;
            return Err(error);
        }
    };
    let outcome = async {
        let mut config = Config::parse(include_str!("../parins.example.toml"))?;
        config.upstreams.servers = vec![primary.address.to_string()];
        config.query_timeout_ms = 2000;
        config.coalescing.max_groups = concurrency;
        if parallel {
            config.upstreams.servers.push(secondary.address.to_string());
            config.upstreams.mode = Mode::Parallel;
            config.upstreams.max_extra_inflight = concurrency;
        }
        config.validate()?;
        let resolver = Arc::new(Resolver::try_from_config(&config)?);
        if warm {
            resolve(&resolver, query(0, true)?).await?;
        }
        let before = resolver.metrics().snapshot();
        let before_primary = primary.count.load(Ordering::Relaxed);
        let before_secondary = secondary.count.load(Ordering::Relaxed);
        let mut samples = Vec::with_capacity(queries);
        let mut errors = 0usize;
        let mut running = JoinSet::new();
        let mut next = 0;
        let started = Instant::now();
        while next < queries || !running.is_empty() {
            while next < queries && running.len() < concurrency {
                let query = query(next, warm)?;
                let resolver = resolver.clone();
                running.spawn(async move {
                    let started = Instant::now();
                    let result = resolve(&resolver, query).await;
                    (started.elapsed().as_micros() as u64, result.is_err())
                });
                next += 1;
            }
            match running
                .join_next()
                .await
                .context("missing benchmark task")?
            {
                Ok((micros, failed)) => {
                    samples.push(micros);
                    errors += usize::from(failed);
                }
                Err(_) => errors += 1,
            }
        }
        let elapsed = started.elapsed();
        samples.sort_unstable();
        let percentile = |percent: usize| {
            samples
                .get((samples.len() * percent).div_ceil(100).saturating_sub(1))
                .copied()
                .unwrap_or(0)
        };
        let after = resolver.metrics().snapshot();
        println!(
            "{}",
            json!({
                "scenario": name, "queries": queries, "concurrency": concurrency,
                "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
                "qps": queries as f64 / elapsed.as_secs_f64(),
                "p50_us": percentile(50), "p95_us": percentile(95), "p99_us": percentile(99),
                "errors": errors, "primary_delay_ms": delay_ms,
                "secondary_delay_ms": 2,
                "upstream_primary": primary.count.load(Ordering::Relaxed) - before_primary,
                "upstream_secondary": secondary.count.load(Ordering::Relaxed) - before_secondary,
                "cache_hits": after.counters["cache_hits"] - before.counters["cache_hits"],
                "cache_misses": after.counters["cache_misses"] - before.counters["cache_misses"],
                "synthetic": true
            })
        );
        ensure!(errors == 0, "{name}: {errors} query errors");
        Ok(())
    }
    .await;
    // Always stop and join both mocks, including if a benchmark assertion fails.
    let (first, second) = tokio::join!(primary.close(), secondary.close());
    outcome?;
    first?;
    second?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() <= 2,
        "usage: bench [queries=1000] [concurrency=32]"
    );
    let queries = args
        .first()
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(1000);
    let concurrency = args
        .get(1)
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(32);
    ensure!(
        (1..=100_000).contains(&queries),
        "queries must be in 1..=100000"
    );
    ensure!(
        (1..=256).contains(&concurrency),
        "concurrency must be in 1..=256"
    );
    for name in ["warm-cache", "cold-single", "cold-parallel"] {
        scenario(name, queries, concurrency).await?;
    }
    Ok(())
}
