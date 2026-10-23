//! Candidate preflight is read-only: no Store::open, runtime, database, lock,
//! cache consumption, listener, or background task is constructed here.
use super::{store, transport::Snapshot};
use crate::{config::Config, tls::CertificateRole};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, net::SocketAddr, path::Path};

pub use crate::update::ipc::{CheckInputs, CheckReport};

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
    crate::private_files::private_permissions(&metadata, 0o700)?;
    let saved = store::read_stored(&dir)?.context("management is not initialized")?;
    // Startup reads the Turnstile keys as well; reject a file this version cannot.
    super::turnstile::read(&dir)?;
    let config = Config::parse_in(&saved.toml, &dir)?;
    // Startup reads the rollback copy too; a copy this version rejects would
    // keep the candidate's management from opening.
    super::runtime::previous_fingerprints(&saved, &dir)?;
    let snapshot = Snapshot::prepare(&config, address)?;
    config.check_non_identity_files()?;
    // A candidate that completes these steps accepts this configuration; the
    // report then only shows that both versions read the same inputs.
    crate::filter_subscriptions::service::read_only(
        &dir.join("filter-subscriptions"),
        &config.filter_subscriptions,
        &config.load_policy()?,
    )?;
    let digest = |parts: &[&[u8]]| {
        let mut digest = Sha256::new();
        for part in parts {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        format!("{:x}", digest.finalize())
    };
    let rule_files = match &config.filter_file {
        Some(path) => bounded(path, 8 * 1024 * 1024, "filter file")?,
        None => Vec::new(),
    };
    let (sources, content_revision) = crate::filter_subscriptions::service::selected_content(
        &dir.join("filter-subscriptions"),
        &config.filter_subscriptions,
    )?;
    let revision = content_revision.to_be_bytes();
    let subscriptions: Vec<&[u8]> = std::iter::once(&revision[..])
        .chain(
            sources
                .iter()
                .flat_map(|(id, sha256)| [id.as_bytes(), sha256.as_bytes()]),
        )
        .collect();
    let mut certificates: Vec<Vec<u8>> = Vec::new();
    if let Some(set) = &snapshot.certificates {
        for role in [
            CertificateRole::Dot,
            CertificateRole::Doh,
            CertificateRole::Doq,
        ] {
            if let Some(key) = set.key(role) {
                certificates.extend(key.cert.iter().map(|cert| cert.as_ref().to_vec()));
            }
        }
    }
    let certificates: Vec<&[u8]> = certificates.iter().map(Vec::as_slice).collect();
    // External regular symlinks are allowed as with the existing TLS loader,
    // but not devices or unbounded IO.
    let upstream_ca = match &config.upstreams.ca_file {
        Some(path) => bounded(path, 1024 * 1024, "upstream CA")?,
        None => Vec::new(),
    };
    Ok(CheckReport {
        config_revision: saved.revision,
        inputs: CheckInputs {
            configuration: digest(&[saved.toml.as_bytes()]),
            rule_files: digest(&[&rule_files]),
            subscriptions: digest(&subscriptions),
            certificates: digest(&certificates),
            upstream_ca: digest(&[&upstream_ca]),
        },
    })
}

fn bounded(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {what}"))?;
    ensure!(file.metadata()?.is_file(), "{what} must be a regular file");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "{what} exceeds {limit} bytes");
    Ok(bytes)
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
        assert_eq!(report.inputs.configuration.len(), 64);
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
    fn invalid_rollback_copy_fails_preflight() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed");
        let store = Store::open(&path).unwrap();
        let mut stored = Stored {
            username: "admin".into(),
            password_hash: store::hash_password("test-password").unwrap(),
            toml: include_str!("../../parins.example.toml").into(),
            previous: Some("listen = 'not an address'\n".into()),
            revision: 2,
        };
        store.save(&stored).unwrap();
        let error = managed(&path, "127.0.0.1:3000".parse().unwrap()).unwrap_err();
        assert!(
            format!("{error:#}").contains("rollback configuration"),
            "{error:#}"
        );
        stored.previous = Some(include_str!("../../parins.example.toml").into());
        store.save(&stored).unwrap();
        assert_eq!(
            managed(&path, "127.0.0.1:3000".parse().unwrap())
                .unwrap()
                .config_revision,
            2
        );
    }

    /// Each input category changes only its own digest; the same inputs give the
    /// same report however this version compiles them.
    #[test]
    fn report_digests_each_version_independent_input_on_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed");
        let store = Store::open(&path).unwrap();
        let rules = temp.path().join("rules.toml");
        let ca = temp.path().join("ca.pem");
        let write_ca = |name: &str| {
            let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
            std::fs::write(&ca, cert.cert.pem()).unwrap();
        };
        std::fs::write(&rules, "enabled = true\nblock_exact = ['one.test']\n").unwrap();
        write_ca("ca-one.test");
        // Top-level keys go first, before any table.
        let toml = |extra: &str| {
            format!(
                "filter_file = {:?}{extra}\n{}",
                rules.display().to_string(),
                include_str!("../../parins.example.toml").replace(
                    "# ca_file = \"private-ca.pem\"",
                    &format!("ca_file = {:?}", ca.display().to_string()),
                )
            )
        };
        let save = |toml: String, revision: u64| {
            store
                .save(&Stored {
                    username: "admin".into(),
                    password_hash: store::hash_password("test-password").unwrap(),
                    toml,
                    previous: None,
                    revision,
                })
                .unwrap();
            managed(&path, "127.0.0.1:3000".parse().unwrap()).unwrap()
        };
        let base = save(toml(""), 1);
        assert_eq!(save(toml(""), 1), base);
        let mut changed = Vec::new();
        changed.push(save(toml(""), 2).differences(&base));
        changed.push(save(toml("\nadmin_listen = '127.0.0.1:9153'"), 1).differences(&base));
        std::fs::write(
            &rules,
            "enabled = true\nblock_exact = ['two.test nxdomain']\n",
        )
        .unwrap();
        changed.push(save(toml(""), 1).differences(&base));
        std::fs::write(&rules, "enabled = true\nblock_exact = ['one.test']\n").unwrap();
        write_ca("ca-two.test");
        changed.push(save(toml(""), 1).differences(&base));
        assert_eq!(
            changed,
            [
                vec!["revision"],
                vec!["configuration"],
                vec!["rule_files"],
                vec!["upstream_ca"]
            ]
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
