use super::*;
use std::{collections::VecDeque, future::ready};

fn response(status: u16, headers: &[(&str, &str)], chunks: &[&[u8]]) -> WireResponse {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        map.insert(
            http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    WireResponse {
        status: StatusCode::from_u16(status).unwrap(),
        headers: map,
        body: Box::pin(futures_util::stream::iter(
            chunks
                .iter()
                .map(|bytes| Ok(Bytes::copy_from_slice(bytes)))
                .collect::<Vec<_>>(),
        )),
        _connection: None,
    }
}

#[test]
fn destinations_are_exact_https_and_global_only() {
    for bad in [
        "http://github.com/x",
        "https://github.com.evil.test/x",
        "https://user@github.com/x",
        "https://github.com:444/x",
        "https://127.0.0.1/x",
        "https://github.com./x",
        "https://github.com/x#fragment",
    ] {
        assert!(fixed_uri(bad).is_err(), "{bad}");
    }
    for good in [
        "https://api.github.com/repos/paricafe/PariNS/releases/latest",
        "https://github.com:443/x",
        "https://release-assets.githubusercontent.com/path?sig=secret",
    ] {
        assert!(fixed_uri(good).is_ok());
    }
    for bad in [
        "0.0.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "100.64.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.1.1",
        "192.0.2.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::ffff:127.0.0.1",
        "::ffff:8.8.8.8",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
        "2002:7f00:1::1",
        "64:ff9b::a00:1",
        "3fff::1",
    ] {
        assert!(!global_address(bad.parse().unwrap()), "{bad}");
    }
    for good in [
        "140.82.113.5",
        "185.199.108.133",
        "2606:50c0:8000::154",
        "2001:4860:4860::8888",
    ] {
        assert!(global_address(good.parse().unwrap()), "{good}");
    }
    assert!(asset_uri("v1.2.3", "../../passwd").is_err());
    assert!(asset_uri("v1.2.3/evil", "parins-update.json").is_err());
}

#[tokio::test]
async fn redirects_bounded_each_hop_validated_without_leaking_signed_location() {
    let start = fixed_uri("https://github.com/initial").unwrap();
    let mut seen = Vec::new();
    let result = fetch_with(start.clone(), None, |uri, _| {
        seen.push(uri.to_string());
        ready(Ok(response(
            302,
            &[(
                "location",
                "https://release-assets.githubusercontent.com/raw?sig=secret",
            )],
            &[],
        )))
    })
    .await;
    assert_eq!(seen.len(), 4);
    assert_eq!(result.err().unwrap().to_string(), "redirect_limit");
    let mut seen = 0;
    let result = fetch_with(start.clone(), None, |_, _| {
        seen += 1;
        ready(Ok(response(
            302,
            &[("location", "http://github.com/raw?sig=secret")],
            &[],
        )))
    })
    .await;
    assert_eq!(seen, 1);
    assert_eq!(result.err().unwrap().to_string(), "destination_not_allowed");
    let mut responses = VecDeque::from([
        response(302, &[("location", "/relative")], &[]),
        response(200, &[], &[b"ok"]),
    ]);
    let result = fetch_with(start, None, |_, _| {
        ready(Ok(responses.pop_front().unwrap()))
    })
    .await
    .unwrap();
    assert_eq!(collect(result, 2).await.unwrap(), b"ok");
}

#[tokio::test]
async fn conditional_etag_and_http_errors_are_distinct_from_latest() {
    let uri = fixed_uri("https://api.github.com/repos/paricafe/PariNS/releases/latest").unwrap();
    let result = fetch_with(uri.clone(), Some("\"etag\""), |_, etag| {
        assert_eq!(etag.as_deref(), Some("\"etag\""));
        ready(Ok(response(304, &[], &[])))
    })
    .await
    .unwrap();
    assert_eq!(result.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        fetch_with(uri.clone(), None, |_, _| ready(Ok(response(304, &[], &[]))))
            .await
            .err()
            .unwrap()
            .code,
        "http_error"
    );
    assert_eq!(
        fetch_with(uri.clone(), Some("bad\r\nCookie: secret"), |_, _| {
            ready(Err(ReaderError::new("must_not_request")))
        })
        .await
        .err()
        .unwrap()
        .code,
        "invalid_etag"
    );
    for (status, code) in [
        (403, "rate_limited"),
        (429, "rate_limited"),
        (500, "http_error"),
        (404, "release_incomplete"),
    ] {
        assert_eq!(
            fetch_with(uri.clone(), None, |_, _| ready(Ok(response(
                status,
                &[],
                &[]
            ))))
            .await
            .err()
            .unwrap()
            .code,
            code
        );
    }
    let headers = response(
        429,
        &[("retry-after", "120"), ("x-ratelimit-reset", "500")],
        &[],
    )
    .headers;
    assert_eq!(
        http_error(StatusCode::TOO_MANY_REQUESTS, &headers, 100).retry_after_unix,
        Some(500)
    );
    let headers = response(
        503,
        &[("retry-after", "Thu, 01 Jan 1970 00:10:00 GMT")],
        &[],
    )
    .headers;
    assert_eq!(
        http_error(StatusCode::SERVICE_UNAVAILABLE, &headers, 100).retry_after_unix,
        Some(600)
    );
}

#[tokio::test]
async fn response_limits_count_actual_chunks_and_reject_encoding() {
    assert_eq!(
        collect(response(200, &[("content-length", "9")], &[]), 8)
            .await
            .unwrap_err()
            .code,
        "response_too_large"
    );
    assert_eq!(
        collect(response(200, &[], &[b"1234", b"56789"]), 8)
            .await
            .unwrap_err()
            .code,
        "response_too_large"
    );
    assert_eq!(
        collect(response(200, &[("content-length", "8")], &[b"short"]), 8)
            .await
            .unwrap_err()
            .code,
        "invalid_response"
    );
    let uri = fixed_uri("https://github.com/x").unwrap();
    assert_eq!(
        fetch_with(uri, None, |_, _| ready(Ok(response(
            200,
            &[("content-encoding", "gzip")],
            &[b"compressed"]
        ))))
        .await
        .err()
        .unwrap()
        .code,
        "unsupported_encoding"
    );
    let mut slow = response(200, &[], &[]);
    slow.body = Box::pin(futures_util::stream::pending());
    assert!(
        timeout(Duration::from_millis(10), collect(slow, 8))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn streamed_download_checks_exact_size_digest_and_progress() {
    let directory = tempfile::tempdir().unwrap();
    let mut file = tokio::fs::File::create(directory.path().join("candidate"))
        .await
        .unwrap();
    let bytes = b"test payload";
    let tuple = DownloadTuple {
        release_id: 1,
        tag: "v1.2.3".into(),
        manifest_sha256: "a".repeat(64),
        asset_id: 2,
        asset_name: "parins-v1.2.3-linux-x86_64.bin".into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    };
    let mut progress = Vec::new();
    stream_binary(
        response(200, &[], &[&bytes[..4], &bytes[4..]]),
        &mut file,
        &tuple,
        &mut |n| progress.push(n),
    )
    .await
    .unwrap();
    assert_eq!(progress, vec![4, 12]);
    assert_eq!(
        tokio::fs::read(directory.path().join("candidate"))
            .await
            .unwrap(),
        bytes
    );
    assert_eq!(
        stream_binary(
            response(200, &[], &[b"too short"]),
            &mut file,
            &tuple,
            &mut |_| {}
        )
        .await
        .unwrap_err()
        .code,
        "verification_failed"
    );
    assert_eq!(
        stream_binary(
            response(200, &[], &[b"test payloae"]),
            &mut file,
            &tuple,
            &mut |_| {}
        )
        .await
        .unwrap_err()
        .code,
        "verification_failed"
    );
    assert_eq!(
        stream_binary(
            response(200, &[], &[b"test payload excess"]),
            &mut file,
            &tuple,
            &mut |_| {}
        )
        .await
        .unwrap_err()
        .code,
        "response_too_large"
    );
}

#[test]
fn release_notes_are_bounded_and_invalid_tags_rejected() {
    let mut value = serde_json::json!({"id":1,"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[],"body":"汉".repeat(30000)});
    let release = parse_release(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(release.body.unwrap().len() <= 64 * 1024);
    value["tag_name"] = serde_json::json!("v1.2.3-rc1");
    assert_eq!(
        parse_release(&serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .code,
        "invalid_version"
    );
    value["tag_name"] = serde_json::json!("v1.2.3");
    value["prerelease"] = serde_json::json!(true);
    assert_eq!(
        parse_release(&serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .code,
        "invalid_release"
    );
}

#[test]
fn confirmation_binds_manifest_bytes_and_release_asset_identity() {
    let hash = "a".repeat(64);
    let manifest: Manifest = serde_json::from_value(serde_json::json!({
        "schema":1,"repository":"paricafe/PariNS","version":"1.2.3","tag":"v1.2.3",
        "source_commit":"a".repeat(40),"update_protocol":1,"install_contract":"linux-managed-updater-v1",
        "min_helper_protocol":1,"durable_contract_epoch":1,"runtime_database_format":2,
        "cache_snapshot_format":2,"cache_semantics":2,"upgrade_mode":"in_place",
        "artifacts":[
            {"target":"x86_64-unknown-linux-musl","name":"parins-v1.2.3-linux-x86_64.bin","size":128,"sha256":hash},
            {"target":"aarch64-unknown-linux-musl","name":"parins-v1.2.3-linux-aarch64.bin","size":128,"sha256":hash}
        ]
    })).unwrap();
    let document = ManifestDocument {
        manifest,
        sha256: "b".repeat(64),
        size: 1024,
    };
    let release: GithubRelease = serde_json::from_value(serde_json::json!({
        "id":123,"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[
            {"id":1,"name":"parins-update.json","state":"uploaded","size":1024,"digest":format!("sha256:{}",document.sha256)},
            {"id":2,"name":"parins-v1.2.3-linux-x86_64.bin","state":"uploaded","size":128,"digest":format!("sha256:{hash}")},
            {"id":3,"name":"parins-v1.2.3-linux-aarch64.bin","state":"uploaded","size":128,"digest":format!("sha256:{hash}")}
        ]
    })).unwrap();
    document.validate_release(&release).unwrap();
    let tuple = DownloadTuple {
        release_id: 123,
        tag: "v1.2.3".into(),
        manifest_sha256: document.sha256.clone(),
        asset_id: 2,
        asset_name: "parins-v1.2.3-linux-x86_64.bin".into(),
        size: 128,
        sha256: hash,
    };
    verify_tuple(&tuple, &release, &document).unwrap();
    let mut changed = tuple.clone();
    changed.asset_id = 4;
    assert_eq!(
        verify_tuple(&changed, &release, &document)
            .unwrap_err()
            .code,
        "release_changed"
    );
    changed = tuple.clone();
    changed.manifest_sha256 = "c".repeat(64);
    assert_eq!(
        verify_tuple(&changed, &release, &document)
            .unwrap_err()
            .code,
        "release_changed"
    );
    let mut changed = document.clone();
    changed.size += 1;
    assert_eq!(
        changed.validate_release(&release).unwrap_err().code,
        "release_changed"
    );
    let mut changed = document;
    changed.sha256 = "c".repeat(64);
    assert_eq!(
        changed.validate_release(&release).unwrap_err().code,
        "release_changed"
    );
}
