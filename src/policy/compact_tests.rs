use super::*;
use std::io::{BufReader, Cursor};

fn parse(text: &[u8], format: Format) -> Result<Index, Error> {
    let mut builder = Builder::new(Limits::default()).unwrap();
    builder.parse_source(BufReader::with_capacity(3, Cursor::new(text)), format, 0)?;
    builder.finish()
}

#[test]
fn streaming_bom_comments_crlf_and_sinkhole_aliases() {
    let index = parse(
        b"\xef\xbb\xbf! first\r\n# second\r\n\r\n.EXAMPLE.com. # comment\r\nexact.test\n",
        Format::DomainList,
    )
    .unwrap();
    assert_eq!(index.input_rules, 2);
    assert!(
        index
            .lookup(&Name::from_ascii("a.example.com").unwrap())
            .is_some()
    );
    assert!(
        index
            .lookup(&Name::from_ascii("a.exact.test").unwrap())
            .is_none()
    );
    let index = parse(
        b"0.0.0.0 a.test B.test. # aliases\r\n127.0.0.1 c.test\n:: d.test\n0:0:0:0:0:0:0:1 e.test",
        Format::HostsBlocklist,
    )
    .unwrap();
    assert_eq!(index.input_rules, 5);
    for name in ["a.test", "b.test", "c.test", "d.test", "e.test"] {
        assert!(index.lookup(&Name::from_ascii(name).unwrap()).is_some());
    }
    assert!(
        index
            .lookup(&Name::from_ascii("x.a.test").unwrap())
            .is_none()
    );
}

#[test]
fn rejects_entire_source_with_stable_line_and_no_remote_text() {
    for text in [
        "||example.com^",
        "*.example.com",
        "/example/",
        "<html>",
        "https://example.com",
        "é.test",
        "..example.com",
        "a..com",
        "a\\.b.com",
        "a b",
        "a#comment",
    ] {
        let mut builder = Builder::new(Limits::default()).unwrap();
        let input = format!("safe.test\n{text}\n");
        let failure = builder
            .parse_source(input.as_bytes(), Format::DomainList, 0)
            .unwrap_err();
        assert_eq!(failure.line, 2, "{text}");
        assert_eq!(builder.finish().err(), Some(failure));
    }
    assert_eq!(
        parse(b"# comment\n! comment\n", Format::DomainList)
            .err()
            .unwrap()
            .kind,
        ErrorKind::EmptySource
    );
    assert_eq!(
        parse(b"a.test\n\xff", Format::DomainList)
            .err()
            .unwrap()
            .kind,
        ErrorKind::InvalidUtf8
    );
    assert_eq!(
        parse(b"192.0.2.1 a.test", Format::HostsBlocklist)
            .err()
            .unwrap()
            .kind,
        ErrorKind::HostsRewrite
    );
    assert_eq!(
        parse(b"127.0.0.2 a.test", Format::HostsBlocklist)
            .err()
            .unwrap()
            .kind,
        ErrorKind::HostsRewrite
    );
    for input in [
        b"||example.com^".as_slice(),
        b"0.0.0.0 .example.com",
        b"0.0.0.0",
    ] {
        assert!(parse(input, Format::HostsBlocklist).is_err());
    }
    assert_eq!(
        parse(&vec![b'a'; 4097], Format::DomainList)
            .err()
            .unwrap()
            .kind,
        ErrorKind::LineLimit
    );
}

#[test]
fn growth_rejects_transient_peak_before_allocating_new_record_buffer() {
    let mut builder = Builder::new(Limits {
        max_memory_bytes: 40,
        ..Limits::default()
    })
    .unwrap();
    builder.add("a", Group::BlockExact, 0).unwrap();
    // Final two-entry data would be 36 B, but record growth needs the old 16 B
    // and new 32 B simultaneously, in addition to 4 arena bytes.
    let failure = builder.add("b", Group::BlockExact, 0).unwrap_err();
    assert_eq!(failure.kind, ErrorKind::MemoryLimit);
    assert_eq!(builder.records.capacity(), 1);
    assert_eq!(builder.budget.peak, 22);
    assert_eq!(builder.finish().err(), Some(failure));
    let mut builder = Builder::new(Limits {
        max_memory_bytes: 27,
        ..Limits::default()
    })
    .unwrap();
    builder.add("a", Group::BlockExact, 0).unwrap();
    // Input 18 B + final arena 2 B + final table 8 B = 28 B.
    assert_eq!(builder.finish().err().unwrap().kind, ErrorKind::MemoryLimit);
}

#[test]
fn duplicate_and_alias_limits_are_before_deduplication() {
    for text in ["a.test\na.test\na.test", "0.0.0.0 a.test a.test a.test"] {
        let mut builder = Builder::new(Limits {
            max_rules: 2,
            ..Limits::default()
        })
        .unwrap();
        let format = if text.starts_with('0') {
            Format::HostsBlocklist
        } else {
            Format::DomainList
        };
        assert_eq!(
            builder
                .parse_source(text.as_bytes(), format, 0)
                .unwrap_err()
                .kind,
            ErrorKind::RuleLimit
        );
    }
}

#[test]
fn digest_is_independent_of_source_order_witness_duplicates_and_pruned_rules() {
    let mut a = Builder::new(Limits::default()).unwrap();
    let mut b = Builder::new(Limits::default()).unwrap();
    a.add("example.com", Group::BlockSuffix, 1).unwrap();
    a.add("safe.example.com", Group::AllowExact, 0).unwrap();
    b.add("SAFE.EXAMPLE.COM.", Group::AllowExact, 4).unwrap();
    b.add("example.com.", Group::BlockSuffix, 2).unwrap();
    b.add("a.example.com", Group::BlockSuffix, 1).unwrap();
    b.add("a.example.com", Group::BlockExact, 0).unwrap();
    b.add("example.com", Group::BlockSuffix, 6).unwrap();
    let (a, b) = (a.finish().unwrap(), b.finish().unwrap());
    assert_eq!(a.semantic_digest, b.semantic_digest);
    assert_ne!(
        a.lookup(&Name::from_ascii("example.com").unwrap())
            .unwrap()
            .entry
            .source_slot,
        b.lookup(&Name::from_ascii("example.com").unwrap())
            .unwrap()
            .entry
            .source_slot
    );
}

#[test]
fn raw_labels_root_maximum_length_and_label_boundaries() {
    let mut builder = Builder::new(Limits::default()).unwrap();
    builder.add("example.com", Group::BlockSuffix, 0).unwrap();
    let maximum = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    assert_eq!(maximum.len(), 253);
    builder.add(&maximum, Group::BlockExact, 0).unwrap();
    let index = builder.finish().unwrap();
    assert!(index.lookup(&Name::from_ascii(&maximum).unwrap()).is_some());
    for name in [
        Name::root(),
        Name::from_labels([b"badexample".as_slice(), b"com"]).unwrap(),
        Name::from_labels([b"a.example".as_slice(), b"com"]).unwrap(),
        Name::from_labels([b"example".as_slice(), b"com", b"other"]).unwrap(),
    ] {
        assert!(index.lookup(&name).is_none(), "{name}");
    }
    for raw in [b"\xff".as_slice(), b".", b"\\"] {
        assert!(
            index
                .lookup(&Name::from_labels([raw, b"example", b"com"]).unwrap())
                .is_some()
        );
    }
}

// Deliberately uses raw label vectors and a linear scan, not compact encoding or
// prefix predecessors, so it can detect mistakes in either representation.
fn oracle(rules: &[(Name, Group)], query: &Name) -> Option<bool> {
    let query: Vec<_> = query
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect();
    let mut block = false;
    for (name, group) in rules {
        let rule: Vec<_> = name
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect();
        let matches = if (*group as usize).is_multiple_of(2) {
            query.ends_with(&rule)
        } else {
            query == rule
        };
        if matches {
            if (*group as usize) < 2 {
                return Some(false);
            }
            block = true;
        }
    }
    block.then_some(true)
}

#[test]
fn four_tables_match_naive_label_oracle_on_overlapping_randomized_inputs() {
    let mut builder = Builder::new(Limits::default()).unwrap();
    let mut rules = Vec::new();
    let mut rng = 1_u64;
    for i in 0..800 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let base = format!("d{}.t{}", (rng >> 16) % 47, (rng >> 32) % 11);
        let domain = if i % 3 == 0 {
            format!("a.{base}")
        } else {
            base
        };
        let group = [
            Group::AllowSuffix,
            Group::AllowExact,
            Group::BlockSuffix,
            Group::BlockExact,
        ][(rng >> 48) as usize % 4];
        builder.add(&domain, group, (i % 16) as u16).unwrap();
        rules.push((Name::from_ascii(&domain).unwrap(), group));
    }
    let index = builder.finish().unwrap();
    for tld in 0..13 {
        for domain in 0..49 {
            for prefix in ["", "a.", "z.a.", "bad"] {
                let query = Name::from_ascii(format!("{prefix}d{domain}.t{tld}")).unwrap();
                let actual = index.lookup(&query).map(|found| found.group as usize >= 2);
                assert_eq!(actual, oracle(&rules, &query), "{query}");
            }
        }
    }
}

#[test]
fn an_allowed_query_does_not_exempt_its_cname_chain_targets() {
    let rules = [
        ("safe.test", Group::AllowExact),
        ("blocked.test", Group::BlockSuffix),
    ];
    let mut builder = Builder::new(Limits::default()).unwrap();
    for (name, group) in rules {
        builder.add(name, group, 0).unwrap();
    }
    let index = builder.finish().unwrap();
    let oracle_rules: Vec<_> = rules
        .into_iter()
        .map(|(name, group)| (Name::from_ascii(name).unwrap(), group))
        .collect();
    // The existing response traversal owns reachability/class/loop handling;
    // this exercises the compact decision at each name it supplies, including
    // a repeated name from a cycle. No allow decision can exempt later names.
    let chain: Vec<_> = ["safe.test", "middle.test", "safe.test", "a.blocked.test"]
        .into_iter()
        .map(|name| Name::from_ascii(name).unwrap())
        .collect();
    let decisions: Vec<_> = chain
        .iter()
        .map(|name| index.lookup(name).map(|found| found.group as usize >= 2))
        .collect();
    assert_eq!(
        decisions,
        chain
            .iter()
            .map(|name| oracle(&oracle_rules, name))
            .collect::<Vec<_>>()
    );
    assert_eq!(decisions, [Some(false), None, Some(false), Some(true)]);
}
