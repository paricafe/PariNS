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
    let before = builder.memory();
    assert_eq!(before.retained_bytes, 7);
    assert_eq!(
        before.live_bytes,
        7 + before.arena_bytes + before.record_bytes
    );
    assert!(before.peak_bytes >= before.live_bytes);
    let canonical = builder.prepare().unwrap();
    assert_eq!(
        (
            canonical.input_rules,
            canonical.duplicates,
            canonical.covered_rules,
            canonical.canonical_rules
        ),
        (6, 2, 3, 1)
    );
    assert_eq!(
        canonical.decoded_bytes,
        first.decoded_bytes + second.decoded_bytes
    );
    assert_eq!(canonical.memory(), before); // truncate does not release capacity
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
    assert_eq!(builder.memory().arena_bytes, 0);
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

#[test]
#[ignore = "requires external fixed corpus, never downloads or bundles third-party lists"]
fn fixed_corpus_full_parse_and_legacy_rejection() {
    let directory = std::path::PathBuf::from(std::env::var("PARINS_FS_CORPUS_DIR").unwrap());
    for (filename, expected_sha, format) in [
        (
            "natsuki-list.list",
            "d68e37b2a861e6e8ef85568db4237bb3e18d1a9f2476323b3dba977fe850af09",
            Format::DomainList,
        ),
        (
            "hosts.txt",
            "0d8f9daf4bd0a3c8d600a8407982466902afea1112d1c73394ec4415ee6ac770",
            Format::HostsBlocklist,
        ),
    ] {
        let path = directory.join(filename);
        let mut file = std::fs::File::open(&path).unwrap();
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 4096];
        loop {
            let count = file.read(&mut bytes).unwrap();
            if count == 0 {
                break;
            }
            hash.update(&bytes[..count]);
        }
        assert_eq!(format!("{:x}", hash.finalize()), expected_sha);
        let reader = BufReader::with_capacity(4096, std::fs::File::open(path).unwrap());
        let mut builder = Builder::new(Limits {
            retained_bytes: reader.capacity(),
            ..Limits::default()
        })
        .unwrap();
        if format == Format::DomainList {
            let stats = builder.parse_source(reader, format, 1).unwrap();
            assert_eq!(
                (
                    stats.lines,
                    stats.input_rules,
                    stats.duplicates,
                    stats.decoded_bytes
                ),
                (201337, 201337, 0, 4392582)
            );
            let canonical = builder.prepare().unwrap();
            assert_eq!(canonical.input_rules, 201337);
            println!(
                "canonical_rules={} duplicates={} covered={} memory={:?}",
                canonical.canonical_rules,
                canonical.duplicates,
                canonical.covered_rules,
                canonical.memory()
            );
        } else {
            let error = builder.parse_source(reader, format, 1).unwrap_err();
            assert_eq!(
                error,
                Error {
                    line: 9,
                    kind: ErrorKind::UnsupportedSyntax
                }
            );
            assert_eq!(builder.prepare().err(), Some(error));
        }
    }
}
