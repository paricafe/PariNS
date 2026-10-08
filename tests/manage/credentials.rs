use super::*;

const NEXT_PASSWORD: &str = "rotated-integration-password";

async fn request(
    server: &Management,
    identity: Option<&rcgen::CertifiedKey<rcgen::KeyPair>>,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> Response {
    if let Some(identity) = identity {
        secure_request(
            server.address,
            identity,
            "dns.test",
            &https_wire(server.address, "dns.test", method, path, auth, body),
        )
        .await
    } else {
        server.request(method, path, auth, body).await
    }
}

#[tokio::test]
async fn credential_rotation_http_and_https_reauthenticate_without_changing_runtime_or_sqlite() {
    for https in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("state");
        let server = Management::start(&directory).await;
        let identity = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let cert = temp.path().join("cert.pem");
        let key = temp.path().join("key.pem");
        std::fs::write(&cert, identity.cert.pem()).unwrap();
        std::fs::write(&key, identity.signing_key.serialize_pem()).unwrap();
        let mut source = format!(
            "{}\n[query_log]\nenabled=true\nmax_entries=10\nretention_secs=60\n[storage]\nflush_interval_ms=100\n[updates]\nauto_check=false\n",
            configuration()
        );
        if https {
            source.push_str(&format!(
                "\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
                json!(cert), json!(key)
            ));
        }
        server
            .request("GET", "/api/account", None, None)
            .await
            .expect(401);
        let setup_token = std::fs::read_to_string(directory.join("setup-token")).unwrap();
        let setup = server.setup_with(&setup_token, &source).await;
        setup.expect(200);
        let identity = https.then_some(&identity);
        let login = request(
            &server,
            identity,
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await;
        login.expect(200);
        let auth = login.auth();
        assert_eq!(
            request(&server, identity, "GET", "/api/account", Some(&auth), None)
                .await
                .expect(200),
            json!({"username":"admin"})
        );
        let cookie_only = auth.split_once('|').unwrap().0.to_owned() + "|wrong-binding";
        request(
            &server,
            identity,
            "GET",
            "/api/account",
            Some(&cookie_only),
            None,
        )
        .await
        .expect(409);
        let next = format!("{source}\n[cache]\nmax_ttl_secs=120\n");
        request(
            &server,
            identity,
            "PUT",
            "/api/config",
            Some(&auth),
            Some(json!({"revision":1,"toml":next})),
        )
        .await
        .expect(200);
        let status = request(&server, identity, "GET", "/api/status", Some(&auth), None)
            .await
            .expect(200);
        assert_dns(status["listen"].as_str().unwrap().parse().unwrap()).await;
        let before_logs = timeout(DEADLINE, async {
            loop {
                let result = request(
                    &server,
                    identity,
                    "POST",
                    "/api/query-log/list",
                    Some(&auth),
                    Some(json!({})),
                )
                .await
                .expect(200);
                if result["page"]["entries"]
                    .as_array()
                    .is_some_and(|entries| !entries.is_empty())
                {
                    break result;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let before: Value =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        let response = request(&server, identity, "POST", "/api/account/credentials", Some(&auth), Some(json!({
            "current_password":PASSWORD, "username":"rotated-admin", "new_password":NEXT_PASSWORD
        }))).await;
        assert_eq!(
            response.expect(200),
            json!({"reauthentication_required":true})
        );
        assert!(!response.headers.contains("set-cookie:"));
        for old in std::iter::once(auth.clone()).chain((!https).then(|| setup.auth())) {
            request(&server, identity, "GET", "/api/account", Some(&old), None)
                .await
                .expect(401);
            request(
                &server,
                identity,
                "POST",
                "/api/config/rollback",
                Some(&old),
                Some(json!({"revision":2})),
            )
            .await
            .expect(401);
        }
        request(
            &server,
            identity,
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await
        .expect(401);
        let login = request(
            &server,
            identity,
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"rotated-admin","password":NEXT_PASSWORD})),
        )
        .await;
        login.expect(200);
        assert!(login.headers.contains("httponly"));
        assert!(login.headers.contains("samesite=strict"));
        assert_eq!(login.headers.contains("; secure"), https);
        assert_eq!(login.headers.contains("__host-parins_session="), https);
        let auth = login.auth();
        assert_eq!(
            request(&server, identity, "GET", "/api/account", Some(&auth), None)
                .await
                .expect(200),
            json!({"username":"rotated-admin"})
        );
        let after_status = request(&server, identity, "GET", "/api/status", Some(&auth), None)
            .await
            .expect(200);
        for field in ["generation", "listen", "certificates", "transport"] {
            assert!(status.get(field).is_some(), "unknown status field {field}");
            assert_eq!(after_status[field], status[field]);
        }
        let after_logs = request(
            &server,
            identity,
            "POST",
            "/api/query-log/list",
            Some(&auth),
            Some(json!({})),
        )
        .await
        .expect(200);
        assert_eq!(after_logs["revision"], before_logs["revision"]);
        for field in ["entries", "total", "log_epoch"] {
            assert_eq!(after_logs["page"][field], before_logs["page"][field]);
        }
        let after: Value =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        for field in ["toml", "previous", "revision"] {
            assert_eq!(after[field], before[field]);
        }
        assert_eq!(after["username"], "rotated-admin");
        assert_ne!(after["password_hash"], before["password_hash"]);
        request(
            &server,
            identity,
            "POST",
            "/api/config/rollback",
            Some(&auth),
            Some(json!({"revision":2})),
        )
        .await
        .expect(200);
        let rolled_back: Value =
            serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
        assert_eq!(rolled_back["username"], after["username"]);
        assert_eq!(rolled_back["password_hash"], after["password_hash"]);
        assert_eq!(rolled_back["toml"], before["previous"]);
        server.finish().await;
        let database = rusqlite::Connection::open_with_flags(
            directory.join("runtime/observability.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let entries: u64 = database
            .query_row(
                "SELECT count(*) FROM query_log WHERE name = 'example.test.'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entries, 1,
            "rotation and configuration rollback preserve durable log rows"
        );
    }
}
