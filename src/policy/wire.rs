//! Ignored, test-only Server/Resolver fixture. There is no release CLI or feature.
#[path = "wire_diagnose.rs"]
mod diagnose;

use super::{
    Policy,
    canonical::{Builder, Canonical, Format, Group, Limits},
    radix,
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io::{BufReader, Read, Write},
    net::SocketAddr,
    sync::Arc,
};

const NATSUKI_SHA256: &str = "d68e37b2a861e6e8ef85568db4237bb3e18d1a9f2476323b3dba977fe850af09";
const FIXTURES: [(&str, Group); 6] = [
    ("blocked.fs-wire.test", Group::BlockSuffix),
    ("allow.blocked.fs-wire.test", Group::AllowExact),
    ("safe.blocked.fs-wire.test", Group::AllowSuffix),
    ("alias.fs-wire.test", Group::AllowExact),
    ("target.fs-wire.test", Group::BlockSuffix),
    ("aliases.fs-wire.test", Group::AllowSuffix),
];

pub(super) struct Injection {
    pub(super) radix: Option<radix::Index>,
    trie: Node,
    pub(super) digest: [u8; 32],
}
#[derive(Default)]
struct Node {
    children: std::collections::HashMap<Vec<u8>, Node>,
    block_exact: bool,
    block_suffix: bool,
    allow_exact: bool,
    allow_suffix: bool,
}
impl Injection {
    pub(super) fn blocks(&self, name: &hickory_proto::rr::Name) -> bool {
        if let Some(index) = &self.radix {
            return index
                .lookup(name)
                .is_some_and(|matched| (matched.group as u8) >= 2);
        }
        // Exact former production trie walk: retain a genuine wire baseline.
        let normalized = name.to_lowercase();
        let mut labels = normalized.iter().rev().peekable();
        let mut node = &self.trie;
        let mut blocked = false;
        while let Some(label) = labels.next() {
            let Some(child) = node.children.get(label) else {
                break;
            };
            node = child;
            let exact = labels.peek().is_none();
            if node.allow_suffix || (exact && node.allow_exact) {
                return false;
            }
            blocked |= node.block_suffix || (exact && node.block_exact);
        }
        blocked
    }
}
impl fmt::Debug for Injection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireTestPolicy")
            .field("radix", &self.radix.is_some())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy)]
enum Engine {
    Trie,
    Radix,
}
impl Engine {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "trie" => Ok(Self::Trie),
            "radix" => Ok(Self::Radix),
            _ => anyhow::bail!("wire fixture engine must be trie or radix"),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Trie => "trie",
            Self::Radix => "radix",
        }
    }
}
fn canonical_policy(input: Canonical, engine: Engine) -> Result<Policy> {
    let digest = input.semantic_digest;
    let mut root = Node::default();
    let radix = match engine {
        Engine::Radix => Some(
            input
                .finish_radix()
                .map_err(|e| anyhow::anyhow!("radix fixture: {e:?}"))?,
        ),
        Engine::Trie => {
            input.visit_rules(|mut encoded, group, _| {
                let mut node = &mut root;
                while let Some((&length, rest)) = encoded.split_first() {
                    node = node
                        .children
                        .entry(rest[..usize::from(length)].to_vec())
                        .or_default();
                    encoded = &rest[usize::from(length)..];
                }
                match group {
                    Group::AllowSuffix => node.allow_suffix = true,
                    Group::AllowExact => node.allow_exact = true,
                    Group::BlockSuffix => node.block_suffix = true,
                    Group::BlockExact => node.block_exact = true,
                }
            });
            None
        }
    };
    Ok(Policy {
        wire: Some(Arc::new(Injection {
            radix,
            trie: root,
            digest,
        })),
        ..Policy::default()
    })
}
fn add_fixtures(builder: &mut Builder) -> Result<()> {
    for (name, group) in FIXTURES {
        builder
            .add(name, group, 0)
            .map_err(|e| anyhow::anyhow!("fixture rule: {e:?}"))?;
    }
    Ok(())
}
struct Hashed<R> {
    inner: R,
    hash: Sha256,
}
impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn full_policy(path: &std::path::Path, engine: Engine) -> Result<(Policy, usize)> {
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == 4_392_582,
        "wire fixture requires fixed complete Natsuki .list"
    );
    let mut reader = BufReader::with_capacity(
        8192,
        Hashed {
            inner: file,
            hash: Sha256::new(),
        },
    );
    let mut builder = Builder::new(Limits {
        retained_bytes: 8192,
        ..Limits::default()
    })
    .map_err(|e| anyhow::anyhow!("fixture budget: {e:?}"))?;
    let stats = builder
        .parse_source(&mut reader, Format::DomainList, 1)
        .map_err(|e| anyhow::anyhow!("fixture parse: {e:?}"))?;
    ensure!(
        stats.input_rules == 201337 && stats.decoded_bytes == 4_392_582,
        "wrong complete fixture counts"
    );
    ensure!(
        hex(&reader.get_ref().hash.clone().finalize()) == NATSUKI_SHA256,
        "wrong fixed fixture SHA256"
    );
    add_fixtures(&mut builder)?;
    let input = builder
        .prepare()
        .map_err(|e| anyhow::anyhow!("fixture canonicalization: {e:?}"))?;
    let input_rules = input.input_rules;
    Ok((canonical_policy(input, engine)?, input_rules))
}

/// Parent harness provides loopback configuration and a verified fixed corpus.
/// Explicitly ignored: regular test runs never open a long-lived fixture server.
#[test]
#[ignore = "FS1e dedicated process; needs fixed corpus/config and external SIGTERM"]
fn serve() -> Result<()> {
    let config_path = std::env::var("PARINS_FS_WIRE_CONFIG")?;
    let rules_path = std::env::var("PARINS_FS_WIRE_RULES")?;
    let engine = Engine::parse(&std::env::var("PARINS_FS_WIRE_ENGINE")?)?;
    let driver = std::env::var("PARINS_FS_WIRE_DRIVER").unwrap_or_else(|_| "root".to_owned());
    ensure!(
        matches!(driver.as_str(), "root" | "worker"),
        "wire driver must be root or worker"
    );
    let mut config = crate::config::Config::parse(&std::fs::read_to_string(config_path)?)?;
    ensure!(
        config.listen.ip().is_loopback()
            && config
                .admin_listen
                .is_some_and(|address| address.ip().is_loopback()),
        "wire fixture listeners must be loopback, with metrics enabled"
    );
    ensure!(
        config.dot.is_none()
            && config.doh.is_none()
            && config.doq.is_none()
            && config.filter_file.is_none(),
        "wire fixture uses plain DNS and injected policy only"
    );
    for upstream in &config.upstreams.servers {
        let address: SocketAddr = upstream
            .strip_prefix("udp://")
            .unwrap_or(upstream)
            .parse()?;
        ensure!(
            address.ip().is_loopback(),
            "wire fixture upstream must be loopback UDP"
        );
    }
    let (policy, input_rules) = full_policy(std::path::Path::new(&rules_path), engine)?;
    let digest = hex(&policy.semantic_digest());
    config.filter = policy.into();
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?.block_on(async move {
        #[cfg(unix)]
        let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        #[cfg(not(unix))]
        anyhow::bail!("FS wire fixture requires Unix SIGTERM");
        #[cfg(unix)]
        {
            let server=crate::server::Server::bind(config).await?;
            let ready=serde_json::json!({"pid":std::process::id(),"dns":server.local_addr()?.to_string(),"metrics":server.admin_addr()?.expect("fixture metrics").to_string(),"engine":engine.name(),"driver":driver,"semantic_digest":digest,"input_rules":input_rules});
            println!("\nPARINS_FS_WIRE_READY {ready}");std::io::stdout().flush()?;
            let serving = server.run(async move {terminate.recv().await;});
            if driver == "worker" { tokio::spawn(serving).await? } else { serving.await }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, ecs::Scope, protocol, resolver::Resolver};
    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::{
            Name, RData, Record, RecordType,
            rdata::{A, CNAME},
        },
    };
    use std::time::{Duration, Instant};
    use tokio::net::UdpSocket;

    fn fixture(engine: Engine) -> Policy {
        let mut builder = Builder::new(Limits::default()).unwrap();
        add_fixtures(&mut builder).unwrap();
        canonical_policy(builder.prepare().unwrap(), engine).unwrap()
    }
    fn query(name: &str) -> Message {
        let mut message = Message::new(27, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        let mut name = Name::from_ascii(name).unwrap();
        name.set_fqdn(true);
        message.add_query(Query::query(name, RecordType::A));
        message
    }
    fn assert_blocked(query: &Message, reply: &Message) {
        assert_eq!(reply.response_code, ResponseCode::NoError);
        assert_eq!(reply.id, query.id);
        assert_eq!(reply.queries, query.queries);
        assert!(
            reply.answers.is_empty()
                && reply.authorities.is_empty()
                && reply.additionals.is_empty()
        );
        assert!(!reply.authoritative && !reply.authentic_data && !reply.truncation);
    }
    async fn resolve(resolver: &Resolver, query: &Message) -> Message {
        resolver
            .resolve(&query.to_vec().unwrap(), "127.0.0.1".parse().unwrap())
            .await
            .unwrap()
            .message
    }
    #[test]
    fn same_fixture_digest_and_policy_blocks_with_allow_boundaries() {
        let trie = fixture(Engine::Trie);
        let radix = fixture(Engine::Radix);
        assert_eq!(trie.semantic_digest(), radix.semantic_digest());
        for (name, blocked) in [
            ("blocked.fs-wire.test", true),
            ("sub.blocked.fs-wire.test", true),
            ("allow.blocked.fs-wire.test", false),
            ("sub.allow.blocked.fs-wire.test", true),
            ("safe.blocked.fs-wire.test", false),
            ("sub.safe.blocked.fs-wire.test", false),
            ("alias.fs-wire.test", false),
            ("q12.aliases.fs-wire.test", false),
            ("warm.fs-wire.test", false),
            ("target.fs-wire.test", true),
        ] {
            let name = Name::from_ascii(name).unwrap();
            assert_eq!(trie.blocks(&name), blocked);
            assert_eq!(radix.blocks(&name), blocked);
        }
        assert!(Engine::parse("compact").is_err());
    }
    #[tokio::test]
    async fn real_resolver_cold_fresh_stale_and_cname_use_the_injected_policy() {
        for engine in [Engine::Trie, Engine::Radix] {
            let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut config=Config::parse(&format!("listen='127.0.0.1:0'\nquery_timeout_ms=50\ntcp_io_timeout_ms=1000\nshutdown_grace_ms=1000\nmax_inflight=128\nmax_tcp_connections=32\n[upstreams]\nservers=['udp://{}']\n[cache.stale]\nenabled=true\n",upstream.local_addr().unwrap())).unwrap();
            let fixture = fixture(engine);
            let digest = fixture.semantic_digest();
            config.filter = fixture.clone().into();
            let cloned = config.load_policy().unwrap();
            assert!(Arc::ptr_eq(
                fixture.wire.as_ref().unwrap(),
                cloned.wire.as_ref().unwrap()
            ));
            let resolver = Resolver::from_config(&config);
            assert_eq!(resolver.policy_digest(), digest);
            let mock = tokio::spawn(async move {
                for _ in 0..4 {
                    let mut bytes = [0; 4096];
                    let (length, peer) = tokio::time::timeout(
                        Duration::from_secs(2),
                        upstream.recv_from(&mut bytes),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    let request = protocol::decode(&bytes[..length]).unwrap();
                    let mut reply = protocol::error_response(&request, ResponseCode::NoError);
                    let alias = request.queries[0].name().to_ascii() == "alias.fs-wire.test.";
                    let target = if alias {
                        Name::from_ascii("target.fs-wire.test.").unwrap()
                    } else {
                        request.queries[0].name().clone()
                    };
                    if alias {
                        reply.add_answer(Record::from_rdata(
                            request.queries[0].name().clone(),
                            60,
                            RData::CNAME(CNAME(target.clone())),
                        ));
                    }
                    reply.add_answer(Record::from_rdata(
                        target,
                        60,
                        RData::A(A::new(192, 0, 2, 1)),
                    ));
                    upstream
                        .send_to(&reply.to_vec().unwrap(), peer)
                        .await
                        .unwrap();
                }
            });
            let direct = query("sub.blocked.fs-wire.test");
            assert_blocked(&direct, &resolve(&resolver, &direct).await);
            let mut cached = Vec::new();
            for name in [
                "warm.fs-wire.test",
                "allow.blocked.fs-wire.test",
                "child.safe.blocked.fs-wire.test",
                "alias.fs-wire.test",
            ] {
                let request = query(name);
                for _ in 0..2 {
                    let reply = resolve(&resolver, &request).await;
                    if name == "alias.fs-wire.test" {
                        assert_blocked(&request, &reply);
                    } else {
                        assert_eq!(reply.answers.len(), 1);
                    }
                }
                let raw = resolver
                    .cache()
                    .lookup(&request, None, Instant::now(), false)
                    .unwrap()
                    .message;
                assert_eq!(
                    raw.answers.len(),
                    if name == "alias.fs-wire.test" { 2 } else { 1 }
                );
                cached.push((request, raw));
            }
            mock.await.unwrap();
            assert_eq!(
                resolver.metrics().snapshot().counters["upstream_operations"],
                4
            );
            assert_eq!(resolver.metrics().snapshot().counters["cache_hits"], 4);
            for (request, raw) in cached {
                assert!(resolver.cache().insert_if_epoch(
                    &request,
                    &raw,
                    Scope::NoEcs,
                    Instant::now() - Duration::from_secs(61),
                    resolver.cache().epoch()
                ));
                assert!(
                    resolver
                        .cache()
                        .lookup(&request, None, Instant::now(), false)
                        .is_none()
                );
                let reply = resolve(&resolver, &request).await;
                if request.queries[0].name().to_ascii() == "alias.fs-wire.test." {
                    assert_blocked(&request, &reply);
                } else {
                    assert_eq!(reply.answers.len(), 1);
                    assert_eq!(reply.answers[0].ttl, config.cache.stale.reply_ttl_secs);
                }
            }
            assert_eq!(
                resolver.metrics().snapshot().counters["cache_lookup_stale"],
                4
            );
            assert_eq!(resolver.policy_digest(), digest);
            resolver.shutdown_refresh().await;
        }
    }
}
