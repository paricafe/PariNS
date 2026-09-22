//! Reproducible, in-process cache benchmark; this does not measure DNS socket QPS.
//! Run `cargo run --release --example bench_cache -- --seconds 1 --repeats 3`.

use std::{
    hint::black_box,
    sync::{Arc, Barrier, Mutex},
    thread,
    time::{Duration, Instant},
};

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::opt::ClientSubnet,
        rdata::{A, SOA},
    },
};
use parins::{cache::Cache, config::CacheConfig, ecs::Scope, protocol};

// The only adapter changed when comparing the original caller-locked cache with
// the concurrent implementation. Workloads and measurements stay identical.
struct SharedCache(Mutex<Cache>);
impl SharedCache {
    fn new() -> Self {
        Self(Mutex::new(Cache::new(CacheConfig::default())))
    }
    fn insert(&self, fixture: &Fixture, now: Instant) {
        self.0
            .lock()
            .unwrap()
            .insert(&fixture.query, &fixture.response, fixture.scope, now);
    }
    fn get(&self, fixture: &Fixture, now: Instant) -> Option<(Message, Scope)> {
        self.0
            .lock()
            .unwrap()
            .get(&fixture.query, fixture.outgoing, now)
    }
}

struct Fixture {
    query: Message,
    response: Message,
    wire: Vec<u8>,
    outgoing: Option<ClientSubnet>,
    scope: Scope,
}

fn fixture(index: usize, ecs: bool, negative: bool) -> Fixture {
    let name =
        Name::from_ascii(format!("key-{}.bench.test.", if ecs { 0 } else { index })).unwrap();
    let mut query = Message::new(index as u16, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(name.clone(), RecordType::A));
    let (scope, outgoing) = if ecs {
        let network = format!("10.23.{index}.0/24");
        let subnet: ClientSubnet = network.parse().unwrap();
        parins::ecs::set_subnet(&mut query, Some(subnet));
        (Scope::Network(network.parse().unwrap()), Some(subnet))
    } else {
        (Scope::NoEcs, None)
    };
    let mut response = protocol::error_response(
        &query,
        if negative {
            ResponseCode::NXDomain
        } else {
            ResponseCode::NoError
        },
    );
    if negative {
        response.add_authority(Record::from_rdata(
            Name::from_ascii("bench.test.").unwrap(),
            120,
            RData::SOA(SOA::new(
                Name::from_ascii("ns.bench.test.").unwrap(),
                Name::from_ascii("hostmaster.bench.test.").unwrap(),
                1,
                60,
                60,
                3600,
                120,
            )),
        ));
    } else {
        response.add_answer(Record::from_rdata(
            name,
            120,
            RData::A(A::new(192, 0, 2, (index % 254 + 1) as u8)),
        ));
    }
    let wire = response.to_vec().unwrap();
    Fixture {
        query,
        response,
        wire,
        outgoing,
        scope,
    }
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Hot,
    Multi,
    Ecs,
    NegativeChurn,
    Decode,
    Clone,
}

fn run(workload: Workload, workers: usize, duration: Duration) {
    let count = match workload {
        Workload::Multi => 2048,
        Workload::Ecs => 64,
        Workload::NegativeChurn => 8192,
        _ => 1,
    };
    let fixtures = Arc::new(
        (0..count)
            .map(|i| {
                fixture(
                    i,
                    matches!(workload, Workload::Ecs),
                    matches!(workload, Workload::NegativeChurn),
                )
            })
            .collect::<Vec<_>>(),
    );
    let cache = Arc::new(SharedCache::new());
    let inserted = Instant::now();
    if !matches!(workload, Workload::NegativeChurn) {
        for fixture in fixtures.iter() {
            cache.insert(fixture, inserted);
        }
    }
    // Synthetic time makes TTL checks identical even if the machine is busy.
    let lookup_time = inserted + Duration::from_secs(5);
    let barrier = Arc::new(Barrier::new(workers + 1));
    let threads = (0..workers)
        .map(|worker| {
            let cache = cache.clone();
            let fixtures = fixtures.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut state = 0x1234_5678_u64 + worker as u64;
                let mut ops = 0_u64;
                let mut hits = 0_u64;
                let mut errors = 0_u64;
                let mut samples = Vec::with_capacity(32768);
                barrier.wait();
                let start = Instant::now();
                loop {
                    for _ in 0..256 {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let fixture = &fixtures[state as usize % fixtures.len()];
                        let sampled = ops.is_multiple_of(64);
                        let clock = sampled.then(Instant::now);
                        match workload {
                            Workload::Decode => {
                                black_box(protocol::decode(black_box(&fixture.wire)).unwrap());
                            }
                            Workload::Clone => {
                                black_box(black_box(&fixture.response).clone());
                            }
                            Workload::NegativeChurn => {
                                if let Some((response, scope)) = cache.get(fixture, lookup_time) {
                                    hits += 1;
                                    if response.id != fixture.query.id
                                        || response.response_code != ResponseCode::NXDomain
                                        || scope != fixture.scope
                                    {
                                        errors += 1;
                                    }
                                } else {
                                    cache.insert(fixture, inserted);
                                }
                            }
                            _ => match cache.get(fixture, lookup_time) {
                                Some((response, scope)) => {
                                    hits += 1;
                                    if response.id != fixture.query.id
                                        || response.queries != fixture.query.queries
                                        || response.answers.len() != 1
                                        || response.answers[0].ttl != 115
                                        || response.answers[0].data
                                            != fixture.response.answers[0].data
                                        || scope != fixture.scope
                                    {
                                        errors += 1;
                                    }
                                    black_box(response);
                                }
                                None => errors += 1,
                            },
                        }
                        if let Some(clock) = clock {
                            samples.push(clock.elapsed().as_nanos() as u64);
                        }
                        ops += 1;
                    }
                    if start.elapsed() >= duration {
                        break;
                    }
                }
                (ops, hits, errors, samples, start.elapsed())
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut ops = 0;
    let mut hits = 0;
    let mut errors = 0;
    let mut samples = Vec::new();
    let mut elapsed = Duration::ZERO;
    for thread in threads {
        let (worker_ops, worker_hits, worker_errors, worker_samples, worker_elapsed) =
            thread.join().unwrap();
        ops += worker_ops;
        hits += worker_hits;
        errors += worker_errors;
        samples.extend(worker_samples);
        elapsed = elapsed.max(worker_elapsed);
    }
    samples.sort_unstable();
    println!(
        "{workload:?},{workers},{:.3},{ops},{hits},{errors},{:.0},{},{}",
        elapsed.as_secs_f64(),
        ops as f64 / elapsed.as_secs_f64(),
        samples[samples.len() * 99 / 100],
        samples.len()
    );
    assert_eq!(errors, 0, "cache semantic regression in {workload:?}");
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let value = |name: &str, default: usize| {
        args.iter()
            .position(|arg| arg == name)
            .map_or(default, |index| {
                args.get(index + 1)
                    .expect("missing option value")
                    .parse()
                    .expect("invalid number")
            })
    };
    let seconds = value("--seconds", 1);
    let repeats = value("--repeats", 3);
    assert!(seconds > 0 && repeats > 0);
    println!(
        "# in-process cache; fixed seed; prebuilt queries; latency sample 1/64; synthetic TTL age 5s; no DNS sockets"
    );
    println!(
        "workload,workers,seconds,operations,hits,errors,ops_per_second,sampled_p99_ns,samples"
    );
    for _ in 0..repeats {
        for workload in [
            Workload::Hot,
            Workload::Multi,
            Workload::Ecs,
            Workload::NegativeChurn,
            Workload::Decode,
            Workload::Clone,
        ] {
            for workers in [1, 2, 4] {
                run(workload, workers, Duration::from_secs(seconds as u64));
            }
        }
    }
}
