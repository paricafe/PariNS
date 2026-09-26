//! Candidate preflight is read-only: no Store::open, runtime, database, lock,
//! cache consumption, listener, or background task is constructed here.
use super::{store, transport::Snapshot};
use crate::{config::Config, tls::CertificateRole};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, net::SocketAddr, path::Path};

pub use crate::update::ipc::CheckReport;

pub fn managed(path: &Path, address: SocketAddr) -> Result<CheckReport> {
    let dir = path
        .canonicalize()
        .context("resolve existing management directory")?;
    ensure!(
        dir.parent().is_some(),
        "use a dedicated management directory"
    );
    let metadata = std::fs::metadata(&dir)?;
    ensure!(metadata.is_dir(), "management path must be a directory");
    store::private_permissions(&metadata, 0o700)?;
    let saved = store::read_stored(&dir)?.context("management is not initialized")?;
    let config = Config::parse_in(&saved.toml, &dir)?;
    let snapshot = Snapshot::prepare(&config, address)?;
    config.check_non_identity_files()?;
    let policy = config.load_policy()?;
    let mut digest = Sha256::new();
    let mut add = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    };
    add(&saved.revision.to_be_bytes());
    add(saved.toml.as_bytes());
    add(&policy.semantic_digest());
    if let Some(certificates) = snapshot.certificates {
        for role in [
            CertificateRole::Dot,
            CertificateRole::Doh,
            CertificateRole::Doq,
        ] {
            if let Some(key) = certificates.key(role) {
                for cert in &key.cert {
                    add(cert.as_ref());
                }
            }
        }
    }
    // CA changes also invalidate the preflight. External regular symlinks are
    // allowed as with the existing TLS loader, but not devices or unbounded IO.
    if let Some(path) = &config.upstreams.ca_file {
        let file = File::open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "upstream CA must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "upstream CA exceeds 1 MiB");
        add(&bytes);
    }
    Ok(CheckReport {
        config_revision: saved.revision,
        material_digest: format!("{:x}", digest.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::store::{Store, Stored};

    #[test]
    fn preflight_ignores_live_lock_and_never_touches_runtime_or_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed");
        let store = Store::open(&path).unwrap();
        store
            .save(&Stored {
                username: "admin".into(),
                password_hash: store::hash_password("test-password").unwrap(),
                toml: include_str!("../../parins.example.toml").into(),
                previous: None,
                revision: 1,
            })
            .unwrap();
        let runtime = path.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let db = runtime.join("observability.sqlite3");
        let cache = runtime.join("dns-cache-clean.snapshot");
        std::fs::write(&db, b"must not open or repair SQLite").unwrap();
        std::fs::write(&cache, b"must not consume clean cache").unwrap();
        let before = std::fs::read(path.join("state.json")).unwrap();
        let members = |dir: &Path| {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before_members = members(&path);
        let before_runtime = members(&runtime);
        let report = managed(&path, "127.0.0.1:3000".parse().unwrap()).unwrap();
        assert_eq!(report.config_revision, 1);
        assert_eq!(report.material_digest.len(), 64);
        assert_eq!(std::fs::read(path.join("state.json")).unwrap(), before);
        assert_eq!(members(&path), before_members);
        assert_eq!(members(&runtime), before_runtime);
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"must not open or repair SQLite"
        );
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            b"must not consume clean cache"
        );
    }

    #[test]
    fn missing_and_invalid_state_fail_without_initializing_anything() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("absent");
        assert!(managed(&path, "127.0.0.1:3000".parse().unwrap()).is_err());
        assert!(!path.exists());
        let store = Store::open(&path).unwrap();
        assert!(managed(&path, "127.0.0.1:3000".parse().unwrap()).is_err());
        assert!(!path.join("setup-token").exists());
        assert!(!path.join("runtime").exists());
        drop(store);
    }
}
