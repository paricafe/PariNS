use super::*;
use crate::https_reader::WireResponse;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use std::fs;

fn response(body: &[u8]) -> WireResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    headers.insert(header::ETAG, HeaderValue::from_static("\"accepted\""));
    WireResponse {
        status: StatusCode::OK,
        headers,
        body: Box::pin(futures_util::stream::iter(
            body.chunks(3)
                .map(Bytes::copy_from_slice)
                .map(Ok)
                .collect::<Vec<_>>(),
        )),
        _connection: None,
    }
}

async fn fixture(
    foundation: &mut Foundation,
    source: &Source,
    body: &[u8],
    now: u64,
) -> Result<Prepared> {
    let response = response(body);
    prepare_with(
        &mut foundation.store,
        source,
        foundation.limits,
        now,
        |mut file| async move {
            let result =
                download::download_fixture(&source.url, None, None, &mut file, vec![response])
                    .await;
            (file, result)
        },
    )
    .await
}

fn open(path: &Path) -> Foundation {
    Foundation::open(
        &path.join("subscriptions"),
        64 * 1024 * 1024,
        Limits::default(),
        100,
    )
    .unwrap()
}

#[test]
fn fingerprint_binds_canonical_url_format_and_parser_version() {
    let a = Source::new("https://EXAMPLE.COM:443/a/../list", Format::DomainList).unwrap();
    let b = Source::new("https://example.com/list", Format::DomainList).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_ne!(
        a.fingerprint,
        Source::new("https://example.com/list", Format::HostsBlocklist)
            .unwrap()
            .fingerprint
    );
    assert_ne!(
        a.fingerprint,
        Source::new("https://example.com/other", Format::DomainList)
            .unwrap()
            .fingerprint
    );
    assert!(Source::new("http://example.com/list", Format::DomainList).is_err());
}

#[tokio::test]
async fn downloaded_parsed_preparation_survives_reopen_without_activation_or_network() {
    let dir = tempfile::tempdir().unwrap();
    let source = Source::new("https://example.com/list", Format::DomainList).unwrap();
    let mut foundation = open(dir.path());
    let prepared = fixture(
        &mut foundation,
        &source,
        b"\xef\xbb\xbf# source\r\n.Example.COM\r\n.exAMPle.com\nplain.test\n",
        101,
    )
    .await
    .unwrap();
    assert_eq!(prepared.stats.input_rules, 3);
    assert_eq!(prepared.stats.duplicates, 1);
    assert_eq!(prepared.content_commit, Some(CommitOutcome::Durable));
    assert!(prepared.memory.retained_bytes >= PARSE_BUFFER);
    assert!(prepared.memory.peak_bytes <= Limits::default().max_memory_bytes);
    let path = dir.path().join("subscriptions");
    let catalog = fs::read(path.join("catalog.json")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
    assert_eq!(value["current"], serde_json::json!([]));
    assert_eq!(value["previous"], serde_json::json!([]));
    assert_eq!(value["records"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read_dir(path.join("indexes")).unwrap().count(), 0);
    drop(foundation);
    let mut reopened = open(dir.path());
    let reused = prepare_with(
        &mut reopened.store,
        &source,
        reopened.limits,
        102,
        |_| async { panic!("verified preparation must not download") },
    )
    .await
    .unwrap();
    assert_eq!(reused.content_commit, None);
    assert_eq!(reused.fingerprint, prepared.fingerprint);
    assert_eq!(reused.sha256, prepared.sha256);
    assert_eq!(reused.semantic_digest, prepared.semantic_digest);
    assert_eq!(reused.stats, prepared.stats);
    assert_eq!(fs::read(path.join("catalog.json")).unwrap(), catalog);
}

#[tokio::test]
async fn invalid_candidate_preserves_existing_content_and_validator() {
    let dir = tempfile::tempdir().unwrap();
    let mut foundation = open(dir.path());
    let source = Source::new("https://example.com/good", Format::HostsBlocklist).unwrap();
    fixture(
        &mut foundation,
        &source,
        b"0.0.0.0 a.test b.test\n::1 c.test\n",
        101,
    )
    .await
    .unwrap();
    let path = dir.path().join("subscriptions");
    let catalog = fs::read(path.join("catalog.json")).unwrap();
    let invalid = Source::new("https://example.com/bad", Format::DomainList).unwrap();
    let error = fixture(
        &mut foundation,
        &invalid,
        b"valid.test\n||not-supported.test^\n",
        102,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("line: 2"));
    assert!(!error.to_string().contains("not-supported.test"));
    assert_eq!(fs::read(path.join("catalog.json")).unwrap(), catalog);
    assert_eq!(fs::read_dir(path.join("objects")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(path.join("staging")).unwrap().count(), 0);
    let repaired = fixture(&mut foundation, &invalid, b"valid.test\n", 103)
        .await
        .unwrap();
    assert_eq!(repaired.stats.input_rules, 1);
}

#[tokio::test]
async fn truncated_download_and_rule_limit_leave_no_selected_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let mut foundation = open(dir.path());
    let source = Source::new("https://example.com/list", Format::DomainList).unwrap();
    let path = dir.path().join("subscriptions");
    let before = fs::read(path.join("catalog.json")).unwrap();
    let mut truncated = response(b"good.test\n");
    truncated
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("100"));
    let url = source.url.clone();
    assert!(
        prepare_with(
            &mut foundation.store,
            &source,
            foundation.limits,
            101,
            |mut file| async move {
                let result =
                    download::download_fixture(&url, None, None, &mut file, vec![truncated]).await;
                (file, result)
            }
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(path.join("catalog.json")).unwrap(), before);
    foundation.limits.max_rules = 1;
    let error = fixture(&mut foundation, &source, b"same.test\nsame.test\n", 102)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("RuleLimit"));
    assert_eq!(fs::read(path.join("catalog.json")).unwrap(), before);
    assert_eq!(fs::read_dir(path.join("staging")).unwrap().count(), 0);
}
