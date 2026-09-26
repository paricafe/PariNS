//! FS1d independent-process comparison. No production engine selection.
//! bench_policy <trie|compact|radix|legacy-check> <natsuki|100k|1m> <list-path>
#[path = "../src/policy/canonical.rs"]
#[allow(dead_code)] // Coordinator-only parser statistics are outside this harness.
mod canonical;
#[path = "../src/policy/compact.rs"]
mod compact;
use compact::{Builder, Format, Group, Index, Limits, radix};
use hickory_proto::rr::Name;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    hint::black_box,
    io::{BufRead, BufReader},
    time::Instant,
};

#[derive(Default)]
struct Trie {
    children: HashMap<Vec<u8>, Trie>,
    flags: [bool; 4],
}
impl Trie {
    fn add(&mut self, mut encoded: &[u8], group: Group) {
        let mut node = self;
        while let Some((&len, rest)) = encoded.split_first() {
            node = node
                .children
                .entry(rest[..usize::from(len)].to_vec())
                .or_default();
            encoded = &rest[usize::from(len)..];
        }
        node.flags[group as usize] = true;
    }
    fn verdict(&self, name: &Name) -> u8 {
        let lower = name.to_lowercase();
        let mut labels = lower.iter().rev().peekable();
        let mut node = self;
        let mut blocked = false;
        while let Some(label) = labels.next() {
            let Some(child) = node.children.get(label) else {
                break;
            };
            node = child;
            let exact = labels.peek().is_none();
            if node.flags[0] || (exact && node.flags[1]) {
                return 2;
            }
            blocked |= node.flags[2] || (exact && node.flags[3]);
        }
        u8::from(blocked)
    }
}
enum Engine {
    Trie(Trie),
    Four(Index),
    Radix(radix::Index),
}
impl Engine {
    fn verdict(&self, name: &Name) -> u8 {
        match self {
            Self::Trie(trie) => trie.verdict(name),
            Self::Four(index) => index
                .lookup(name)
                .map_or(0, |m| if (m.group as usize) < 2 { 2 } else { 1 }),
            Self::Radix(index) => index
                .lookup(name)
                .map_or(0, |m| if (m.group as usize) < 2 { 2 } else { 1 }),
        }
    }
}
fn synthetic(i: usize) -> (String, Group) {
    let i = if i > 0 && i.is_multiple_of(10) {
        i - 1
    } else {
        i
    };
    let group = if i.is_multiple_of(17) {
        Group::AllowExact
    } else if i.is_multiple_of(5) {
        Group::BlockSuffix
    } else {
        Group::BlockExact
    };
    (format!("n{i}.a.long-common-suffix.example"), group)
}
fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
const GROUPS: [&str; 8] = [
    "corpus_root",
    "corpus_child",
    "exact_hit",
    "suffix_hit",
    "allow_exact_parent",
    "allow_suffix_parent",
    "deep_miss",
    "root_miss",
];

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(args.len() >= 4, "engine workload fixed-list-path");
    let (engine_name, workload) = (&args[1], &args[2]);
    if engine_name == "legacy-check" {
        let mut builder = Builder::new(Limits::default()).unwrap();
        let failure = builder
            .parse_source(
                BufReader::new(File::open(&args[3]).unwrap()),
                Format::HostsBlocklist,
                1,
            )
            .unwrap_err();
        assert_eq!(failure.kind, compact::ErrorKind::UnsupportedSyntax);
        assert!(builder.finish().is_err());
        println!(
            "legacy_rejected=true,line={},reason={:?}",
            failure.line, failure.kind
        );
        return;
    }
    let count = match workload.as_str() {
        "natsuki" => 201337,
        "100k" => 100000,
        "1m" => 1000000,
        _ => panic!("workload"),
    };
    let limits = Limits {
        max_rules: 1_010_000,
        max_memory_bytes: if workload == "1m" {
            512 * 1024 * 1024
        } else {
            128 * 1024 * 1024
        },
        retained_bytes: 8192,
    };
    let started = Instant::now();
    let mut builder = Builder::new(limits).unwrap();
    if workload == "natsuki" {
        let stats = builder
            .parse_source(
                BufReader::new(File::open(&args[3]).unwrap()),
                Format::DomainList,
                1,
            )
            .unwrap();
        assert_eq!(stats.input_rules, count);
        assert_eq!(stats.decoded_bytes, 4392582);
    } else {
        for i in 0..count {
            let (domain, group) = synthetic(i);
            builder.add(&domain, group, 0).unwrap();
        }
    }
    // Identical, bounded fixture for exact/suffix allows under a blocked parent.
    for i in 0..64 {
        for (prefix, group) in [
            ("exact", Group::BlockExact),
            ("suffix", Group::BlockSuffix),
            ("parent", Group::BlockSuffix),
            ("exception.parent", Group::AllowExact),
            ("safe.parent", Group::AllowSuffix),
            ("hit.branch", Group::BlockExact),
        ] {
            builder
                .add(&format!("{prefix}.g{i}.checks.invalid"), group, 0)
                .unwrap();
        }
    }
    let parse_ms = millis(started);
    let started = Instant::now();
    let canonical = builder.prepare().unwrap();
    let canonical_ms = millis(started);
    let digest = canonical.semantic_digest;
    let input_rules = canonical.input_rules;
    let started = Instant::now();
    let engine = match engine_name.as_str() {
        "trie" => {
            let mut trie = Trie::default();
            canonical.visit_rules(|key, group, _| trie.add(key, group));
            drop(canonical);
            Engine::Trie(trie)
        }
        "compact" => Engine::Four(canonical.finish().unwrap()),
        "radix" => Engine::Radix(canonical.finish_radix().unwrap()),
        _ => panic!("engine"),
    };
    let physical_ms = millis(started);
    let (index_bytes, index_rules, budget_peak, nodes) = match &engine {
        Engine::Trie(_) => (0, 0, 0, 0),
        Engine::Four(i) => (i.index_bytes(), i.index_rules(), i.peak_bytes, 0),
        Engine::Radix(i) => (i.index_bytes(), i.index_rules, i.peak_bytes, i.node_count()),
    };
    match &engine {
        Engine::Four(i) => {
            assert_eq!(i.semantic_digest, digest);
            assert_eq!(i.input_rules, input_rules);
        }
        Engine::Radix(i) => {
            assert_eq!(i.semantic_digest, digest);
            assert_eq!(i.input_rules, input_rules);
        }
        _ => {}
    }
    println!(
        "kind=build,engine={engine_name},workload={workload},parse_ms={parse_ms:.3},canonical_digest_ms={canonical_ms:.3},physical_ms={physical_ms:.3},index_bytes={index_bytes},input_rules={input_rules},index_rules={index_rules},budget_peak={budget_peak},nodes={nodes},semantic_digest={}",
        hex(digest)
    );
    let mut queries: [Vec<Name>; 8] = std::array::from_fn(|_| Vec::with_capacity(16384));
    let mut append = |domain: String, i: usize| {
        let fixture = i % 64;
        let names = [
            domain.clone(),
            format!("child.{domain}"),
            format!("exact.g{fixture}.checks.invalid"),
            format!("child{i}.suffix.g{fixture}.checks.invalid"),
            format!("exception.parent.g{fixture}.checks.invalid"),
            format!("child{i}.safe.parent.g{fixture}.checks.invalid"),
            format!("miss{i}.branch.g{fixture}.checks.invalid"),
            format!("n{i}.never-policy"),
        ];
        for (group, name) in names.into_iter().enumerate() {
            queries[group].push(Name::from_ascii(name).unwrap());
        }
    };
    if workload == "natsuki" {
        for (i, line) in BufReader::new(File::open(&args[3]).unwrap())
            .lines()
            .step_by(count / 16384)
            .take(16384)
            .enumerate()
        {
            let line = line.unwrap();
            append(line.strip_prefix('.').unwrap().to_owned(), i);
        }
    } else {
        for i in 0..16384 {
            append(synthetic(i * count / 16384).0, i);
        }
    }
    // Every verdict is hashed outside timing, with a separate ordered query hash.
    let mut query_hash = Sha256::new();
    let mut verdict_hash = Sha256::new();
    for (group, names) in queries.iter().enumerate() {
        for name in names {
            let verdict = engine.verdict(name);
            if group >= 2 {
                assert_eq!(
                    verdict,
                    [1, 1, 2, 2, 0, 0][group - 2],
                    "{} {name}",
                    GROUPS[group]
                );
            }
            query_hash.update([group as u8]);
            for label in name.iter() {
                query_hash.update([label.len() as u8]);
                query_hash.update(label);
            }
            query_hash.update([0]);
            verdict_hash.update([verdict]);
        }
    }
    println!(
        "kind=verification,engine={engine_name},workload={workload},queries=131072,query_digest={},verdict_digest={}",
        hex(query_hash.finalize()),
        hex(verdict_hash.finalize())
    );
    let mut hot: Vec<usize> = (0..8)
        .flat_map(|group| (0..512).map(move |i| group * 16384 + i * 32))
        .collect();
    let mut large: Vec<usize> = (0..131072).collect();
    for indices in [&mut hot, &mut large] {
        let mut seed = 42u64;
        for i in (1..indices.len()).rev() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            indices.swap(i, seed as usize % (i + 1));
        }
    }
    for (set, indices) in [
        ("hot4096", hot.as_slice()),
        ("large131072", large.as_slice()),
    ] {
        for &i in indices {
            black_box(engine.verdict(black_box(&queries[i / 16384][i % 16384])));
        }
        let mut checksum = 0usize;
        let started = Instant::now();
        for _ in 0..(1048576 / indices.len()) {
            for &i in indices {
                checksum += usize::from(black_box(
                    engine.verdict(black_box(&queries[i / 16384][i % 16384])),
                ));
            }
        }
        let batch_ms = millis(started);
        let mut timings = Vec::with_capacity(131072);
        for i in 0..131072 {
            let j = indices[i % indices.len()];
            let name = &queries[j / 16384][j % 16384];
            let begin = Instant::now();
            black_box(engine.verdict(black_box(name)));
            timings.push(begin.elapsed().as_nanos() as u64);
        }
        timings.sort_unstable();
        println!(
            "kind=batch,engine={engine_name},workload={workload},set={set},lookups=1048576,batch_ms={batch_ms:.3},checksum={checksum},timed_p50_ns={},timed_p95_ns={},timed_p99_ns={}",
            timings[65536],
            timings[131072 * 95 / 100],
            timings[131072 * 99 / 100]
        );
    }
    for (group, names) in queries.iter().enumerate() {
        let mut checksum = 0usize;
        let started = Instant::now();
        for _ in 0..8 {
            for name in names {
                checksum += usize::from(black_box(engine.verdict(black_box(name))));
            }
        }
        let batch_ms = millis(started);
        println!(
            "kind=group,engine={engine_name},workload={workload},group={},lookups=131072,batch_ms={batch_ms:.3},checksum={checksum}",
            GROUPS[group]
        );
    }
    if let Engine::Four(index) = &engine
        && let Some(found) = index.lookup(&queries[0][0])
    {
        black_box(index.rule(found.entry));
    }
}
