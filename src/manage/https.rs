//! The managed HTTPS identity survives restarts independently of DNS settings.
//! One private file is authoritative; the certificate-only export is derived.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, ensure};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

use super::store::{Store, read_bounded};
use crate::tls::{self, TlsFiles};

const IDENTITY: &str = "https-identity.pem";
const CERTIFICATE: &str = "https-cert.pem";
const MAX_IDENTITY: usize = 64 * 1024;

pub(super) fn config(
    store: &Store,
    custom: Option<&TlsFiles>,
    address: SocketAddr,
) -> Result<Arc<rustls::ServerConfig>> {
    if let Some(files) = custom {
        return tls::server_config(files, &[b"http/1.1"]);
    }

    let path = store.dir.join(IDENTITY);
    let identity = match read_bounded(&path, MAX_IDENTITY)? {
        Some(bytes) => bytes,
        None => {
            let bytes = generate(address)?.into_bytes();
            // The store lock serializes creation. A combined PEM gives the key
            // and certificate one atomic commit point, even across power loss.
            store.atomic_write(IDENTITY, &bytes)?;
            bytes
        }
    };
    let files = TlsFiles {
        cert_file: path.clone(),
        key_file: path,
    };
    // Includes an explicit key/certificate consistency check. Never silently
    // replace an existing invalid identity and change the operator's trust pin.
    let config = tls::server_config(&files, &[b"http/1.1"])
        .context("invalid managed HTTPS identity; restore it or explicitly replace it")?;
    let certificate = certificate_pem(&identity)?;
    let exported = read_bounded(&store.dir.join(CERTIFICATE), MAX_IDENTITY)?;
    if exported.as_deref() != Some(certificate) {
        store.atomic_write(CERTIFICATE, certificate)?;
    }
    Ok(config)
}

fn generate(address: SocketAddr) -> Result<String> {
    let mut names = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    let explicit_ip = address.ip().to_string();
    if !address.ip().is_unspecified() && !names.contains(&explicit_ip) {
        names.push(explicit_ip);
    }
    let mut params = CertificateParams::new(names)?;
    let now = SystemTime::now();
    params.not_before = (now - Duration::from_secs(300)).into();
    params.not_after = (now + Duration::from_secs(10 * 365 * 24 * 60 * 60)).into();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "PariNS management");
    let key = KeyPair::generate()?;
    let certificate = params.self_signed(&key)?;
    Ok(format!("{}{}", certificate.pem(), key.serialize_pem()))
}

fn certificate_pem(identity: &[u8]) -> Result<&[u8]> {
    let text = std::str::from_utf8(identity).context("HTTPS identity must be PEM")?;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let start = text
        .find(BEGIN)
        .context("HTTPS identity has no certificate")?;
    let end = start
        + text[start..]
            .find(END)
            .context("HTTPS identity has an incomplete certificate")?
        + END.len();
    ensure!(
        !text[end..].contains(BEGIN),
        "managed self-signed identity must contain one certificate"
    );
    Ok(&identity[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn listen() -> SocketAddr {
        "0.0.0.0:3000".parse().unwrap()
    }

    #[test]
    fn generates_reuses_and_repairs_certificate_only_export() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let tls = config(&store, None, listen()).unwrap();
        assert_eq!(tls.alpn_protocols, [b"http/1.1"]);
        let identity = fs::read(store.dir.join(IDENTITY)).unwrap();
        let certificate = fs::read(store.dir.join(CERTIFICATE)).unwrap();
        assert!(String::from_utf8_lossy(&identity).contains("BEGIN PRIVATE KEY"));
        assert!(!String::from_utf8_lossy(&certificate).contains("PRIVATE KEY"));
        assert_eq!(certificate, certificate_pem(&identity).unwrap());
        config(&store, None, listen()).unwrap();
        assert_eq!(fs::read(store.dir.join(IDENTITY)).unwrap(), identity);
        fs::remove_file(store.dir.join(CERTIFICATE)).unwrap();
        config(&store, None, listen()).unwrap();
        assert_eq!(fs::read(store.dir.join(CERTIFICATE)).unwrap(), certificate);
        drop(store);
        let store = Store::open(&dir.path().join("state")).unwrap();
        config(&store, None, listen()).unwrap();
        assert_eq!(fs::read(store.dir.join(IDENTITY)).unwrap(), identity);
    }

    #[test]
    fn rejects_corruption_and_mismatched_keys_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        store.atomic_write(IDENTITY, b"corrupt").unwrap();
        assert!(config(&store, None, listen()).is_err());
        assert_eq!(fs::read(store.dir.join(IDENTITY)).unwrap(), b"corrupt");
        let generated = generate(listen()).unwrap();
        let certificate = certificate_pem(generated.as_bytes()).unwrap();
        let other_key = KeyPair::generate().unwrap().serialize_pem();
        let mismatch = format!("{}\n{other_key}", std::str::from_utf8(certificate).unwrap());
        store.atomic_write(IDENTITY, mismatch.as_bytes()).unwrap();
        assert!(config(&store, None, listen()).is_err());
        assert_eq!(
            fs::read(store.dir.join(IDENTITY)).unwrap(),
            mismatch.as_bytes()
        );
        assert!(!store.dir.join(CERTIFICATE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn identity_is_private_and_insecure_or_linked_files_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        config(&store, None, listen()).unwrap();
        let identity = store.dir.join(IDENTITY);
        assert_eq!(
            fs::metadata(&identity).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config(&store, None, listen()).is_err());
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
        let actual = store.dir.join("actual.pem");
        fs::rename(&identity, &actual).unwrap();
        symlink(&actual, &identity).unwrap();
        assert!(config(&store, None, listen()).is_err());
    }

    #[test]
    fn custom_certificate_does_not_generate_managed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).unwrap();
        let custom = store.dir.join("custom.pem");
        store
            .atomic_write("custom.pem", generate(listen()).unwrap().as_bytes())
            .unwrap();
        let files = TlsFiles {
            cert_file: custom.clone(),
            key_file: custom,
        };
        config(&store, Some(&files), listen()).unwrap();
        assert!(!store.dir.join(IDENTITY).exists());
        store.atomic_write("custom.pem", b"broken").unwrap();
        assert!(config(&store, Some(&files), listen()).is_err());
        assert!(!store.dir.join(IDENTITY).exists());
    }
}
