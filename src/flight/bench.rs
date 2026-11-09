//! Manual registry CPU microbenchmark; no network IO or wire-QPS claim.
use super::*;
use hickory_proto::{
    op::{MessageType, OpCode, Query},
    rr::{Name, RecordType},
};
use std::{hint::black_box, time::Instant};

// Enabled only by the opt-in concurrent benchmark, with preallocated per-thread
// buffers. Production builds contain no timers, buffers or measurement branches.
thread_local! {
    static LOCK_WAITS: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn before_lock() -> Option<Instant> {
    LOCK_WAITS.with_borrow(|samples| samples.as_ref().map(|_| Instant::now()))
}
pub(super) fn after_lock(started: Option<Instant>) {
    if let Some(started) = started {
        let elapsed = started.elapsed().as_nanos() as u64;
        LOCK_WAITS.with_borrow_mut(|samples| samples.as_mut().unwrap().push(elapsed));
    }
}

fn query(index: usize) -> Message {
    let mut q = Message::new(0, MessageType::Query, OpCode::Query);
    q.add_query(Query::query(
        Name::from_ascii(format!("q{index}.bench.test.")).unwrap(),
        RecordType::A,
    ));
    q
}

#[test]
#[ignore = "manual registry CPU benchmark; run release with --ignored --nocapture"]
fn registry() {
    const OPERATIONS: usize = 3000;
    for round in 0..5 {
        for groups in [128, 1024, 4096] {
            for case in ["joined", "turnover"] {
                let flights = Flights::new(CoalescingConfig {
                    enabled: true,
                    max_groups: groups,
                    max_waiters: 64,
                });
                let sent = Arc::new(AtomicBool::new(false));
                let work = || futures_util::future::pending::<Answer>().boxed();
                let queries: Vec<_> = (0..groups).map(query).collect();
                let mut holds: Vec<_> = queries
                    .iter()
                    .map(|q| {
                        let (hold, role, _) = flights.join(q, work(), sent.clone()).unwrap();
                        assert!(matches!(role, Role::Leader));
                        Some(hold)
                    })
                    .collect();
                let mut samples = Vec::with_capacity(OPERATIONS);
                let started = Instant::now();
                for index in 0..OPERATIONS {
                    let slot = index % groups;
                    let replacement;
                    let q = if case == "turnover" {
                        drop(holds[slot].take());
                        replacement = query(groups + index);
                        &replacement
                    } else {
                        &queries[slot]
                    };
                    let begin = Instant::now();
                    let (hold, role, _) = flights.join(q, work(), sent.clone()).unwrap();
                    samples.push(begin.elapsed().as_nanos() as u64);
                    if case == "turnover" {
                        assert!(matches!(role, Role::Leader));
                        holds[slot] = Some(hold);
                    } else {
                        assert!(matches!(role, Role::Joined));
                        drop(black_box(hold));
                    }
                }
                let elapsed = started.elapsed();
                samples.sort_unstable();
                println!(
                    "FLIGHT_BENCH {}",
                    serde_json::json!({
                        "case":case,"round":round,"groups":groups,"operations":OPERATIONS,
                        "elapsed_ns":elapsed.as_nanos() as u64,
                        "p50_ns":samples[OPERATIONS/2],"p95_ns":samples[OPERATIONS*95/100],
                        "p99_ns":samples[OPERATIONS*99/100],"synthetic":true,"network":false,
                    })
                );
                assert_eq!(flights.groups.lock().unwrap().len(), groups);
            }
        }
    }
}

#[test]
#[ignore = "manual concurrent registry benchmark; run release with --ignored --nocapture"]
fn concurrent_registry() {
    for round in 0..5 {
        for groups in [128, 4096] {
            for threads in [1, 4, 8] {
                for case in ["hotspot", "joined", "turnover"] {
                    concurrent_case(round, groups, threads, case);
                }
            }
        }
    }
}

fn concurrent_case(round: usize, groups: usize, threads: usize, case: &str) {
    const OPERATIONS: usize = 3000;
    let flights = Flights::new(CoalescingConfig {
        enabled: true,
        max_groups: groups,
        max_waiters: 64,
    });
    let sent = Arc::new(AtomicBool::new(false));
    let work = || futures_util::future::pending::<Answer>().boxed();
    let queries: Vec<_> = (0..groups).map(query).collect();
    let mut partitions: Vec<Vec<Option<Hold>>> = (0..threads).map(|_| Vec::new()).collect();
    for (index, q) in queries.iter().enumerate() {
        let (hold, role, _) = flights.join(q, work(), sent.clone()).unwrap();
        assert!(matches!(role, Role::Leader));
        partitions[index % threads].push(Some(hold));
    }
    assert!(flights.join(&query(groups), work(), sent.clone()).is_err());
    let ready = std::sync::Barrier::new(threads + 1);
    let (workers, elapsed) = std::thread::scope(|scope| {
        let workers: Vec<_> = partitions
            .into_iter()
            .enumerate()
            .map(|(thread, mut holds)| {
                let flights = &flights;
                let queries = &queries;
                let ready = &ready;
                let sent = sent.clone();
                scope.spawn(move || {
                    let mut samples = Vec::with_capacity(OPERATIONS);
                    LOCK_WAITS
                        .with_borrow_mut(|waits| *waits = Some(Vec::with_capacity(OPERATIONS)));
                    ready.wait();
                    for index in 0..OPERATIONS {
                        let slot = index % holds.len();
                        let replacement;
                        let q = match case {
                            "turnover" => {
                                drop(holds[slot].take());
                                replacement = query(groups + thread * OPERATIONS + index);
                                &replacement
                            }
                            "hotspot" => &queries[0],
                            _ => &queries[slot * threads + thread],
                        };
                        let begin = Instant::now();
                        let (hold, role, _) = flights
                            .join(q, work(), sent.clone())
                            .expect("bounded active groups have room for every replacement");
                        samples.push(begin.elapsed().as_nanos() as u64);
                        if case == "turnover" {
                            assert!(matches!(role, Role::Leader));
                            holds[slot] = Some(hold);
                        } else {
                            assert!(matches!(role, Role::Joined));
                            drop(black_box(hold));
                        }
                    }
                    let waits = LOCK_WAITS.with_borrow_mut(|waits| waits.take().unwrap());
                    (samples, waits, holds)
                })
            })
            .collect();
        let started = Instant::now();
        ready.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        (results, started.elapsed())
    });
    let mut samples = Vec::new();
    let mut waits = Vec::new();
    let mut retained = Vec::new();
    for (worker_samples, worker_waits, holds) in workers {
        samples.extend(worker_samples);
        waits.extend(worker_waits);
        retained.extend(holds.into_iter().flatten());
    }
    let count = threads * OPERATIONS;
    assert_eq!(samples.len(), count);
    assert_eq!(waits.len(), count);
    assert_eq!(retained.len(), groups);
    assert_eq!(flights.groups.lock().unwrap().len(), groups);
    samples.sort_unstable();
    waits.sort_unstable();
    println!(
        "FLIGHT_CONCURRENT {}",
        serde_json::json!({
            "round":round,"case":case,"groups":groups,"threads":threads,"operations":count,
            "elapsed_ns":elapsed.as_nanos() as u64,"operations_per_second":count as f64 / elapsed.as_secs_f64(),
            "join_p50_ns":samples[count/2],"join_p99_ns":samples[count*99/100],
            "lock_p50_ns":waits[count/2],"lock_p99_ns":waits[count*99/100],
            "errors":0,"network":false,"test_instrumentation":true,
        })
    );
}

#[tokio::test]
async fn a_completed_group_with_a_live_holder_is_replaced_and_its_old_hold_cannot_remove_the_new_group()
 {
    let flights = Flights::new(CoalescingConfig {
        enabled: true,
        max_groups: 1,
        max_waiters: 2,
    });
    let q = query(0);
    let sent = Arc::new(AtomicBool::new(false));
    let completed = async {
        Answer {
            response: Err(()),
            stale_eligible: false,
            admitted: false,
            upstream: None,
            outgoing_ecs: None,
            cached: false,
            cache_store: None,
            cache_scope: None,
            upstream_trace: None,
            failure_stage: None,
            failure_reason: None,
            prefetch: false,
        }
    }
    .boxed();
    let (mut old, ..) = flights.join(&q, completed, sent.clone()).unwrap();
    old.result().await;
    let (current, role, _) = flights
        .join(&q, futures_util::future::pending().boxed(), sent.clone())
        .unwrap();
    assert!(matches!(role, Role::Leader));
    drop(old);
    let (follower, role, _) = flights
        .join(&q, futures_util::future::pending().boxed(), sent)
        .unwrap();
    assert!(matches!(role, Role::Joined));
    drop((current, follower));
}
