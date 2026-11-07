//! Manual registry CPU microbenchmark; no network IO or wire-QPS claim.
use super::*;
use hickory_proto::{
    op::{MessageType, OpCode, Query},
    rr::{Name, RecordType},
};
use std::{hint::black_box, time::Instant};

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
