//! Bounded, private certificate imports. Configuration stores paths, never PEM.
//! The manager's mutation semaphore serializes import and configuration writes.

use std::{
    fs,
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
use serde::Serialize;

use super::store::{read_bounded, secret, sync_committed};
use crate::private_files::{create_private, private_permissions};

const MAX_PEM: usize = 64 * 1024;
const MAX_IMPORTS: usize = 32;
const MAX_IDENTITY: usize = 2 * MAX_PEM + 1024;

#[derive(Serialize)]
pub(super) struct ImportedCertificate {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

/// `state_dir` is the resolved, private directory owned by the locked Store.
/// Call only while holding the manager mutation permit. Newly created paths
/// become observable to configuration only after the complete file is synced.
pub(super) fn import(
    state_dir: &Path,
    certificate_pem: &str,
    private_key_pem: &str,
) -> Result<ImportedCertificate> {
    ensure!(
        !certificate_pem.is_empty() && certificate_pem.len() <= MAX_PEM,
        "certificate PEM must contain 1 to 65536 bytes"
    );
    ensure!(
        !private_key_pem.is_empty() && private_key_pem.len() <= MAX_PEM,
        "private key PEM must contain 1 to 65536 bytes"
    );
    let identity =
        crate::tls::validate_pem_identity(certificate_pem.as_bytes(), private_key_pem.as_bytes())
            .context("certificate and private key must be valid and match")?;
    // Re-encode only parsed certificates and the validated key. In particular,
    // a hidden key in the certificate input cannot become the stored identity.
    let mut combined = String::new();
    for cert in &identity.cert {
        append_pem(&mut combined, "CERTIFICATE", cert.as_ref());
    }
    let key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())?;
    let label = match &key {
        PrivateKeyDer::Pkcs1(_) => "RSA PRIVATE KEY",
        PrivateKeyDer::Sec1(_) => "EC PRIVATE KEY",
        PrivateKeyDer::Pkcs8(_) => "PRIVATE KEY",
        _ => anyhow::bail!("unsupported private key format"),
    };
    append_pem(&mut combined, label, key.secret_der());
    ensure!(
        combined.len() <= MAX_IDENTITY,
        "encoded identity is too large"
    );

    let directory = state_dir.join("certificates");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("create private certificate directory"),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir(),
        "certificate directory must not be a symlink"
    );
    private_permissions(&metadata, 0o700)?;

    let mut count = 0;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        count += 1;
        ensure!(
            count <= MAX_IMPORTS,
            "certificate import storage limit exceeded"
        );
        // Only a file of this length can already hold this identity; others
        // are not read, so they cannot block an unrelated import.
        if entry.metadata()?.len() != combined.len() as u64 {
            continue;
        }
        let existing = read_bounded(&entry.path(), MAX_IDENTITY).with_context(|| {
            format!(
                "check imported identity {}",
                entry.file_name().to_string_lossy()
            )
        })?;
        if existing.as_deref() == Some(combined.as_bytes()) {
            return Ok(imported(entry.path()));
        }
    }
    ensure!(
        count < MAX_IMPORTS,
        "maximum 32 imported identities reached; remove unused files from the private certificate directory before importing another identity"
    );

    let path = directory.join(format!("{}.pem", secret()));
    let mut file = create_private(&path).context("create private imported identity")?;
    // Cleanup is limited to the exact file that this call created exclusively.
    if let Err(error) = file
        .write_all(combined.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error).context("persist imported identity");
    }
    sync_committed(&directory, "certificate import");
    Ok(imported(path))
}

fn imported(path: PathBuf) -> ImportedCertificate {
    ImportedCertificate {
        cert_file: path.clone(),
        key_file: path,
    }
}

fn append_pem(target: &mut String, label: &str, der: &[u8]) {
    target.push_str(&format!("-----BEGIN {label}-----\n"));
    let encoded = STANDARD.encode(der);
    for line in encoded.as_bytes().chunks(64) {
        // Base64 is ASCII by construction.
        target.push_str(std::str::from_utf8(line).expect("base64 ASCII"));
        target.push('\n');
    }
    target.push_str(&format!("-----END {label}-----\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        manage::store::Store,
        tls::{self, TlsFiles},
    };
    use rustls::pki_types::CertificateDer;

    fn pair() -> (String, String) {
        pair_with_eku(vec![])
    }

    fn pair_with_eku(usages: Vec<rcgen::ExtendedKeyUsagePurpose>) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages = usages;
        let key = rcgen::KeyPair::generate().unwrap();
        (params.self_signed(&key).unwrap().pem(), key.serialize_pem())
    }

    #[test]
    fn persists_reuses_and_loads_combined_identity_without_secret_response() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair();
        let result = import(&store.dir, &cert, &key).unwrap();
        assert_eq!(result.cert_file, result.key_file);
        tls::server_config(
            &TlsFiles {
                cert_file: result.cert_file.clone(),
                key_file: result.key_file.clone(),
            },
            &[b"dot"],
        )
        .unwrap();
        let again = import(&store.dir, &format!("\n{cert}"), &key).unwrap();
        assert_eq!(result.cert_file, again.cert_file);
        assert_eq!(
            fs::read_dir(store.dir.join("certificates"))
                .unwrap()
                .count(),
            1
        );
        let response = serde_json::to_string(&result).unwrap();
        assert!(!response.contains("PRIVATE KEY"));
        assert!(!response.contains(&key));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&result.key_file).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.dir.join("certificates"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
    }

    #[test]
    fn rejects_bad_mismatched_multiple_and_oversized_input_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair();
        let (_, other_key) = pair();
        for (cert, key) in [
            (cert.clone(), other_key),
            ("bad PEM".into(), key.clone()),
            (cert.clone(), format!("{key}{key}")),
            (cert.clone(), "x".repeat(MAX_PEM + 1)),
            ("x".repeat(MAX_PEM + 1), key),
        ] {
            assert!(import(&store.dir, &cert, &key).is_err());
        }
        assert!(!store.dir.join("certificates").exists());
    }

    #[test]
    fn client_auth_only_leaf_is_rejected_before_private_import_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair_with_eku(vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth]);
        let error = match import(&store.dir, &cert, &key) {
            Ok(_) => panic!("client-auth-only import was accepted"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("server authentication"),
            "{error:#}"
        );
        assert!(!store.dir.join("certificates").exists());
    }

    #[test]
    fn normalizes_chain_and_ignores_key_hidden_in_certificate_input() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair();
        let (other_cert, other_key) = pair();
        let result = import(&store.dir, &format!("{other_key}{cert}{other_cert}"), &key).unwrap();
        let bytes = fs::read(&result.cert_file).unwrap();
        assert_eq!(CertificateDer::pem_slice_iter(&bytes).count(), 2);
        assert_eq!(PrivateKeyDer::pem_slice_iter(&bytes).count(), 1);
        tls::server_config(
            &TlsFiles {
                cert_file: result.cert_file,
                key_file: result.key_file,
            },
            &[b"h2"],
        )
        .unwrap();
    }

    #[test]
    fn bounds_import_count_and_reuses_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let first = pair();
        let original = import(&store.dir, &first.0, &first.1).unwrap();
        for _ in 1..MAX_IMPORTS {
            let (cert, key) = pair();
            import(&store.dir, &cert, &key).unwrap();
        }
        let next = pair();
        assert!(import(&store.dir, &next.0, &next.1).is_err());
        assert_eq!(
            import(&store.dir, &first.0, &first.1).unwrap().cert_file,
            original.cert_file
        );
        assert_eq!(
            fs::read_dir(store.dir.join("certificates"))
                .unwrap()
                .count(),
            MAX_IMPORTS
        );
    }

    #[test]
    fn only_a_file_that_can_hold_the_identity_is_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair();
        let existing = import(&store.dir, &cert, &key).unwrap();
        let other = store.dir.join("certificates/other.pem");
        fs::write(&other, "unrelated").unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o644)).unwrap();
        let (next_cert, next_key) = pair();
        import(&store.dir, &next_cert, &next_key).unwrap();
        // A non-compliant file that could hold the identity is named.
        fs::set_permissions(&existing.key_file, fs::Permissions::from_mode(0o644)).unwrap();
        let error = format!("{:#}", import(&store.dir, &cert, &key).err().unwrap());
        let name = existing.key_file.file_name().unwrap().to_string_lossy();
        assert!(error.contains(&*name), "{error}");
    }

    #[test]
    fn rejects_linked_or_non_private_storage() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let (cert, key) = pair();
        let result = import(&store.dir, &cert, &key).unwrap();
        fs::set_permissions(&result.key_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(import(&store.dir, &cert, &key).is_err());
        fs::set_permissions(&result.key_file, fs::Permissions::from_mode(0o600)).unwrap();
        let actual = store.dir.join("actual-certificates");
        fs::rename(store.dir.join("certificates"), &actual).unwrap();
        symlink(&actual, store.dir.join("certificates")).unwrap();
        assert!(import(&store.dir, &cert, &key).is_err());
    }
}
