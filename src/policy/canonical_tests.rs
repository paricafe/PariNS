use super::*;
use std::io::{BufReader, Cursor, Read};

#[test]
fn source_and_aggregate_counts_preserve_pre_dedup_inputs() {
    let mut builder = Builder::new(Limits {
        retained_bytes: 7,
        ..Limits::default()
    })
    .unwrap();
    let text =
        b"# source\n.EXAMPLE.test.\n.example.test\n.child.example.test\nchild.example.test\n";
    let first = builder
        .parse_source(
            BufReader::with_capacity(7, Cursor::new(text)),
            Format::DomainList,
            1,
        )
        .unwrap();
    assert_eq!(
        (
            first.lines,
            first.input_rules,
            first.duplicates,
            first.decoded_bytes
        ),
        (5, 4, 1, text.len())
    );
    let second = builder
        .parse_source(
            b"example.test\n.example.test\n".as_slice(),
            Format::DomainList,
            2,
        )
        .unwrap();
    assert_eq!((second.input_rules, second.duplicates), (2, 0));
    let live = builder.budget.live;
    assert_eq!(
        live,
        7 + builder.arena.capacity() + builder.records.capacity() * size_of::<Record>()
    );
    assert!(builder.budget.peak >= live);
    let canonical = builder.prepare().unwrap();
    assert_eq!((canonical.input_rules, canonical.records.len()), (6, 1));
    assert_eq!(canonical.budget.live, live); // truncate does not release capacity
    let mut one = Builder::new(Limits::default()).unwrap();
    one.parse_source(b".example.test\n".as_slice(), Format::DomainList, 9)
        .unwrap();
    assert_eq!(
        canonical.semantic_digest,
        one.prepare().unwrap().semantic_digest
    );
}

#[test]
fn formats_are_explicit_and_hard_limits_fail_before_allocation() {
    for (format, name) in [
        (Format::DomainList, "domain_list"),
        (Format::HostsBlocklist, "hosts_blocklist"),
    ] {
        assert_eq!(serde_json::to_value(format).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<Format>(name.into()).unwrap(),
            format
        );
    }
    assert!(serde_json::from_value::<Format>("hosts".into()).is_err());
    assert_eq!(PARSER_VERSION, 1);
    for limits in [
        Limits {
            max_rules: 5_000_001,
            ..Limits::default()
        },
        Limits {
            max_rules: 0,
            ..Limits::default()
        },
        Limits {
            max_memory_bytes: 512 * 1024 * 1024 + 1,
            ..Limits::default()
        },
        Limits {
            max_memory_bytes: 0,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            Builder::new(limits).err().unwrap().kind,
            ErrorKind::InvalidLimits
        );
    }
    assert_eq!(
        Builder::new(Limits {
            retained_bytes: usize::MAX,
            ..Limits::default()
        })
        .err()
        .unwrap()
        .kind,
        ErrorKind::MemoryLimit
    );
    let mut builder = Builder::new(Limits {
        retained_bytes: 20,
        max_memory_bytes: 4115,
        ..Limits::default()
    })
    .unwrap();
    let failure = builder
        .parse_source(b"a\n".as_slice(), Format::DomainList, 0)
        .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::MemoryLimit);
    assert_eq!(builder.arena.capacity(), 0);
    assert_eq!(builder.prepare().err(), Some(failure));
}

#[test]
fn line_boundary_utf8_and_read_failure_poison_the_whole_candidate() {
    let mut maximum = vec![b'#'; 4096];
    maximum.extend_from_slice(b"\na.test");
    let mut builder = Builder::new(Limits::default()).unwrap();
    let stats = builder
        .parse_source(
            BufReader::with_capacity(1, Cursor::new(&maximum)),
            Format::DomainList,
            0,
        )
        .unwrap();
    assert_eq!((stats.lines, stats.input_rules), (2, 1));
    for input in [
        b"a.test\n\xef\xbb\xbfb.test".as_slice(),
        b"a.test\n\xef\xbb",
        b"\xef\xbb\xbf\xef\xbb\xbfa.test",
    ] {
        let mut builder = Builder::new(Limits::default()).unwrap();
        let error = builder
            .parse_source(
                BufReader::with_capacity(1, Cursor::new(input)),
                Format::DomainList,
                0,
            )
            .unwrap_err();
        assert_eq!(builder.prepare().err(), Some(error));
        assert!(!error.to_string().contains("a.test"));
    }
    struct Truncated;
    impl Read for Truncated {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::UnexpectedEof.into())
        }
    }
    let mut builder = Builder::new(Limits::default()).unwrap();
    let reader = Cursor::new(b"valid.test\npartial").chain(Truncated);
    let failure = builder
        .parse_source(BufReader::with_capacity(3, reader), Format::DomainList, 0)
        .unwrap_err();
    assert_eq!(
        failure,
        Error {
            line: 2,
            kind: ErrorKind::Io
        }
    );
    assert_eq!(builder.prepare().err(), Some(failure));
}

/// Generates bounded comments without retaining a large input allocation.
struct Comments {
    remaining: usize,
    offset: usize,
}
impl Read for Comments {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let count = out.len().min(self.remaining);
        for byte in &mut out[..count] {
            *byte = if self.offset % 4096 == 4095 {
                b'\n'
            } else {
                b'#'
            };
            self.offset += 1;
        }
        self.remaining -= count;
        Ok(count)
    }
}
fn bounded_source(bytes: usize) -> impl std::io::BufRead {
    BufReader::with_capacity(
        4096,
        Cursor::new(b"a\n").chain(Comments {
            remaining: bytes - 2,
            offset: 0,
        }),
    )
}

#[test]
fn decoded_source_and_aggregate_limits_are_measured_while_streaming() {
    const SOURCE: usize = 16 * 1024 * 1024;
    let limits = Limits {
        retained_bytes: 4096,
        ..Limits::default()
    };
    let mut builder = Builder::new(limits).unwrap();
    for slot in 0..2 {
        let stats = builder
            .parse_source(bounded_source(SOURCE), Format::DomainList, slot)
            .unwrap();
        assert_eq!((stats.decoded_bytes, stats.input_rules), (SOURCE, 1));
    }
    let failure = builder
        .parse_source(b"a".as_slice(), Format::DomainList, 2)
        .unwrap_err();
    assert_eq!(failure.kind, ErrorKind::InputLimit);
    assert_eq!(builder.prepare().err(), Some(failure));
    let mut builder = Builder::new(limits).unwrap();
    assert_eq!(
        builder
            .parse_source(bounded_source(SOURCE + 1), Format::DomainList, 0)
            .unwrap_err()
            .kind,
        ErrorKind::InputLimit
    );
}

// Parser and builder contracts below were previously exercised through the
// deleted compact index; they now build the production radix index.
fn parse_radix(text: &[u8], format: Format) -> Result<super::super::radix::Index, Error> {
    let mut builder = Builder::new(Limits::default()).unwrap();
    builder.parse_source(BufReader::with_capacity(3, Cursor::new(text)), format, 0)?;
    builder.prepare()?.finish_radix()
}

fn lookup(index: &super::super::radix::Index, name: &str) -> bool {
    index.lookup(&Name::from_ascii(name).unwrap()).is_some()
}

#[test]
fn streaming_bom_comments_crlf_and_sinkhole_aliases() {
    let index = parse_radix(
        b"\xef\xbb\xbf! first\r\n# second\r\n\r\n.EXAMPLE.com. # comment\r\nexact.test\n",
        Format::DomainList,
    )
    .unwrap();
    assert_eq!(index.input_rules, 2);
    assert!(lookup(&index, "a.example.com"));
    assert!(!lookup(&index, "a.exact.test"));
    let index = parse_radix(
        b"0.0.0.0 a.test B.test. # aliases\r\n127.0.0.1 c.test\n:: d.test\n0:0:0:0:0:0:0:1 e.test",
        Format::HostsBlocklist,
    )
    .unwrap();
    assert_eq!(index.input_rules, 5);
    for name in ["a.test", "b.test", "c.test", "d.test", "e.test"] {
        assert!(lookup(&index, name), "{name}");
    }
    assert!(!lookup(&index, "x.a.test"));
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
        assert_eq!(builder.prepare().err(), Some(failure));
    }
    let kind = |text: &[u8], format| parse_radix(text, format).err().unwrap().kind;
    assert_eq!(
        kind(b"# comment\n! comment\n", Format::DomainList),
        ErrorKind::EmptySource
    );
    assert_eq!(
        kind(b"a.test\n\xff", Format::DomainList),
        ErrorKind::InvalidUtf8
    );
    assert_eq!(
        kind(b"192.0.2.1 a.test", Format::HostsBlocklist),
        ErrorKind::HostsRewrite
    );
    assert_eq!(
        kind(b"127.0.0.2 a.test", Format::HostsBlocklist),
        ErrorKind::HostsRewrite
    );
    for input in [
        b"||example.com^".as_slice(),
        b"0.0.0.0 .example.com",
        b"0.0.0.0",
    ] {
        assert!(parse_radix(input, Format::HostsBlocklist).is_err());
    }
    assert_eq!(
        kind(&vec![b'a'; 4097], Format::DomainList),
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
    // Record growth needs the old 16 B and new 32 B simultaneously, in addition
    // to 4 arena bytes, although the final two-entry data would fit.
    let failure = builder.add("b", Group::BlockExact, 0).unwrap_err();
    assert_eq!(failure.kind, ErrorKind::MemoryLimit);
    assert_eq!(builder.records.capacity(), 1);
    assert_eq!(builder.budget.peak, 22);
    assert_eq!(builder.prepare().err(), Some(failure));
}

#[test]
fn duplicate_and_alias_limits_are_before_deduplication() {
    for (text, format) in [
        ("a.test\na.test\na.test", Format::DomainList),
        ("0.0.0.0 a.test a.test a.test", Format::HostsBlocklist),
    ] {
        let mut builder = Builder::new(Limits {
            max_rules: 2,
            ..Limits::default()
        })
        .unwrap();
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
    let a = a.prepare().unwrap().finish_radix().unwrap();
    let b = b.prepare().unwrap().finish_radix().unwrap();
    assert_eq!(a.semantic_digest, b.semantic_digest);
    let name = Name::from_ascii("example.com").unwrap();
    assert_ne!(
        a.lookup(&name).unwrap().source_slot,
        b.lookup(&name).unwrap().source_slot
    );
}
