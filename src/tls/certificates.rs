//! One prepared identity generation shared by every inbound TLS transport.
//! Call preparation on a blocking worker; resolution and publication do no IO.

use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, ensure};
use rustls::{
    ServerConfig,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::{TlsFiles, validate_management_identity, validate_pem_identity};

// Match imported identity bounds. At most six distinct files are read per set.
pub(super) const MAX_PEM_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateRole {
    Dot,
    Doh,
    Doq,
}

impl CertificateRole {
    const ALL: [Self; 3] = [Self::Dot, Self::Doh, Self::Doq];

    fn index(self) -> usize {
        match self {
            Self::Dot => 0,
            Self::Doh => 1,
            Self::Doq => 2,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CertificateSources {
    pub dot: Option<TlsFiles>,
    pub doh: Option<TlsFiles>,
    pub doq: Option<TlsFiles>,
    pub doh_public_host: Option<String>,
}

impl CertificateSources {
    fn files(&self, role: CertificateRole) -> Option<&TlsFiles> {
        match role {
            CertificateRole::Dot => self.dot.as_ref(),
            CertificateRole::Doh => self.doh.as_ref(),
            CertificateRole::Doq => self.doq.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RoleSummary {
    pub role: CertificateRole,
    pub leaf_sha256: String,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CertificateSummary {
    pub certificate_generation: u64,
    pub roles: Vec<RoleSummary>,
}

#[derive(Debug)]
pub struct PreparedCertificateSet {
    owner: Arc<()>,
    keys: [Option<Arc<CertifiedKey>>; 3],
    summary: CertificateSummary,
}

#[derive(Debug)]
pub struct CertificateSet {
    sources: CertificateSources,
    owner: Arc<()>,
    active: RwLock<Arc<PreparedCertificateSet>>,
}

impl CertificateSet {
    /// Build an independent candidate for one configuration generation.
    pub fn prepare(sources: CertificateSources) -> Result<Arc<Self>> {
        let owner = Arc::new(());
        let mut prepared = prepare(&sources, owner.clone())?;
        prepared.summary.certificate_generation = u64::from(!prepared.summary.roles.is_empty());
        Ok(Arc::new(Self {
            sources,
            owner,
            active: RwLock::new(Arc::new(prepared)),
        }))
    }

    pub fn prepare_reload(&self) -> Result<PreparedCertificateSet> {
        prepare(&self.sources, self.owner.clone())
    }

    /// Caller serializes reload with configuration apply and shutdown. A
    /// candidate from another configuration's set can never be installed here.
    /// The lock contains only comparisons and the single snapshot replacement.
    pub fn publish(&self, mut prepared: PreparedCertificateSet) -> Result<bool> {
        ensure!(
            Arc::ptr_eq(&self.owner, &prepared.owner),
            "certificate candidate belongs to another configuration"
        );
        let mut active = self
            .active
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let unchanged = active
            .keys
            .iter()
            .zip(&prepared.keys)
            .all(|(old, new)| match (old, new) {
                (None, None) => true,
                (Some(old), Some(new)) => old.cert == new.cert,
                _ => false,
            });
        if unchanged {
            return Ok(false);
        }
        prepared.summary.certificate_generation = active
            .summary
            .certificate_generation
            .checked_add(1)
            .context("certificate generation exhausted")?;
        *active = Arc::new(prepared);
        Ok(true)
    }

    pub fn summary(&self) -> CertificateSummary {
        self.active
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .summary
            .clone()
    }

    pub fn key(&self, role: CertificateRole) -> Option<Arc<CertifiedKey>> {
        self.active
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .keys[role.index()]
        .clone()
    }

    pub fn server_config(
        self: &Arc<Self>,
        role: CertificateRole,
        alpn: &[&[u8]],
    ) -> Result<Arc<ServerConfig>> {
        ensure!(
            self.key(role).is_some(),
            "TLS certificate role is not configured"
        );
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(IdentityResolver {
                    set: self.clone(),
                    role,
                }));
        config.alpn_protocols = alpn.iter().map(|name| name.to_vec()).collect();
        Ok(Arc::new(config))
    }
}

#[derive(Debug)]
struct IdentityResolver {
    set: Arc<CertificateSet>,
    role: CertificateRole,
}

impl ResolvesServerCert for IdentityResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.set.key(self.role)
    }
}

fn prepare(sources: &CertificateSources, owner: Arc<()>) -> Result<PreparedCertificateSet> {
    let mut files = HashMap::<PathBuf, Vec<u8>>::new();
    let mut identities = HashMap::<(PathBuf, PathBuf), Arc<CertifiedKey>>::new();
    let mut keys = [None, None, None];
    let mut roles = Vec::new();
    for role in CertificateRole::ALL {
        let Some(source) = sources.files(role) else {
            continue;
        };
        let pair = (source.cert_file.clone(), source.key_file.clone());
        let key = if let Some(key) = identities.get(&pair) {
            key.clone()
        } else {
            for path in [&source.cert_file, &source.key_file] {
                if !files.contains_key(path) {
                    files.insert(
                        path.clone(),
                        read_pem_file(path).context("read TLS identity material")?,
                    );
                }
            }
            let key = validate_pem_identity(&files[&source.cert_file], &files[&source.key_file])?;
            identities.insert(pair, key.clone());
            key
        };
        if role == CertificateRole::Doh
            && let Some(host) = &sources.doh_public_host
        {
            validate_management_identity(&key, host)?;
        }
        let leaf = key.cert.first().context("TLS certificate chain is empty")?;
        let (_, parsed) = X509Certificate::from_der(leaf.as_ref())
            .map_err(|_| anyhow::anyhow!("invalid TLS certificate"))?;
        roles.push(RoleSummary {
            role,
            leaf_sha256: format!("{:x}", Sha256::digest(leaf.as_ref())),
            not_before_ms: parsed
                .validity()
                .not_before
                .timestamp()
                .checked_mul(1000)
                .context("invalid certificate start date")?,
            not_after_ms: parsed
                .validity()
                .not_after
                .timestamp()
                .checked_mul(1000)
                .context("invalid certificate expiry date")?,
        });
        keys[role.index()] = Some(key);
    }
    Ok(PreparedCertificateSet {
        owner,
        keys,
        summary: CertificateSummary {
            certificate_generation: 0,
            roles,
        },
    })
}

pub(super) fn read_pem_file(path: &Path) -> Result<Vec<u8>> {
    // NONBLOCK prevents FIFO opens from stalling before fstat rejects them.
    // Symlinks are intentional: external renewers commonly rotate symlink targets.
    #[cfg(unix)]
    let file = File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(not(unix))]
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "TLS material must be a regular file");
    ensure!(
        metadata.len() <= MAX_PEM_BYTES as u64,
        "TLS PEM exceeds the 64 KiB size limit"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PEM_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_PEM_BYTES,
        "TLS PEM exceeds the 64 KiB size limit"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{
        ClientConfig, ClientConnection, RootCertStore, ServerConnection,
        pki_types::{CertificateDer, ServerName},
    };

    fn identity(directory: &Path, name: &str) -> (TlsFiles, CertificateDer<'static>) {
        identity_with_eku(directory, name, vec![])
    }

    fn identity_with_eku(
        directory: &Path,
        name: &str,
        usages: Vec<rcgen::ExtendedKeyUsagePurpose>,
    ) -> (TlsFiles, CertificateDer<'static>) {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = usages;
        let signing_key = rcgen::KeyPair::generate().unwrap();
        let generated = params.self_signed(&signing_key).unwrap();
        let files = TlsFiles {
            cert_file: directory.join(format!("{name}.pem")),
            key_file: directory.join(format!("{name}.key")),
        };
        std::fs::write(&files.cert_file, generated.pem()).unwrap();
        std::fs::write(&files.key_file, signing_key.serialize_pem()).unwrap();
        (files, generated.der().clone())
    }

    fn replace(source: &TlsFiles, destination: &TlsFiles) {
        std::fs::copy(&source.cert_file, &destination.cert_file).unwrap();
        std::fs::copy(&source.key_file, &destination.key_file).unwrap();
    }

    fn sources(files: &TlsFiles) -> CertificateSources {
        CertificateSources {
            dot: Some(files.clone()),
            doh: Some(files.clone()),
            doq: Some(files.clone()),
            doh_public_host: Some("localhost".into()),
        }
    }

    fn handshake(
        config: Arc<ServerConfig>,
        roots: &[CertificateDer<'static>],
        alpn: &[u8],
    ) -> CertificateDer<'static> {
        let mut store = RootCertStore::empty();
        for root in roots {
            store.add(root.clone()).unwrap();
        }
        let mut config_client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(store)
                .with_no_client_auth();
        config_client.alpn_protocols = vec![alpn.to_vec()];
        let mut client = ClientConnection::new(
            Arc::new(config_client),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = ServerConnection::new(config).unwrap();
        for _ in 0..10 {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                server.read_tls(&mut bytes.as_slice()).unwrap();
                server.process_new_packets().unwrap();
            }
            bytes.clear();
            server.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                client.read_tls(&mut bytes.as_slice()).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }
        assert!(!client.is_handshaking());
        assert_eq!(client.alpn_protocol(), Some(alpn));
        client.peer_certificates().unwrap()[0].clone()
    }

    #[test]
    fn one_publication_updates_all_roles_and_existing_resolvers_without_file_io() {
        let directory = tempfile::tempdir().unwrap();
        let (files, old) = identity(directory.path(), "active");
        let (new_files, new) = identity(directory.path(), "new");
        let set = CertificateSet::prepare(sources(&files)).unwrap();
        let old_summary = set.summary();
        assert_eq!(old_summary.certificate_generation, 1);
        assert_eq!(old_summary.roles.len(), 3);
        assert_eq!(
            old_summary.roles[0].leaf_sha256,
            format!("{:x}", Sha256::digest(old.as_ref()))
        );
        assert!(Arc::ptr_eq(
            &set.key(CertificateRole::Dot).unwrap(),
            &set.key(CertificateRole::Doh).unwrap()
        ));
        let roles = [
            (CertificateRole::Dot, &b"dot"[..]),
            (CertificateRole::Doh, &b"h2"[..]),
            (CertificateRole::Doh, &b"h3"[..]),
            (CertificateRole::Doh, &b"http/1.1"[..]),
            (CertificateRole::Doq, &b"doq"[..]),
        ];
        let configs: Vec<_> = roles
            .iter()
            .map(|(role, alpn)| set.server_config(*role, &[*alpn]).unwrap())
            .collect();
        assert!(!set.publish(set.prepare_reload().unwrap()).unwrap());
        assert_eq!(set.summary(), old_summary);
        replace(&new_files, &files);
        let prepared = set.prepare_reload().unwrap();
        assert_eq!(set.summary(), old_summary);
        assert_eq!(set.key(CertificateRole::Doh).unwrap().cert[0], old);
        assert!(set.publish(prepared).unwrap());
        assert_eq!(set.summary().certificate_generation, 2);
        std::fs::remove_file(files.cert_file).unwrap();
        std::fs::remove_file(files.key_file).unwrap();
        for ((_, alpn), config) in roles.iter().zip(configs) {
            assert_eq!(handshake(config, &[old.clone(), new.clone()], alpn), new);
        }
    }

    #[test]
    fn partial_rotation_and_cross_generation_candidates_preserve_the_whole_set() {
        let directory = tempfile::tempdir().unwrap();
        let (files, _) = identity(directory.path(), "active");
        let (new_files, _) = identity(directory.path(), "new");
        let set = CertificateSet::prepare(sources(&files)).unwrap();
        let initial = set.summary();
        std::fs::copy(&new_files.cert_file, &files.cert_file).unwrap();
        assert!(set.prepare_reload().is_err());
        assert_eq!(set.summary(), initial);
        replace(&new_files, &files);
        let other = CertificateSet::prepare(sources(&files)).unwrap();
        assert!(set.publish(other.prepare_reload().unwrap()).is_err());
        assert_eq!(set.summary(), initial);
        assert!(set.publish(set.prepare_reload().unwrap()).unwrap());
        std::fs::write(&files.key_file, "not PEM").unwrap();
        assert!(set.prepare_reload().is_err());
        assert_eq!(set.summary().certificate_generation, 2);
    }

    #[test]
    fn failure_in_the_last_role_keeps_earlier_valid_replacements_unpublished() {
        let directory = tempfile::tempdir().unwrap();
        let (dot, _) = identity(directory.path(), "dot");
        let (doh, _) = identity(directory.path(), "doh");
        let (doq, _) = identity(directory.path(), "doq");
        let (replacement, _) = identity(directory.path(), "replacement");
        let set = CertificateSet::prepare(CertificateSources {
            dot: Some(dot.clone()),
            doh: Some(doh),
            doq: Some(doq.clone()),
            doh_public_host: Some("localhost".into()),
        })
        .unwrap();
        let initial = set.summary();
        assert_ne!(initial.roles[0].leaf_sha256, initial.roles[2].leaf_sha256);
        replace(&replacement, &dot);
        std::fs::remove_file(&doq.key_file).unwrap();
        assert!(set.prepare_reload().is_err());
        assert_eq!(set.summary(), initial);
        replace(&replacement, &doq);
        assert!(set.publish(set.prepare_reload().unwrap()).unwrap());
        let changed = set.summary();
        assert_eq!(changed.certificate_generation, 2);
        assert_ne!(changed.roles[0].leaf_sha256, initial.roles[0].leaf_sha256);
        assert_eq!(changed.roles[1], initial.roles[1]);
        assert_eq!(changed.roles[0].leaf_sha256, changed.roles[2].leaf_sha256);
    }

    #[test]
    fn validates_dates_for_every_role_and_san_only_for_doh() {
        let directory = tempfile::tempdir().unwrap();
        let (files, _) = identity(directory.path(), "active");
        let mut wrong_host = sources(&files);
        wrong_host.doh_public_host = Some("not-localhost.example".into());
        assert!(
            CertificateSet::prepare(wrong_host)
                .unwrap_err()
                .is::<super::super::ManagementNameMismatch>()
        );
        for (start, end) in [(1990, 2000), (9990, 9999)] {
            let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
            params.not_before = rcgen::date_time_ymd(start, 1, 1);
            params.not_after = rcgen::date_time_ymd(end, 1, 1);
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = params.self_signed(&key).unwrap();
            std::fs::write(&files.cert_file, cert.pem()).unwrap();
            std::fs::write(&files.key_file, key.serialize_pem()).unwrap();
            let only_dot = CertificateSources {
                dot: Some(files.clone()),
                ..CertificateSources::default()
            };
            assert!(
                CertificateSet::prepare(only_dot)
                    .unwrap_err()
                    .to_string()
                    .contains("not currently valid")
            );
            assert!(super::super::load_identity(&files).is_err());
            assert!(
                validate_pem_identity(cert.pem().as_bytes(), key.serialize_pem().as_bytes())
                    .is_err()
            );
        }
    }

    #[test]
    fn inbound_identity_requires_server_auth_when_leaf_has_eku() {
        let directory = tempfile::tempdir().unwrap();
        for (name, usages) in [
            (
                "client-only",
                vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth],
            ),
            ("any-only", vec![rcgen::ExtendedKeyUsagePurpose::Any]),
        ] {
            let (files, _) = identity_with_eku(directory.path(), name, usages);
            let error = CertificateSet::prepare(sources(&files)).unwrap_err();
            assert!(
                error.to_string().contains("server authentication"),
                "{error:#}"
            );
            assert!(super::super::load_identity(&files).is_err());
        }
        let (server_auth, server_der) = identity_with_eku(
            directory.path(),
            "server-auth",
            vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth],
        );
        let server_set = CertificateSet::prepare(sources(&server_auth)).unwrap();
        assert_eq!(
            handshake(
                server_set
                    .server_config(CertificateRole::Dot, &[b"dot"])
                    .unwrap(),
                std::slice::from_ref(&server_der),
                b"dot"
            ),
            server_der
        );
        let (without_eku, no_eku_der) = identity(directory.path(), "without-eku");
        let no_eku_set = CertificateSet::prepare(sources(&without_eku)).unwrap();
        assert_eq!(
            handshake(
                no_eku_set
                    .server_config(CertificateRole::Doh, &[b"h2"])
                    .unwrap(),
                std::slice::from_ref(&no_eku_der),
                b"h2"
            ),
            no_eku_der
        );
    }

    #[test]
    fn empty_set_is_unchanged_and_has_no_resolver() {
        let set = CertificateSet::prepare(CertificateSources::default()).unwrap();
        assert_eq!(
            set.summary(),
            CertificateSummary {
                certificate_generation: 0,
                roles: vec![]
            }
        );
        assert!(!set.publish(set.prepare_reload().unwrap()).unwrap());
        assert!(set.server_config(CertificateRole::Doh, &[b"h2"]).is_err());
    }

    #[test]
    fn bounded_regular_file_reads_reject_missing_directories_and_oversize() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_pem_file(directory.path()).is_err());
        assert!(read_pem_file(&directory.path().join("missing")).is_err());
        let path = directory.path().join("large.pem");
        std::fs::write(&path, vec![b'x'; MAX_PEM_BYTES + 1]).unwrap();
        assert!(read_pem_file(&path).is_err());
        #[cfg(unix)]
        {
            let fifo = directory.path().join("fifo.pem");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(read_pem_file(&fifo).is_err());
            assert!(read_pem_file(Path::new("/dev/null")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn allows_regular_symlink_targets_used_by_external_renewers() {
        let directory = tempfile::tempdir().unwrap();
        let (files, _) = identity(directory.path(), "active");
        let path = directory.path().join("renewed.pem");
        std::os::unix::fs::symlink(&files.cert_file, &path).unwrap();
        assert_eq!(
            read_pem_file(&path).unwrap(),
            read_pem_file(&files.cert_file).unwrap()
        );
    }
}
