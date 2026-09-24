use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use parins::{
    cache::Cache,
    cache_persistence::{CLEAN_FILE, CachePersistence},
    config::{CacheConfig, CachePersistenceConfig},
    ecs::Scope,
    protocol,
};
use std::{
    fs,
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime},
};

const FP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn filled() -> (Cache, Message, Instant) {
    let cache = Cache::new(CacheConfig::default());
    let mut query = Message::new(1, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(
        Name::from_ascii("cached.test.").unwrap(),
        RecordType::A,
    ));
    let mut answer = protocol::error_response(&query, ResponseCode::NoError);
    answer.add_answer(Record::from_rdata(
        query.queries[0].name().clone(),
        60,
        RData::A(A::new(192, 0, 2, 1)),
    ));
    let now = Instant::now();
    cache.insert(&query, &answer, Scope::NoEcs, now);
    (cache, query, now)
}

fn private_write(path: &std::path::Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn clean_file_is_one_shot_and_tmp_is_never_a_recovery_source() {
    let dir = tempfile::tempdir().unwrap();
    let settings = CachePersistenceConfig::default();
    let cancelled = AtomicBool::new(false);
    let owner = CachePersistence::new(dir.path());
    assert_eq!(
        owner
            .consume_startup(&settings, &cancelled)
            .unwrap()
            .report()
            .reason
            .as_deref(),
        Some("no_clean_snapshot")
    );
    let (cache, query, now) = filled();
    let wall = SystemTime::now();
    assert_eq!(
        owner
            .save_terminal(&cache, FP, &settings, wall, now, &cancelled)
            .unwrap()
            .saved,
        1
    );
    let clean = dir.path().join(CLEAN_FILE);
    assert!(clean.exists());
    assert!(
        owner
            .save_terminal(&cache, FP, &settings, wall, now, &cancelled)
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&clean).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::copy(&clean, dir.path().join("dns-cache.tmp.old")).unwrap();
    let startup = CachePersistence::new(dir.path());
    let candidate = startup.consume_startup(&settings, &cancelled).unwrap();
    assert!(
        !clean.exists(),
        "recovery eligibility must be revoked before serving"
    );
    let restored = Cache::new(CacheConfig::default());
    let report = candidate.restore_into(
        &restored,
        FP,
        wall + Duration::from_secs(2),
        now,
        &cancelled,
    );
    assert_eq!(report.restored, 1);
    assert_eq!(
        restored
            .peek(&query, None, now, false)
            .unwrap()
            .message
            .answers[0]
            .ttl,
        58
    );
    let after_crash = CachePersistence::new(dir.path());
    assert_eq!(
        after_crash
            .consume_startup(&settings, &cancelled)
            .unwrap()
            .report()
            .reason
            .as_deref(),
        Some("no_clean_snapshot")
    );
    assert!(dir.path().join("dns-cache.tmp.old").exists());
}

#[test]
fn disabled_oversized_invalid_and_cancelled_files_are_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let clean = dir.path().join(CLEAN_FILE);
    let (cache, _, now) = filled();
    for (settings, cancel, expected) in [
        (
            CachePersistenceConfig {
                enabled: false,
                ..CachePersistenceConfig::default()
            },
            false,
            "persistence_disabled",
        ),
        (
            CachePersistenceConfig {
                max_bytes: 1,
                ..CachePersistenceConfig::default()
            },
            false,
            "byte budget",
        ),
        (CachePersistenceConfig::default(), true, "cancelled"),
        (CachePersistenceConfig::default(), false, "truncated"),
    ] {
        private_write(&clean, b"broken snapshot");
        let cancelled = AtomicBool::new(cancel);
        let owner = CachePersistence::new(dir.path());
        let candidate = owner.consume_startup(&settings, &cancelled).unwrap();
        assert!(!clean.exists());
        let report = candidate.restore_into(&cache, FP, SystemTime::now(), now, &cancelled);
        assert!(report.reason.unwrap().contains(expected));
    }
}

#[test]
fn failed_startup_consumption_and_non_quiescent_cancel_do_not_publish() {
    let dir = tempfile::tempdir().unwrap();
    let settings = CachePersistenceConfig::default();
    let cancel = AtomicBool::new(false);
    let owner = CachePersistence::new(dir.path());
    let (cache, _, now) = filled();
    assert!(
        owner
            .save_terminal(&cache, FP, &settings, SystemTime::now(), now, &cancel)
            .is_err()
    );
    fs::create_dir(dir.path().join(CLEAN_FILE)).unwrap();
    assert!(owner.consume_startup(&settings, &cancel).is_err());
    assert!(
        owner
            .save_terminal(&cache, FP, &settings, SystemTime::now(), now, &cancel)
            .is_err()
    );
    fs::remove_dir(dir.path().join(CLEAN_FILE)).unwrap();
    owner.consume_startup(&settings, &cancel).unwrap();
    cancel.store(true, std::sync::atomic::Ordering::Release);
    assert!(
        owner
            .save_terminal(&cache, FP, &settings, SystemTime::now(), now, &cancel)
            .is_err()
    );
    assert!(!dir.path().join(CLEAN_FILE).exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn linked_files_are_rejected_without_touching_external_material() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("certificate.pem");
    private_write(&external, b"external unchanged");
    let clean = dir.path().join(CLEAN_FILE);
    std::os::unix::fs::symlink(&external, &clean).unwrap();
    let owner = CachePersistence::new(dir.path());
    assert!(
        owner
            .consume_startup(&CachePersistenceConfig::default(), &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(fs::read(&external).unwrap(), b"external unchanged");
    fs::remove_file(&clean).unwrap();
    fs::hard_link(&external, &clean).unwrap();
    assert!(
        owner
            .consume_startup(&CachePersistenceConfig::default(), &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(fs::read(&external).unwrap(), b"external unchanged");
}

#[cfg(unix)]
mod process {
    use super::*;
    use hickory_proto::{op::Edns, rr::rdata::opt::EdnsOption};
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicU8, AtomicUsize, Ordering},
        },
    };
    use tokio::{
        net::UdpSocket,
        time::{sleep, timeout},
    };

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    struct Upstream {
        address: SocketAddr,
        requests: Arc<AtomicUsize>,
        mode: Arc<AtomicU8>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Upstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn upstream(ttl: u32) -> Upstream {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicU8::new(0));
        let task = tokio::spawn({
            let requests = Arc::clone(&requests);
            let mode = Arc::clone(&mode);
            async move {
                let mut buffer = [0; 4096];
                loop {
                    let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
                    let query = protocol::decode(&buffer[..length]).unwrap();
                    let mode = mode.load(Ordering::Acquire);
                    requests.fetch_add(1, Ordering::Release);
                    if mode == 4 {
                        continue;
                    }
                    let mut response = protocol::error_response(&query, ResponseCode::NoError);
                    response.add_answer(Record::from_rdata(
                        query.queries[0].name().clone(),
                        if mode == 1 { 0 } else { ttl },
                        RData::A(A::new(192, 0, 2, mode + 1)),
                    ));
                    response.add_authority(Record::from_rdata(
                        Name::from_ascii("test.").unwrap(),
                        ttl + 10,
                        RData::A(A::new(192, 0, 2, 20)),
                    ));
                    response.add_additional(Record::from_rdata(
                        Name::from_ascii("extra.test.").unwrap(),
                        ttl + 20,
                        RData::A(A::new(192, 0, 2, 30)),
                    ));
                    if mode == 2 {
                        let mut edns = Edns::new();
                        edns.options_mut()
                            .insert(EdnsOption::Unknown(15, vec![0, 0]));
                        response.edns = Some(edns);
                    }
                    socket
                        .send_to(&response.to_vec().unwrap(), peer)
                        .await
                        .unwrap();
                }
            }
        });
        Upstream {
            address,
            requests,
            mode,
            task,
        }
    }

    fn configuration(directory: &Path, upstream: SocketAddr, prefetch: bool) -> PathBuf {
        let mut config =
            parins::config::Config::parse(include_str!("../parins.example.toml")).unwrap();
        config.listen = "127.0.0.1:0".parse().unwrap();
        config.upstreams.servers = vec![upstream.to_string()];
        config.cache.prefetch.enabled = prefetch;
        config.cache.prefetch.min_hits = 1;
        config.cache.prefetch.remaining_percent = 90;
        let path = directory.join("config.toml");
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        path
    }

    async fn start(directory: &Path, config: &Path, generation: usize) -> (ChildGuard, SocketAddr) {
        let stderr_path = directory.join(format!("stderr-{generation}.txt"));
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_parins"))
                .arg("--config")
                .arg(config)
                .arg("--data-dir")
                .arg(directory.join("data"))
                .current_dir(directory)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(&stderr_path).unwrap())
                .spawn()
                .unwrap(),
        );
        let address = timeout(Duration::from_secs(15), async {
            loop {
                let stderr = fs::read_to_string(&stderr_path).unwrap();
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "server exited before readiness: {stderr}"
                );
                if let Some(address) = stderr.lines().find_map(|line| {
                    line.strip_prefix("PariNS listening on ")
                        .and_then(|line| line.strip_suffix(" (UDP/TCP)"))
                }) {
                    break address.parse::<SocketAddr>().unwrap();
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("isolated process startup timeout");
        (child, address)
    }

    async fn stop(child: &mut ChildGuard, graceful: bool) {
        if graceful {
            assert!(
                Command::new("kill")
                    .args(["-TERM", &child.0.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            child.0.kill().unwrap();
        }
        let status = timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("isolated process shutdown timeout");
        assert_eq!(status.success(), graceful);
    }

    async fn request(address: SocketAddr) -> Message {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_, query, _) = filled();
        socket
            .send_to(&query.to_vec().unwrap(), address)
            .await
            .unwrap();
        let mut buffer = [0; 4096];
        let (length, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        protocol::decode(&buffer[..length]).unwrap()
    }

    fn address(response: &Message) -> u8 {
        match &response.answers[0].data {
            RData::A(value) => value.0.octets()[3],
            other => panic!("unexpected answer {other:?}"),
        }
    }

    #[tokio::test]
    async fn binary_normal_restart_restores_fresh_wire_and_sigkill_restarts_cold() {
        let directory = tempfile::tempdir().unwrap();
        let mock = upstream(60).await;
        let config = configuration(directory.path(), mock.address, false);
        let (mut first, endpoint) = start(directory.path(), &config, 1).await;
        let initial = request(endpoint).await;
        assert_eq!(address(&initial), 1);
        assert_eq!(mock.requests.load(Ordering::Acquire), 1);
        stop(&mut first, true).await;
        let clean = directory.path().join("data").join(CLEAN_FILE);
        assert!(clean.is_file());
        let (mut second, endpoint) = start(directory.path(), &config, 2).await;
        assert!(!clean.exists(), "clean recovery consumed before serving");
        let recovered = request(endpoint).await;
        assert_eq!(address(&recovered), 1);
        assert_eq!(
            mock.requests.load(Ordering::Acquire),
            1,
            "warm restart must not use upstream"
        );
        for (old, recovered) in initial
            .answers
            .iter()
            .chain(&initial.authorities)
            .chain(&initial.additionals)
            .zip(
                recovered
                    .answers
                    .iter()
                    .chain(&recovered.authorities)
                    .chain(&recovered.additionals),
            )
        {
            assert!(
                recovered.ttl < old.ttl,
                "TTL must account for both snapshot and offline age"
            );
        }
        stop(&mut second, false).await;
        assert!(!clean.exists());
        mock.mode.store(3, Ordering::Release);
        let (mut third, endpoint) = start(directory.path(), &config, 3).await;
        assert_eq!(address(&request(endpoint).await), 4);
        assert_eq!(mock.requests.load(Ordering::Acquire), 2);
        stop(&mut third, true).await;
    }

    #[tokio::test]
    async fn binary_ttl_zero_and_ede_supersession_cannot_resurrect_consumed_old_snapshot() {
        for replacement in [1, 2] {
            let directory = tempfile::tempdir().unwrap();
            let mock = upstream(6).await;
            let config = configuration(directory.path(), mock.address, true);
            let (mut first, endpoint) = start(directory.path(), &config, 1).await;
            assert_eq!(address(&request(endpoint).await), 1);
            stop(&mut first, true).await;
            let (mut second, endpoint) = start(directory.path(), &config, 2).await;
            assert_eq!(address(&request(endpoint).await), 1);
            assert_eq!(mock.requests.load(Ordering::Acquire), 1);
            mock.mode.store(replacement, Ordering::Release);
            // The restored short-lived answer crosses the configured prefetch
            // threshold. Its new non-admissible response supersedes old data.
            sleep(Duration::from_millis(700)).await;
            request(endpoint).await;
            timeout(Duration::from_secs(5), async {
                loop {
                    if address(&request(endpoint).await) == replacement + 1 {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert!(mock.requests.load(Ordering::Acquire) > 1);
            assert!(!directory.path().join("data").join(CLEAN_FILE).exists());
            stop(&mut second, false).await;
            mock.mode.store(3, Ordering::Release);
            let before = mock.requests.load(Ordering::Acquire);
            let (mut third, endpoint) = start(directory.path(), &config, 3).await;
            assert_eq!(address(&request(endpoint).await), 4);
            assert_eq!(mock.requests.load(Ordering::Acquire), before + 1);
            stop(&mut third, true).await;
        }
    }

    #[test]
    fn binary_check_does_not_create_runtime_files() {
        let directory = tempfile::tempdir().unwrap();
        let config = configuration(directory.path(), "127.0.0.1:1".parse().unwrap(), false);
        let data = directory.path().join("absent-data");
        let output = Command::new(env!("CARGO_BIN_EXE_parins"))
            .arg("--config")
            .arg(config)
            .arg("--data-dir")
            .arg(&data)
            .arg("--check")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!data.exists());
    }

    #[tokio::test]
    async fn binary_foreground_drain_timeout_does_not_publish_clean_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mock = upstream(60).await;
        let path = configuration(directory.path(), mock.address, false);
        let mut config = parins::config::Config::load(&path).unwrap();
        config.shutdown_grace_ms = 20;
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let (mut first, endpoint) = start(directory.path(), &path, 1).await;
        assert_eq!(address(&request(endpoint).await), 1);
        mock.mode.store(4, Ordering::Release);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut query = Message::new(2, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(
            Name::from_ascii("pending.test.").unwrap(),
            RecordType::A,
        ));
        socket
            .send_to(&query.to_vec().unwrap(), endpoint)
            .await
            .unwrap();
        timeout(Duration::from_secs(5), async {
            while mock.requests.load(Ordering::Acquire) < 2 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop(&mut first, true).await;
        assert!(!directory.path().join("data").join(CLEAN_FILE).exists());
        let stderr = fs::read_to_string(directory.path().join("stderr-1.txt")).unwrap();
        assert!(stderr.contains("shutdown_not_quiescent"), "{stderr}");
        mock.mode.store(3, Ordering::Release);
        let (mut second, endpoint) = start(directory.path(), &path, 2).await;
        assert_eq!(address(&request(endpoint).await), 4);
        assert_eq!(mock.requests.load(Ordering::Acquire), 3);
        stop(&mut second, true).await;
    }
}
