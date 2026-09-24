use super::*;
use crate::storage::HistoryOptions;

fn config() -> String {
    "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=200\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into()
}

async fn assert_sampler(services: &RuntimeServices, running: bool, generation: u64) {
    // Isolate the current-state tail from intentional aggregate transition gaps.
    services
        .clear_history(services.status().history_epoch)
        .await
        .unwrap();
    services.flush().await.unwrap();
    let stats = services
        .statistics(HistoryOptions::default())
        .await
        .unwrap();
    let last = stats.samples.last().expect("forced tail sample");
    assert_eq!(last.running, running);
    assert_eq!(last.generation, generation);
}

async fn assert_web_identity(
    config: Arc<rustls::ServerConfig>,
    identity: &rcgen::CertifiedKey<rcgen::KeyPair>,
) {
    use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
    use tokio::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(config)
            .accept(socket)
            .await
            .unwrap()
    });
    let mut roots = RootCertStore::empty();
    roots.add(identity.cert.der().clone()).unwrap();
    let mut client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    client.alpn_protocols = vec![b"http/1.1".to_vec()];
    let stream = tokio_rustls::TlsConnector::from(Arc::new(client))
        .connect(
            ServerName::try_from("dns.test").unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        stream.get_ref().1.peer_certificates().unwrap()[0].as_ref(),
        identity.cert.der().as_ref()
    );
    assert_eq!(
        stream.get_ref().1.alpn_protocol(),
        Some(b"http/1.1".as_slice())
    );
    server.await.unwrap();
}

#[tokio::test]
async fn certificate_reload_retains_cache_owner_and_updates_web_identity_while_dns_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let first = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let third = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
    let cert = temp.path().join("cert.pem");
    let key = temp.path().join("key.pem");
    let write = |identity: &rcgen::CertifiedKey<rcgen::KeyPair>| {
        std::fs::write(&cert, identity.cert.pem()).unwrap();
        std::fs::write(&key, identity.signing_key.serialize_pem()).unwrap();
    };
    write(&first);
    let toml = format!(
        "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
        config(),
        serde_json::json!(cert),
        serde_json::json!(key)
    );
    let store = Store::open(&temp.path().join("state")).unwrap();
    store
        .save(&Stored {
            username: "admin".into(),
            password_hash: super::super::store::hash_password("local-unit-test-password").unwrap(),
            toml,
            previous: None,
            revision: 1,
        })
        .unwrap();
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: Vec::new(),
    }));
    let mut manager = Manager::open(store, "127.0.0.1:3000".parse().unwrap(), active.clone())
        .await
        .unwrap();
    let resolver = manager.resolver().unwrap().clone();
    let cache = resolver.cache();
    let generation = manager.generation;
    let web = active.lock().unwrap().snapshot.tls.clone().unwrap();
    write(&second);
    assert_eq!(
        manager.reload_certificates("api").await.unwrap()["outcome"],
        "applied"
    );
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&cache, &manager.resolver().unwrap().cache()));
    assert_eq!(manager.generation, generation);
    assert_web_identity(web.clone(), &second).await;

    manager.stop().await;
    assert!(manager.resolver().is_none());
    assert_eq!(
        manager.services.health.snapshot().state,
        HealthState::Stopped
    );
    write(&third);
    assert_eq!(
        manager.reload_certificates("api").await.unwrap()["outcome"],
        "applied"
    );
    assert!(manager.resolver().is_none());
    assert_eq!(manager.generation, generation);
    assert_eq!(manager.saved.as_ref().unwrap().revision, 1);
    assert_web_identity(web, &third).await;
    assert_sampler(&manager.services, false, generation).await;
    manager.terminal_shutdown().await;
}

#[tokio::test]
async fn aborted_dns_task_publishes_failed_sampler_state_and_never_clean_shutdown() {
    let config = Config::parse(&config()).unwrap();
    let services = RuntimeServices::ephemeral(RuntimeSettings::from_config(&config));
    let server = Server::bind_with_services(config.clone(), None, services.clone())
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let running = Running::start(server, config, address, services.clone(), 1);
    assert!(services.health.snapshot().ready);
    running.task.abort();
    let (resolver, _, clean) = running.stop().await;
    assert!(!clean);
    let health = services.health.snapshot();
    assert_eq!(health.state, HealthState::Failed);
    assert_eq!(health.code, Some("task_aborted"));
    assert!(!health.ready);
    assert!(!resolver.is_quiescent());
    assert_sampler(&services, false, 1).await;
    services.set_dns_state(false, 1);
    assert_eq!(services.health.snapshot().code, Some("task_aborted"));

    // Model a newer live generation, then drop another old completion owner.
    services.set_dns_health(HealthState::Running, 2, None);
    drop(RuntimeCompletion {
        services: services.clone(),
        resolver: resolver.clone(),
        generation: 1,
        complete: false,
    });
    assert_eq!(services.health.snapshot().state, HealthState::Running);
    assert_eq!(services.health.snapshot().generation, 2);
    assert_sampler(&services, true, 2).await;
    services.shutdown().await.unwrap();
}
