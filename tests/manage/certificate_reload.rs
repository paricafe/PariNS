use super::*;

type Identity = rcgen::CertifiedKey<rcgen::KeyPair>;

async fn api(
    server: &Management,
    identity: &Identity,
    auth: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Response {
    timeout(
        DEADLINE,
        secure_request(
            server.address,
            identity,
            "dns.test",
            &https_wire(server.address, "dns.test", method, path, Some(auth), body),
        ),
    )
    .await
    .unwrap()
}

fn write_identity(cert: &Path, key: &Path, identity: &Identity) {
    std::fs::write(cert, identity.cert.pem()).unwrap();
    std::fs::write(key, identity.signing_key.serialize_pem()).unwrap();
}

async fn assert_tcp_identity(address: SocketAddr, identity: &Identity, alpn: &[u8]) {
    // Each helper call makes a fresh ClientConfig: no resumption state is shared.
    let stream = timeout(DEADLINE, trusted_tls(address, identity, "dns.test", alpn))
        .await
        .unwrap();
    assert_eq!(
        stream.get_ref().1.peer_certificates().unwrap()[0].as_ref(),
        identity.cert.der().as_ref()
    );
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(alpn));
}

async fn assert_quic_identity(address: SocketAddr, identity: &Identity, alpn: &[u8]) {
    let mut roots = RootCertStore::empty();
    roots.add(identity.cert.der().clone()).unwrap();
    let mut tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let connection = timeout(DEADLINE, endpoint.connect(address, "dns.test").unwrap())
        .await
        .unwrap()
        .unwrap();
    let peer = connection
        .peer_identity()
        .unwrap()
        .downcast::<Vec<CertificateDer<'static>>>()
        .unwrap();
    assert_eq!(peer[0].as_ref(), identity.cert.der().as_ref());
    let handshake = connection
        .handshake_data()
        .unwrap()
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .unwrap();
    assert_eq!(handshake.protocol.as_deref(), Some(alpn));
    connection.close(0u32.into(), b"test complete");
    timeout(DEADLINE, endpoint.wait_idle()).await.unwrap();
}

async fn assert_all_identities(
    web: SocketAddr,
    doh: SocketAddr,
    dot: SocketAddr,
    doq: SocketAddr,
    identity: &Identity,
) {
    assert_tcp_identity(web, identity, b"http/1.1").await;
    assert_tcp_identity(doh, identity, b"h2").await;
    assert_tcp_identity(dot, identity, b"dot").await;
    assert_quic_identity(doh, identity, b"h3").await;
    assert_quic_identity(doq, identity, b"doq").await;
}

async fn assert_existing_dot(stream: &mut tokio_rustls::client::TlsStream<TcpStream>, id: u16) {
    let mut question = query("example.test.");
    question.metadata.id = id;
    let wire = question.to_vec().unwrap();
    timeout(DEADLINE, async {
        stream.write_u16(wire.len() as u16).await.unwrap();
        stream.write_all(&wire).await.unwrap();
        stream.flush().await.unwrap();
        let length = stream.read_u16().await.unwrap();
        let mut wire = vec![0; usize::from(length)];
        stream.read_exact(&mut wire).await.unwrap();
        let response = protocol::decode(&wire).unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.response_code, ResponseCode::NoError);
        assert_eq!(response.queries, question.queries);
    })
    .await
    .unwrap();
}

fn assert_stable_state(before: &Value, after: &Value) {
    for path in [
        "/revision",
        "/generation",
        "/dns_health/generation",
        "/cache/epoch",
        "/diagnostics/cache/since_ms",
        "/storage/log_epoch",
        "/storage/totals_epoch",
        "/storage/history_epoch",
    ] {
        let initial = before.pointer(path).expect(path);
        assert!(!initial.is_null(), "missing initial {path}");
        assert_eq!(after.pointer(path), Some(initial), "changed {path}");
    }
    assert_eq!(after["running"], true);
}

#[tokio::test]
async fn reload_is_atomic_across_transports_and_preserves_sessions_runtime_and_old_dot() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let first = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let pairs: Vec<_> = ["doh", "dot", "doq"]
        .into_iter()
        .map(|role| {
            (
                temporary.path().join(format!("{role}.pem")),
                temporary.path().join(format!("{role}-key.pem")),
            )
        })
        .collect();
    for (cert, key) in &pairs {
        write_identity(cert, key, &first);
    }
    let doh_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let doh_address = doh_tcp.local_addr().unwrap();
    let doh_udp = UdpSocket::bind(doh_address).await.unwrap();
    let dot = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dot_address = dot.local_addr().unwrap();
    let doq = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let doq_address = doq.local_addr().unwrap();
    let mut candidate =
        configuration().replace("tcp_io_timeout_ms = 500", "tcp_io_timeout_ms = 30000");
    candidate.push_str("\n[web]\npublic_host='dns.test'\n");
    for ((role, address), (cert, key)) in [
        ("doh", doh_address),
        ("dot", dot_address),
        ("doq", doq_address),
    ]
    .into_iter()
    .zip(&pairs)
    {
        candidate.push_str(&format!(
            "\n[{role}]\nlisten='{address}'\ncert_file={}\nkey_file={}\n",
            json!(cert),
            json!(key)
        ));
        if role == "doh" {
            candidate.push_str("http3=true\n");
        }
    }
    drop((doh_tcp, doh_udp, dot, doq));
    let setup = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    server.setup_with(&setup, &candidate).await.expect(200);
    let login = secure_request(
        server.address,
        &first,
        "dns.test",
        &https_wire(
            server.address,
            "dns.test",
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin", "password":PASSWORD})),
        ),
    )
    .await;
    login.expect(200);
    let auth = login.auth();
    let initial = api(&server, &first, &auth, "GET", "/api/status", None)
        .await
        .expect(200);
    let generation = initial["certificates"]["certificate_generation"]
        .as_u64()
        .unwrap();
    let revision = initial["revision"].as_u64().unwrap();
    let mut existing_dot = trusted_tls(dot_address, &first, "dns.test", b"dot").await;
    assert_existing_dot(&mut existing_dot, 1).await;

    api(
        &server,
        &first,
        &auth,
        "POST",
        "/api/certificates/reload",
        Some(json!({"revision":revision+1})),
    )
    .await
    .expect(409);
    let unchanged = api(
        &server,
        &first,
        &auth,
        "POST",
        "/api/certificates/reload",
        Some(json!({"revision":revision})),
    )
    .await
    .expect(200);
    assert_eq!(unchanged["outcome"], "unchanged");
    assert_eq!(unchanged["certificate_generation"], generation);

    // One complete candidate and a later role's mismatched key must publish none.
    write_identity(&pairs[0].0, &pairs[0].1, &second);
    std::fs::write(&pairs[1].0, second.cert.pem()).unwrap();
    api(
        &server,
        &first,
        &auth,
        "POST",
        "/api/certificates/reload",
        Some(json!({"revision":revision})),
    )
    .await
    .expect(422);
    assert_all_identities(
        server.address,
        doh_address,
        dot_address,
        doq_address,
        &first,
    )
    .await;
    let failed = api(&server, &first, &auth, "GET", "/api/status", None)
        .await
        .expect(200);
    assert_stable_state(&initial, &failed);
    assert_eq!(failed["certificates"]["certificate_generation"], generation);
    assert_eq!(failed["certificates"]["last_reload"]["outcome"], "failed");
    assert_existing_dot(&mut existing_dot, 2).await;

    for (cert, key) in &pairs {
        write_identity(cert, key, &second);
    }
    let applied = api(
        &server,
        &first,
        &auth,
        "POST",
        "/api/certificates/reload",
        Some(json!({"revision":revision})),
    )
    .await
    .expect(200);
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(applied["certificate_generation"], generation + 1);
    assert_all_identities(
        server.address,
        doh_address,
        dot_address,
        doq_address,
        &second,
    )
    .await;
    assert_existing_dot(&mut existing_dot, 3).await;
    let after = api(&server, &second, &auth, "GET", "/api/status", None)
        .await
        .expect(200);
    assert_stable_state(&initial, &after);
    assert_eq!(after["certificates"]["last_reload"]["source"], "api");
    let unchanged = api(
        &server,
        &second,
        &auth,
        "POST",
        "/api/certificates/reload",
        Some(json!({"revision":revision})),
    )
    .await
    .expect(200);
    assert_eq!(unchanged["outcome"], "unchanged");
    assert_eq!(unchanged["certificate_generation"], generation + 1);
    drop(existing_dot);
    server.finish().await;
}

#[cfg(unix)]
struct ManagedChild(std::process::Child);

#[cfg(unix)]
impl ManagedChild {
    fn signal(&mut self, signal: &str, repeats: usize) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "managed child already exited"
        );
        let pid = self.0.id();
        assert_ne!(pid, std::process::id(), "never signal the test runner");
        assert!(
            std::process::Command::new("/bin/kill")
                .arg(signal)
                .args(std::iter::repeat_n(pid.to_string(), repeats))
                .status()
                .unwrap()
                .success()
        );
    }
}

#[cfg(unix)]
impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Failure cleanup affects only the exact child owned by this fixture.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn managed_child_sighup_rotates_web_certificate_without_restarting_dns_and_term_drains() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let setup_server = Management::start(&directory).await;
    let address = setup_server.address;
    // DNS binds TCP first and then UDP on the same port. Preselect a port
    // available to both protocols; release it before the public setup API binds.
    let (dns_tcp, dns_udp) = {
        let mut pair = None;
        for _ in 0..16 {
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            match UdpSocket::bind(tcp.local_addr().unwrap()).await {
                Ok(udp) => {
                    pair = Some((tcp, udp));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(error) => panic!("DNS UDP reservation failed: {error}"),
            }
        }
        pair.expect("reserve a DNS port available to TCP and UDP")
    };
    let dns_address = dns_tcp.local_addr().unwrap();
    let first = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert = temporary.path().join("signal-cert.pem");
    let key = temporary.path().join("signal-key.pem");
    write_identity(&cert, &key, &first);
    let candidate = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        configuration().replacen("127.0.0.1:0", &dns_address.to_string(), 1),
        json!(cert),
        json!(key)
    );
    let setup = std::fs::read_to_string(directory.join("setup-token")).unwrap();
    drop((dns_tcp, dns_udp));
    setup_server
        .setup_with(&setup, &candidate)
        .await
        .expect(200);
    setup_server.finish().await;

    let mut child = ManagedChild(
        std::process::Command::new(env!("CARGO_BIN_EXE_parins"))
            .arg("--manage")
            .arg("--state-dir")
            .arg(&directory)
            .arg("--web-listen")
            .arg(address.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    timeout(DEADLINE, async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "managed startup failed"
            );
            if TcpStream::connect(address).await.is_ok() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Reuse the API fixture's request helpers without owning this child's stop.
    let server = Management {
        address,
        stop: None,
        task: None,
    };
    let login = timeout(
        DEADLINE,
        secure_request(
            address,
            &first,
            "dns.test",
            &https_wire(
                address,
                "dns.test",
                "POST",
                "/api/login",
                None,
                Some(json!({"username":"admin", "password":PASSWORD})),
            ),
        ),
    )
    .await
    .unwrap();
    login.expect(200);
    let auth = login.auth();
    let before = api(&server, &first, &auth, "GET", "/api/status", None)
        .await
        .expect(200);
    let generation = before["certificates"]["certificate_generation"]
        .as_u64()
        .unwrap();

    let next_cert = cert.with_extension("next");
    let next_key = key.with_extension("next");
    write_identity(&next_cert, &next_key, &second);
    std::fs::rename(next_cert, &cert).unwrap();
    std::fs::rename(next_key, &key).unwrap();
    child.signal("-HUP", 1);
    let status_wire = https_wire(address, "dns.test", "GET", "/api/status", Some(&auth), None);
    let after = timeout(DEADLINE, async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "managed child exited on HUP"
            );
            // Trust only these two explicit fixture certificates while polling
            // publication; each iteration has a new client and full handshake.
            let mut roots = RootCertStore::empty();
            roots.add(first.cert.der().clone()).unwrap();
            roots.add(second.cert.der().clone()).unwrap();
            let mut stream = connector(roots, b"http/1.1")
                .connect(
                    ServerName::try_from("dns.test").unwrap(),
                    TcpStream::connect(address).await.unwrap(),
                )
                .await
                .unwrap();
            let new_identity = stream.get_ref().1.peer_certificates().unwrap()[0].as_ref()
                == second.cert.der().as_ref();
            stream.write_all(status_wire.as_bytes()).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            let status = parse_response(&response).expect(200);
            if new_identity
                && status["certificates"]["last_reload"]["source"] == "signal"
                && status["certificates"]["last_reload"]["outcome"] == "applied"
            {
                break status;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        after["certificates"]["certificate_generation"],
        generation + 1
    );
    assert_stable_state(&before, &after);
    // Only the original in-process login token is used after reload.
    api(&server, &second, &auth, "GET", "/api/status", None)
        .await
        .expect(200);
    // One kill invocation emits the burst without per-signal process startup.
    // Unix may coalesce HUP delivery; no exact reload count is assumed.
    child.signal("-HUP", 16);
    child.signal("-TERM", 1);
    let exited = timeout(DEADLINE, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("SIGTERM must drain pending HUP work within test deadline");
    assert!(
        exited.success(),
        "managed child exited unsuccessfully: {exited}"
    );
}
