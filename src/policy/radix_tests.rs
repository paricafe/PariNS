use super::super::{Builder, Limits};
use super::*;

fn compare(rules: &[(String, Group, u16)], queries: &[Name]) {
    let mut a = Builder::new(Limits::default()).unwrap();
    let mut b = Builder::new(Limits::default()).unwrap();
    for (name, group, slot) in rules {
        a.add(name, *group, *slot).unwrap();
        b.add(name, *group, *slot).unwrap();
    }
    let a = a.finish().unwrap();
    let input = b.prepare().unwrap();
    let mut count = 0;
    input.visit_rules(|_, _, _| count += 1);
    let b = input.finish_radix().unwrap();
    assert_eq!(a.semantic_digest, b.semantic_digest);
    assert_eq!(a.input_rules, b.input_rules);
    assert_eq!(count, b.index_rules);
    assert!(b.index_bytes() >= b.node_count() * 20);
    assert!(b.peak_bytes >= b.index_bytes());
    for name in queries {
        let expected = a
            .lookup(name)
            .map(|m| (m.group, m.entry.source_slot, m.entry.len));
        let actual = b
            .lookup(name)
            .map(|m| (m.group, m.source_slot, m.matched_key_len));
        assert_eq!(expected, actual, "{name}");
        let query: Vec<_> = name
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect();
        let mut blocked = false;
        let mut allowed = false;
        for (rule, group, _) in rules {
            let labels: Vec<_> = rule
                .trim_end_matches('.')
                .split('.')
                .map(|label| label.as_bytes().to_ascii_lowercase())
                .collect();
            let matches = if (*group as usize).is_multiple_of(2) {
                query.ends_with(&labels)
            } else {
                query == labels
            };
            if matches {
                if (*group as usize) < 2 {
                    allowed = true;
                } else {
                    blocked = true;
                }
            }
        }
        assert_eq!(
            actual.map(|m| m.0 as usize >= 2),
            if allowed {
                Some(false)
            } else if blocked {
                Some(true)
            } else {
                None
            },
            "oracle {name}"
        );
    }
}

#[test]
fn terminals_internal_splits_allow_and_raw_labels_match_oracle() {
    let mut rules = Vec::new();
    for (rule, group, slot) in [
        ("example.com", Group::BlockSuffix, 3),
        ("safe.example.com", Group::AllowSuffix, 1),
        ("exact.example.com", Group::AllowExact, 0),
        ("a.terminal.test", Group::BlockExact, 1),
        ("terminal.test", Group::BlockExact, 0),
        ("ab.terminal.test", Group::AllowExact, 2),
        ("abc.terminal.test", Group::BlockSuffix, 2),
        ("abx.terminal.test", Group::BlockSuffix, 1),
        ("deep.safe.example.com", Group::BlockExact, 0),
        ("a.example.com", Group::BlockExact, 0),
    ] {
        rules.push((rule.to_owned(), group, slot));
    }
    let maximum = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    rules.push((maximum.clone(), Group::BlockExact, 0));
    let mut queries: Vec<_> = [
        ".",
        "com",
        "test",
        "terminal.test",
        "a.terminal.test",
        "ab.terminal.test",
        "abc.terminal.test",
        "abq.terminal.test",
        "example.com",
        "z.example.com",
        "safe.example.com",
        "deep.safe.example.com",
        "exact.example.com",
        "z.exact.example.com",
        "badexample.com",
    ]
    .into_iter()
    .map(|name| Name::from_ascii(name).unwrap())
    .collect();
    queries.push(Name::from_ascii(maximum).unwrap());
    for raw in [b"\xff".as_slice(), b".", b"\\"] {
        queries.push(Name::from_labels([raw, b"example", b"com"]).unwrap());
    }
    queries.push(Name::from_labels([b"a.example".as_slice(), b"com"]).unwrap());
    compare(&rules, &queries);
}

#[test]
fn randomized_radix_and_four_table_witnesses_match_label_oracle() {
    let mut rules = Vec::new();
    let mut seed = 42u64;
    for i in 0..700 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let base = format!("d{}.t{}", (seed >> 16) % 29, (seed >> 32) % 7);
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
        ][(seed >> 48) as usize % 4];
        rules.push((domain, group, (i % 16) as u16));
    }
    let mut queries = Vec::new();
    for tld in 0..9 {
        for domain in 0..31 {
            for prefix in ["", "a.", "z.a.", "bad"] {
                queries.push(Name::from_ascii(format!("{prefix}d{domain}.t{tld}")).unwrap());
            }
        }
    }
    compare(&rules, &queries);
}

#[test]
fn node_growth_accounts_for_both_old_and_new_buffers() {
    // Input key+record = 18 B and initial root = 20 B. One byte less than
    // their sum plus the explicit Frame reserve must fail before that reserve.
    let mut no_scratch = Builder::new(Limits {
        max_memory_bytes: BUILD_SCRATCH_BYTES + 37,
        ..Limits::default()
    })
    .unwrap();
    no_scratch.add("a", Group::BlockExact, 0).unwrap();
    assert_eq!(
        no_scratch
            .prepare()
            .unwrap()
            .finish_radix()
            .err()
            .unwrap()
            .kind,
        super::super::ErrorKind::MemoryLimit
    );
    let mut invalid = Builder::new(Limits::default()).unwrap();
    assert_eq!(
        invalid
            .add("a", Group::BlockExact, u16::MAX)
            .unwrap_err()
            .kind,
        super::super::ErrorKind::InvalidSource
    );
    let mut builder = Builder::new(Limits {
        max_memory_bytes: 60 + BUILD_SCRATCH_BYTES,
        ..Limits::default()
    })
    .unwrap();
    builder.add("a", Group::BlockExact, 0).unwrap();
    assert_eq!(
        builder
            .prepare()
            .unwrap()
            .finish_radix()
            .err()
            .unwrap()
            .kind,
        super::super::ErrorKind::MemoryLimit
    );
    let index = Builder::new(Limits::default())
        .unwrap()
        .prepare()
        .unwrap()
        .finish_radix()
        .unwrap();
    assert_eq!(index.lookup(&Name::root()), None);
}

#[test]
fn bounded_explicit_frames_handle_deep_byte_branching() {
    let domain = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    let mut rules = vec![(domain.clone(), Group::BlockExact, 0)];
    for (i, byte) in domain.bytes().enumerate() {
        if byte == b'.' {
            continue;
        }
        let mut changed = domain.clone().into_bytes();
        changed[i] = b'z';
        rules.push((String::from_utf8(changed).unwrap(), Group::BlockExact, 0));
    }
    let queries: Vec<_> = rules
        .iter()
        .map(|(name, _, _)| Name::from_ascii(name).unwrap())
        .collect();
    compare(&rules, &queries);
}
