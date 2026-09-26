use super::*;
use std::{collections::VecDeque, future::ready};

fn response(status: u16, headers: &[(&str, &str)], bytes: &[u8]) -> WireResponse {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    WireResponse {
        status: StatusCode::from_u16(status).unwrap(),
        headers: map,
        body: Box::pin(futures_util::stream::iter(
            bytes
                .chunks(7)
                .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )),
        _connection: None,
    }
}
fn binding() -> Validators {
    Validators {
        final_url: "https://example.com/a".into(),
        representation: REPRESENTATION.into(),
        content_sha256: "a".repeat(64),
        etag: Some("\"same\"".into()),
        last_modified: None,
    }
}

#[tokio::test]
async fn subscription_download_slot_is_separate_and_nonqueueing() {
    let _permit = DOWNLOAD.try_acquire().unwrap();
    let error = SubscriptionReader::new()
        .download("https://example.com/a", None, None, &mut Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "subscription_busy");
}

#[test]
fn urls_are_canonical_https_without_credentials_fragments_or_private_literals() {
    assert_eq!(
        canonical_url("HTTPS://EXAMPLE.COM:443/x/../a").unwrap(),
        "https://example.com/a"
    );
    for bad in [
        "http://example.com/a",
        "file:///tmp/a",
        "https://u:p@example.com/a",
        "https://@example.com/a",
        "https:example.com/a",
        "https:\\@example.com/a",
        "https://example.com/a#frag",
        "https://127.0.0.1/",
        "https://[::ffff:127.0.0.1]/",
        "https://[::ffff:8.8.8.8]/",
        "https://198.18.1.1/",
        "https://example.com:0/",
    ] {
        assert!(canonical_url(bad).is_err(), "{bad}");
    }
    assert!(canonical_url(&format!("https://example.com/{}", "x".repeat(2048))).is_err());
}

#[tokio::test]
async fn validators_bind_exact_final_path_representation_and_verified_content() {
    let previous = binding();
    let mut seen = Vec::new();
    let mut responses = VecDeque::from([
        response(302, &[("location", "/b")], b""),
        response(304, &[], b""),
        response(200, &[("etag", "\"same\"")], b".new.test\n"),
    ]);
    let mut output = Vec::new();
    let result = download_with(
        &previous.final_url,
        Some(&previous),
        Some(&previous.content_sha256),
        &mut output,
        |uri, headers| {
            seen.push((uri.to_string(), headers));
            ready(Ok(responses.pop_front().unwrap()))
        },
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert!(seen[0].1.contains_key(header::IF_NONE_MATCH));
    assert!(!seen[1].1.contains_key(header::IF_NONE_MATCH));
    assert!(!seen[2].1.contains_key(header::IF_NONE_MATCH));
    let DownloadOutcome::Downloaded(result) = result else {
        panic!("not a new object")
    };
    assert_eq!(result.validators.final_url, "https://example.com/b");
    assert_eq!(
        result.validators.content_sha256,
        format!("{:x}", Sha256::digest(&output))
    );
    assert_eq!(output, b".new.test\n");
    let mut changed = previous.clone();
    changed.representation = "other".into();
    assert!(changed.validate().is_err());
}

#[tokio::test]
async fn valid_304_requires_verified_lkg_and_missing_object_repairs_only_once() {
    let previous = binding();
    let mut output = Vec::new();
    let result = download_with(
        &previous.final_url,
        Some(&previous),
        Some(&previous.content_sha256),
        &mut output,
        |_, headers| {
            assert!(headers.contains_key(header::IF_NONE_MATCH));
            ready(Ok(response(304, &[], b"")))
        },
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert!(matches!(result, DownloadOutcome::NotModified));
    let mut requests = 0;
    let result = download_with(
        &previous.final_url,
        Some(&previous),
        None,
        &mut output,
        |_, headers| {
            requests += 1;
            assert!(!headers.contains_key(header::IF_NONE_MATCH));
            ready(Ok(response(304, &[], b"")))
        },
        MAX_BYTES,
    )
    .await
    .unwrap_err();
    assert_eq!(requests, 2);
    assert_eq!(result.code, "invalid_not_modified");
    let mut responses = VecDeque::from([response(304, &[], b""), response(200, &[], b"fixed")]);
    let result = download_with(
        &previous.final_url,
        Some(&previous),
        None,
        &mut output,
        |_, _| ready(Ok(responses.pop_front().unwrap())),
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert!(matches!(result, DownloadOutcome::Downloaded(_)));
}

#[tokio::test]
async fn every_redirect_is_checked_and_auth_is_never_added() {
    for location in [
        "http://example.com/x",
        "https://127.0.0.1/x",
        "https://u:p@example.com/x",
        "https://@example.com/x",
        "//@example.com/x",
        "\\\\@example.com/x",
        "https://example.com/a#f",
    ] {
        let mut calls = 0;
        let result = download_with(
            "https://example.com/a",
            None,
            None,
            &mut Vec::new(),
            |_, headers| {
                calls += 1;
                assert!(!headers.contains_key(header::COOKIE));
                assert!(!headers.contains_key(header::AUTHORIZATION));
                ready(Ok(response(302, &[("location", location)], b"")))
            },
            MAX_BYTES,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
    let mut calls = 0;
    let result = download_with(
        "https://example.com/a",
        None,
        None,
        &mut Vec::new(),
        |_, _| {
            calls += 1;
            ready(Ok(response(302, &[("location", "/a")], b"")))
        },
        MAX_BYTES,
    )
    .await
    .unwrap_err();
    assert_eq!(calls, 4);
    assert_eq!(result.code, "redirect_limit");
}

#[tokio::test]
async fn last_modified_fallback_and_etag_priority_are_bound_to_verified_content() {
    let mut previous = binding();
    previous.last_modified = Some("Thu, 01 Jan 1970 00:10:00 GMT".into());
    for use_etag in [true, false] {
        if !use_etag {
            previous.etag = None;
        }
        let result = download_with(
            &previous.final_url,
            Some(&previous),
            Some(&previous.content_sha256),
            &mut Vec::new(),
            |_, headers| {
                assert_eq!(headers.contains_key(header::IF_NONE_MATCH), use_etag);
                assert_eq!(headers.contains_key(header::IF_MODIFIED_SINCE), !use_etag);
                ready(Ok(response(304, &[], b"")))
            },
            MAX_BYTES,
        )
        .await
        .unwrap();
        assert!(matches!(result, DownloadOutcome::NotModified));
    }
    for status in [429, 503] {
        let error = http_error(
            StatusCode::from_u16(status).unwrap(),
            &response(status, &[("retry-after", "120")], b"").headers,
        );
        assert!(error.retry_after_unix.is_some());
    }
}

async fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn gzip_optional_header_crc_is_supported() {
    let raw = b".example.com\n";
    let mut compressed = gzip(raw).await;
    compressed[3] |= 2;
    let mut crc = !0u32;
    for &byte in &compressed[..10] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    let crc = (!crc).to_le_bytes();
    compressed.splice(10..10, crc[..2].iter().copied());
    let mut output = Vec::new();
    stream_text(
        response(200, &[("content-encoding", "gzip")], &compressed),
        &mut output,
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(output, raw);
    compressed[10] ^= 1;
    assert!(
        stream_text(
            response(200, &[("content-encoding", "gzip")], &compressed),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn streaming_identity_gzip_size_truncation_and_unknown_encoding() {
    let raw = b".example.com\n".repeat(4000);
    let zipped = gzip(&raw).await;
    let mut bad_crc = zipped.clone();
    let footer = bad_crc.len() - 8;
    bad_crc[footer] ^= 1;
    assert!(
        stream_text(
            response(200, &[("content-encoding", "gzip")], &bad_crc),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
    let mut concatenated = zipped.clone();
    concatenated.extend_from_slice(&zipped);
    let mut doubled = Vec::new();
    stream_text(
        response(200, &[("content-encoding", "gzip")], &concatenated),
        &mut doubled,
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(doubled, raw.repeat(2));
    concatenated.truncate(concatenated.len() - 1);
    assert!(
        stream_text(
            response(200, &[("content-encoding", "gzip")], &concatenated),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
    let mut output = Vec::new();
    let (hash, bytes, transferred) = stream_text(
        response(
            200,
            &[
                ("content-encoding", "gzip"),
                ("content-length", &zipped.len().to_string()),
            ],
            &zipped,
        ),
        &mut output,
        MAX_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(output, raw);
    assert_eq!(bytes, raw.len() as u64);
    assert_eq!(transferred, zipped.len() as u64);
    assert_eq!(hash, format!("{:x}", Sha256::digest(&raw)));
    assert_eq!(
        stream_text(
            response(200, &[("content-encoding", "gzip")], &zipped),
            &mut Vec::new(),
            1024
        )
        .await
        .unwrap_err()
        .code,
        "decoded_too_large"
    );
    assert!(
        stream_text(
            response(
                200,
                &[("content-encoding", "gzip")],
                &zipped[..zipped.len() - 3]
            ),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
    let mut trailing = zipped.clone();
    trailing.extend_from_slice(b"garbage");
    assert!(
        stream_text(
            response(200, &[("content-encoding", "gzip")], &trailing),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
    for headers in [
        vec![("content-encoding", "br")],
        vec![("content-encoding", "gzip, identity")],
        vec![
            ("content-encoding", "identity"),
            ("content-encoding", "gzip"),
        ],
    ] {
        assert_eq!(
            stream_text(response(200, &headers, b"x"), &mut Vec::new(), MAX_BYTES)
                .await
                .unwrap_err()
                .code,
            "unsupported_encoding"
        );
    }
    assert_eq!(
        stream_text(response(200, &[], b"123456789"), &mut Vec::new(), 8)
            .await
            .unwrap_err()
            .code,
        "transfer_too_large"
    );
    assert!(
        stream_text(
            response(200, &[("content-length", "5")], b"123"),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
    assert!(
        stream_text(
            response(200, &[("content-length", "bad")], b"123"),
            &mut Vec::new(),
            MAX_BYTES
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn header_and_validator_limits_and_transport_errors_are_bounded() {
    for bad in ["bare", "\"a\"b\"", "W/bare", "bad\r\nvalue"] {
        assert!(!valid_etag(bad));
    }
    for good in ["\"\"", "\"abc\"", "W/\"abc\""] {
        assert!(valid_etag(good));
    }
    assert!(!valid_modified("not a date"));
    assert!(valid_modified("Thu, 01 Jan 1970 00:10:00 GMT"));
    let etag = "x".repeat(1025);
    let mut output = Vec::new();
    let result = download_with(
        "https://example.com/a",
        None,
        None,
        &mut output,
        |_, _| ready(Ok(response(200, &[("etag", &etag)], b"x"))),
        MAX_BYTES,
    )
    .await
    .unwrap();
    let DownloadOutcome::Downloaded(result) = result else {
        panic!()
    };
    assert!(result.validators.etag.is_none());
    let huge = "x".repeat(32768);
    assert_eq!(
        download_with(
            "https://example.com/a",
            None,
            None,
            &mut Vec::new(),
            |_, _| ready(Ok(response(200, &[("x-large", &huge)], b"x"))),
            MAX_BYTES
        )
        .await
        .unwrap_err()
        .code,
        "response_headers_too_large"
    );
    for code in ["tls_error", "destination_not_allowed", "connect_timeout"] {
        assert_eq!(
            download_with(
                "https://example.com/a",
                None,
                None,
                &mut Vec::new(),
                |_, _| ready(Err(DownloadError::new(code))),
                MAX_BYTES
            )
            .await
            .unwrap_err()
            .code,
            code
        );
    }
    let mut slow = response(200, &[], b"");
    slow.body = Box::pin(futures_util::stream::pending());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            stream_text(slow, &mut Vec::new(), MAX_BYTES)
        )
        .await
        .is_err()
    );
}
