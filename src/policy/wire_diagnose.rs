//! Bounded in-process cost attribution, not wire throughput or a capacity test.
use super::{Engine, full_policy, hex};
use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, CNAME},
    },
};
use std::{hint::black_box, path::Path, time::Instant};

const SAMPLES: usize = 4096;
const WARMUP: usize = 256;
const OPERATIONS: usize = 262_144;

#[test]
#[ignore = "FS cost attribution; fixed corpus and one explicitly selected engine"]
fn costs() -> Result<()> {
    let rules = std::env::var("PARINS_FS_WIRE_RULES")?;
    let engine = Engine::parse(&std::env::var("PARINS_FS_WIRE_ENGINE")?)?;
    let (policy, input_rules) = full_policy(Path::new(&rules), engine)?;
    let digest = hex(&policy.semantic_digest());
    let target = Name::from_ascii("target.fs-wire.test.")?;
    for (group, cname) in [
        ("cname_cached", true),
        ("cname_miss", true),
        ("upstream_miss", false),
    ] {
        let samples: Vec<_> = (0..SAMPLES)
            .map(|i| -> Result<_> {
                let text = match group {
                    "cname_cached" => "alias.fs-wire.test.".to_owned(),
                    "cname_miss" => format!("q{i}.aliases.fs-wire.test."),
                    _ => format!("q{i}.miss.fs-wire.test."),
                };
                let name = Name::from_ascii(&text)?;
                let mut query = Message::new(i as u16, MessageType::Query, OpCode::Query);
                query.metadata.recursion_desired = true;
                query.add_query(Query::query(name.clone(), RecordType::A));
                let mut response = crate::protocol::error_response(&query, ResponseCode::NoError);
                if cname {
                    response.add_answer(Record::from_rdata(
                        name.clone(),
                        60,
                        RData::CNAME(CNAME(target.clone())),
                    ));
                }
                response.add_answer(Record::from_rdata(
                    if cname { target.clone() } else { name },
                    60,
                    RData::A(A::new(192, 0, 2, 1)),
                ));
                Ok((query, response))
            })
            .collect::<Result<_>>()?;
        // Verify every prebuilt input and both paths before any timing begins.
        for (query, response) in &samples {
            ensure!(
                !policy.blocks(query.queries[0].name()),
                "fixture question blocked"
            );
            ensure!(policy.blocks(&target), "fixture target not blocked");
            ensure!(!policy.blocks_query(query), "query path blocked");
            let mut actual = response.clone();
            ensure!(
                policy.apply_response(query, &mut actual) == cname,
                "response decision"
            );
            let expected = if cname {
                crate::protocol::error_response(query, ResponseCode::NoError)
            } else {
                response.clone()
            };
            ensure!(
                actual.to_vec()? == expected.to_vec()?,
                "complete response mismatch"
            );
        }
        for path in ["blocks_only", "query_and_response"] {
            let operation = |i: usize| -> usize {
                let (query, response) = black_box(&samples[i % SAMPLES]);
                let policy = black_box(&policy);
                if path == "blocks_only" {
                    let name = query.queries[0].name();
                    usize::from(black_box(policy.blocks(black_box(name))))
                        + usize::from(black_box(policy.blocks(black_box(name))))
                        + if cname {
                            usize::from(black_box(policy.blocks(black_box(&target))))
                        } else {
                            0
                        }
                } else {
                    // Same orchestration calls; excludes cache/network, includes clone.
                    let query_blocked = black_box(policy.blocks_query(black_box(query)));
                    let mut actual = black_box(response).clone();
                    let blocked = policy.apply_response(black_box(query), &mut actual);
                    black_box(&actual);
                    usize::from(query_blocked) + usize::from(black_box(blocked))
                }
            };
            let warmup_checksum: usize = (0..WARMUP).map(&operation).sum();
            ensure!(
                warmup_checksum == WARMUP * usize::from(cname),
                "warmup checksum"
            );
            let start = Instant::now();
            let mut checksum = 0usize;
            for i in 0..OPERATIONS {
                checksum += black_box(operation(black_box(i)));
            }
            let elapsed = start.elapsed();
            ensure!(
                checksum == OPERATIONS * usize::from(cname),
                "measured checksum"
            );
            println!(
                "PARINS_FS_COST {}",
                serde_json::json!({
                    "engine": engine.name(), "group": group, "path": path,
                    "semantic_digest": digest, "input_rules": input_rules,
                    "prebuilt_samples": SAMPLES, "warmup_operations": WARMUP,
                    "operations": OPERATIONS, "blocks_calls_per_operation": if cname { 3 } else { 2 },
                    "elapsed_ns": elapsed.as_nanos(),
                    "ns_per_operation": elapsed.as_nanos() as f64 / OPERATIONS as f64,
                    "checksum": checksum, "warmup_checksum": warmup_checksum,
                    "response_clone_included": path == "query_and_response",
                    "scope": "in-process policy calls only; not wire throughput or deployment capacity",
                    "timing": "one timer around entire batch; no per-operation latency percentiles",
                    "correctness_passed": true
                })
            );
        }
    }
    Ok(())
}
