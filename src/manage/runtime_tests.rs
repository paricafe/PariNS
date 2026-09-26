use super::*;
use crate::storage::HistoryOptions;

fn config() -> String {
    "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=200\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into()
}

fn identity_with_eku(
    name: &str,
    usages: Vec<rcgen::ExtendedKeyUsagePurpose>,
) -> rcgen::CertifiedKey<rcgen::KeyPair> {
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    params.extended_key_usages = usages;
    let signing_key = rcgen::KeyPair::generate().unwrap();
    rcgen::CertifiedKey {
        cert: params.self_signed(&signing_key).unwrap(),
        signing_key,
    }
}

fn subscription_config(enabled: bool) -> String {
    format!(
        "{}\n[filter_subscriptions]\nenabled={enabled}\n[[filter_subscriptions.sources]]\nid='online'\nname='Online'\nurl='https://example.org/fixture.list'\nformat='domain_list'\nenabled=true\nauto_update=false\n",
        config()
    )
}

fn seed_subscription(path: &std::path::Path) {
    use crate::filter_subscriptions::{
        download::{REPRESENTATION, Validators},
        store::{PreparedMetadata, Store as Sources},
    };
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let config = Config::parse(&subscription_config(true)).unwrap();
    let mut sources = Sources::open(path, config.filter_subscriptions.max_disk_bytes, 1).unwrap();
    let bytes = b".blocked.test\n";
    let hash = format!("{:x}", Sha256::digest(bytes));
    let mut stage = sources.begin_staging(bytes.len() as u64, 1).unwrap();
    stage.file.write_all(bytes).unwrap();
    sources
        .prepare(
            stage,
            PreparedMetadata {
                fingerprint: config
                    .filter_subscriptions
                    .fingerprints()
                    .unwrap()
                    .remove(0),
                sha256: hash.clone(),
                bytes: bytes.len() as u64,
                rules: 1,
                validators: Validators {
                    final_url: "https://example.org/fixture.list".into(),
                    representation: REPRESENTATION.into(),
                    content_sha256: hash,
                    etag: None,
                    last_modified: None,
                },
            },
            1,
        )
        .unwrap();
    sources
        .set_references(
            config.filter_subscriptions.fingerprints().unwrap(),
            vec![],
            1,
        )
        .unwrap();
}

#[tokio::test]
async fn subscription_hot_apply_retains_runtime_and_offline_rules_allow_exceptions() {
    use hickory_proto::rr::Name;
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state")).unwrap();
    seed_subscription(&store.dir.join("filter-subscriptions"));
    store
        .save(&Stored {
            username: "admin".into(),
            password_hash: super::super::store::hash_password("local-test-password").unwrap(),
            toml: subscription_config(true),
            previous: None,
            revision: 1,
        })
        .unwrap();
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: vec![],
    }));
    let mut manager = Manager::open(store, "127.0.0.1:3000".parse().unwrap(), active.clone())
        .await
        .unwrap();
    let resolver = manager
        .resolver()
        .expect("verified offline startup")
        .clone();
    let cache = resolver.cache();
    let name = Name::from_ascii("child.blocked.test").unwrap();
    assert_eq!(
        serde_json::to_value(manager.filters.explain(&name)).unwrap()["decision"],
        "blocked"
    );
    let generation = manager.filters.snapshot().generation;
    let mut next = manager.saved.clone().unwrap();
    next.revision += 1;
    next.toml = next.toml.replace("name='Online'", "name='Renamed'");
    assert!(!manager.apply(next).await.unwrap());
    assert_eq!(manager.filters.snapshot().generation, generation);
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&cache, &resolver.cache()));
    let mut next = manager.saved.clone().unwrap();
    next.revision += 1;
    next.toml
        .push_str("\n[filter]\nenabled=true\nallow_suffix=['blocked.test']\n");
    assert!(!manager.apply(next).await.unwrap());
    assert_eq!(
        serde_json::to_value(manager.filters.explain(&name)).unwrap()["decision"],
        "allowed"
    );
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&cache, &resolver.cache()));
    assert_eq!(manager.filters.snapshot().config_revision, 3);
    manager.terminal_shutdown().await;
}

#[tokio::test]
async fn subscriptions_missing_material_keep_console_and_disable_recovers_local_dns() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state")).unwrap();
    store
        .save(&Stored {
            username: "admin".into(),
            password_hash: super::super::store::hash_password("local-test-password").unwrap(),
            toml: subscription_config(true),
            previous: None,
            revision: 1,
        })
        .unwrap();
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: vec![],
    }));
    let mut manager = Manager::open(store, "127.0.0.1:3000".parse().unwrap(), active)
        .await
        .unwrap();
    assert!(manager.resolver().is_none());
    assert!(manager.last_error.is_some());
    assert!(manager.validate(subscription_config(true)).await.is_err());
    let mut next = manager.saved.clone().unwrap();
    next.revision += 1;
    next.toml = subscription_config(false);
    manager.apply(next).await.unwrap();
    assert!(manager.resolver().is_some());
    assert_eq!(manager.saved.as_ref().unwrap().revision, 2);
    manager.terminal_shutdown().await;
}

#[tokio::test]
async fn local_policy_retirement_is_checked_before_managed_config_and_certificate_commit() {
    use crate::filter_subscriptions::store::Store as Sources;
    use hickory_proto::rr::Name;

    for corrupt_catalog in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let first = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let replacement = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let cert = temp.path().join("cert.pem");
        let key = temp.path().join("key.pem");
        std::fs::write(&cert, first.cert.pem()).unwrap();
        std::fs::write(&key, first.signing_key.serialize_pem()).unwrap();
        let store = Store::open(&temp.path().join("state")).unwrap();
        let sources = store.dir.join("filter-subscriptions");
        drop(Sources::open(&sources, 32 * 1024 * 1024, 1).unwrap());
        if corrupt_catalog {
            std::fs::write(sources.join("catalog.json"), b"invalid catalog").unwrap();
            assert!(Sources::read_only(&sources).is_err());
        }
        let toml = format!(
            "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n[filter]\nenabled=true\nblock_exact=['one.test']\n",
            config(),
            serde_json::json!(cert),
            serde_json::json!(key),
        );
        store
            .save(&Stored {
                username: "admin".into(),
                password_hash: super::super::store::hash_password("local-test-password").unwrap(),
                toml,
                previous: None,
                revision: 1,
            })
            .unwrap();
        let active = Arc::new(Mutex::new(Active {
            snapshot: Arc::new(Snapshot::initial()),
            sessions: vec![],
        }));
        let mut manager = Manager::open(store, "127.0.0.1:3000".parse().unwrap(), active.clone())
            .await
            .unwrap();
        assert!(manager.filters.ready());
        assert!(manager.filters.snapshot().sources.is_empty());
        let resolver = manager.resolver().unwrap().clone();
        // Keep the same generation Arc that an in-flight DNS request owns.
        let old_request = manager.filters.handle().snapshot();
        let mut second = manager.saved.clone().unwrap();
        second.revision += 1;
        second.toml = second.toml.replace("'one.test'", "'two.test'");
        assert!(!manager.apply(second).await.unwrap());
        let generation = manager.filters.snapshot().generation;
        assert_eq!(generation, old_request.number + 1);

        // Changed raw input/counts with identical publication content remain
        // saveable while a retired request is held, with or without a Store.
        let mut metadata = manager.saved.clone().unwrap();
        metadata.revision += 1;
        metadata.toml = metadata
            .toml
            .replace("'two.test'", "'two.test', 'two.test'");
        assert!(!manager.apply(metadata).await.unwrap());
        assert_eq!(manager.filters.snapshot().generation, generation);
        assert_eq!(manager.filters.snapshot().input_rules, 2);

        let saved = manager.saved.clone().unwrap();
        let persisted = std::fs::read(manager.store.dir.join("state.json")).unwrap();
        std::fs::write(&cert, replacement.cert.pem()).unwrap();
        std::fs::write(&key, replacement.signing_key.serialize_pem()).unwrap();
        let mut third = saved.clone();
        third.revision += 1;
        third.toml = third.toml.replace("'two.test', 'two.test'", "'three.test'");
        // Exercise both hot persistence and the listener/certificate commit
        // path. Busy must reject before either saves Config or stops live DNS.
        for restart in [false, true] {
            if restart {
                third.toml = third.toml.replace("max_inflight=16", "max_inflight=17");
            }
            let error = manager.apply(third.clone()).await.unwrap_err();
            assert_eq!(
                crate::filter_subscriptions::service::Failure::from_error(&error).code,
                "busy"
            );
            assert_eq!(
                std::fs::read(manager.store.dir.join("state.json")).unwrap(),
                persisted
            );
            assert_eq!(manager.saved.as_ref().unwrap().revision, saved.revision);
            assert_eq!(manager.saved.as_ref().unwrap().toml, saved.toml);
            assert_eq!(manager.filters.snapshot().generation, generation);
            assert!(
                manager
                    .filters
                    .handle()
                    .snapshot()
                    .policy
                    .blocks(&Name::from_ascii("two.test").unwrap())
            );
            assert!(
                !manager
                    .filters
                    .handle()
                    .snapshot()
                    .policy
                    .blocks(&Name::from_ascii("three.test").unwrap())
            );
            assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
            let web = active.lock().unwrap().snapshot.tls.clone().unwrap();
            assert_web_identity(web, &first).await;
        }

        drop(old_request);
        assert!(manager.apply(third).await.unwrap());
        assert_eq!(manager.filters.snapshot().generation, generation + 1);
        assert_eq!(
            manager.store.read().unwrap().unwrap().revision,
            saved.revision + 1
        );
        assert!(
            manager
                .filters
                .handle()
                .snapshot()
                .policy
                .blocks(&Name::from_ascii("three.test").unwrap())
        );
        let web = active.lock().unwrap().snapshot.tls.clone().unwrap();
        assert_web_identity(web, &replacement).await;
        manager.terminal_shutdown().await;
    }
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
    let client_only =
        identity_with_eku("dns.test", vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth]);
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

    let before_rejected = active
        .lock()
        .unwrap()
        .snapshot
        .certificates
        .as_ref()
        .unwrap()
        .summary();
    write(&client_only);
    let error = manager.reload_certificates("api").await.unwrap_err();
    assert!(
        format!("{error:#}").contains("server authentication"),
        "{error:#}"
    );
    assert_eq!(
        active
            .lock()
            .unwrap()
            .snapshot
            .certificates
            .as_ref()
            .unwrap()
            .summary(),
        before_rejected
    );
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
async fn client_auth_only_candidate_keeps_http_config_and_dns_active() {
    let temp = tempfile::tempdir().unwrap();
    let client_only =
        identity_with_eku("dns.test", vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth]);
    let cert_file = temp.path().join("candidate-cert.pem");
    let key_file = temp.path().join("candidate-key.pem");
    std::fs::write(&cert_file, client_only.cert.pem()).unwrap();
    std::fs::write(&key_file, client_only.signing_key.serialize_pem()).unwrap();

    let store = Store::open(&temp.path().join("state")).unwrap();
    let original = Stored {
        username: "admin".into(),
        password_hash: super::super::store::hash_password("local-unit-test-password").unwrap(),
        toml: config(),
        previous: None,
        revision: 1,
    };
    store.save(&original).unwrap();
    let state_before = std::fs::read(store.dir.join("state.json")).unwrap();
    let active = Arc::new(Mutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: Vec::new(),
    }));
    let mut manager = Manager::open(store, "127.0.0.1:3000".parse().unwrap(), active.clone())
        .await
        .unwrap();
    let resolver = manager.resolver().unwrap().clone();
    let generation = manager.generation;
    let snapshot = active.lock().unwrap().snapshot.clone();
    assert_eq!(snapshot.scheme, super::super::transport::Scheme::Http);
    let next = Stored {
        toml: format!(
            "{}\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
            original.toml,
            serde_json::json!(cert_file),
            serde_json::json!(key_file)
        ),
        revision: 2,
        ..original
    };
    let error = manager.apply(next).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("server authentication"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(manager.store.dir.join("state.json")).unwrap(),
        state_before
    );
    assert_eq!(manager.saved.as_ref().unwrap().revision, 1);
    assert_eq!(manager.generation, generation);
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&snapshot, &active.lock().unwrap().snapshot));
    assert_eq!(
        active.lock().unwrap().snapshot.scheme,
        super::super::transport::Scheme::Http
    );
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

#[tokio::test]
async fn update_settings_change_only_management_state_even_with_dns_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state")).unwrap();
    store
        .save(&Stored {
            username: "admin".into(),
            password_hash: super::super::store::hash_password("local-test-password").unwrap(),
            toml: config(),
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
    let certificates = active
        .lock()
        .unwrap()
        .snapshot
        .certificates
        .clone()
        .unwrap();
    let services = manager.services.clone();
    let generation = manager.generation;
    let storage_revision = manager.services.status().configured_revision;
    let mut next = manager.saved.clone().unwrap();
    next.revision += 1;
    next.toml
        .push_str("\n[updates]\nauto_check=false\ncheck_interval_hours=168\n");
    assert!(!manager.apply(next).await.unwrap());
    assert!(Arc::ptr_eq(&resolver, manager.resolver().unwrap()));
    assert!(Arc::ptr_eq(&cache, &manager.resolver().unwrap().cache()));
    assert!(Arc::ptr_eq(&services, &manager.services));
    assert!(Arc::ptr_eq(
        &certificates,
        active
            .lock()
            .unwrap()
            .snapshot
            .certificates
            .as_ref()
            .unwrap()
    ));
    assert_eq!(manager.generation, generation);
    assert_eq!(
        manager.services.status().configured_revision,
        storage_revision
    );
    manager.stop().await;
    let mut next = manager.saved.clone().unwrap();
    next.revision += 1;
    next.toml = next.toml.replace("auto_check=false", "auto_check=true");
    assert!(!manager.apply(next).await.unwrap());
    assert!(manager.resolver().is_none());
    assert_eq!(manager.generation, generation);
    manager.terminal_shutdown().await;
}
