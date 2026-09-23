//! Management transport is an immutable per-connection view. Manager prepares a
//! candidate before persistence and publishes it only after the store commit.
use std::{fmt, net::SocketAddr, sync::Arc};

use anyhow::{Result, ensure};
use rustls::{ServerConfig, sign::CertifiedKey};
use serde_json::{Value, json};

use crate::{
    config::{Config, public_host},
    tls,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

#[derive(Clone)]
pub(super) struct Snapshot {
    pub scheme: Scheme,
    pub origin: Option<String>,
    pub public_host: Option<String>,
    pub source: Option<&'static str>,
    pub tls: Option<Arc<ServerConfig>>,
    pub selected: Option<(&'static str, Arc<CertifiedKey>)>,
    pub realm: u64,
}

impl Snapshot {
    pub fn initial() -> Self {
        Self {
            scheme: Scheme::Http,
            origin: None,
            public_host: None,
            source: None,
            tls: None,
            selected: None,
            realm: 0,
        }
    }

    pub fn describe(config: &Config, address: SocketAddr) -> Result<Self> {
        let (source, _) = selected_files(config);
        let scheme = if source.is_some() {
            Scheme::Https
        } else {
            Scheme::Http
        };
        let public_host = config
            .web
            .as_ref()
            .map(|web| public_host(&web.public_host))
            .transpose()?;
        ensure!(
            scheme == Scheme::Http || public_host.is_some(),
            "web.public_host is required when DoH or DoH3 is enabled in managed mode"
        );
        let origin = public_host
            .as_deref()
            .map(|host| origin(scheme, host, address.port()));
        Ok(Self {
            scheme,
            origin,
            public_host,
            source,
            tls: None,
            selected: None,
            realm: 0,
        })
    }

    pub fn prepare(config: &Config, address: SocketAddr) -> Result<Self> {
        let mut candidate = Self::describe(config, address)?;
        if let (Some(source), Some(files)) = selected_files(config) {
            let key = tls::load_identity(files).map_err(CertificateInvalid)?;
            tls::validate_management_identity(
                &key,
                candidate
                    .public_host
                    .as_deref()
                    .expect("HTTPS requires public_host"),
            )
            .map_err(|error| {
                if error.is::<tls::ManagementNameMismatch>() {
                    error
                } else {
                    CertificateInvalid(error).into()
                }
            })?;
            candidate.tls = Some(tls::server_config_with_key(key.clone(), &[b"http/1.1"])?);
            candidate.selected = Some((source, key));
        }
        Ok(candidate)
    }

    pub fn view(&self) -> Value {
        json!({"scheme":self.scheme.as_str(),"origin":self.origin,
            "certificate_source":self.source})
    }

    pub fn change(&self, next: &Self, request_host: &str, address: SocketAddr) -> Value {
        if self.scheme == next.scheme && self.public_host == next.public_host {
            return Value::Null;
        }
        let next_origin = next.origin.clone().or_else(|| {
            // An HTTP address seen on this request is usable only if the next
            // allowlist still admits it. Never derive a public NAT address.
            if next.scheme == Scheme::Http && super::host_allowed(address, request_host, next) {
                Some(format!("http://{request_host}"))
            } else {
                None
            }
        });
        json!({
            "from":self.scheme.as_str(), "to":next.scheme.as_str(),
            "next_origin":next_origin,
            "reauthenticate":true,
            "requires_http_confirmation":self.scheme == Scheme::Https && next.scheme == Scheme::Http,
        })
    }

    pub fn changes_realm(&self, next: &Self) -> bool {
        self.scheme != next.scheme || self.public_host != next.public_host
    }
}

#[derive(Debug)]
pub(super) struct CertificateInvalid(pub anyhow::Error);

impl fmt::Display for CertificateInvalid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid management certificate: {:#}", self.0)
    }
}

impl std::error::Error for CertificateInvalid {}

fn selected_files(config: &Config) -> (Option<&'static str>, Option<&tls::TlsFiles>) {
    if let Some(doh) = &config.doh {
        (Some("doh"), Some(&doh.files))
    } else if let Some(doh3) = &config.doh3 {
        (Some("doh3"), Some(&doh3.files))
    } else {
        (None, None)
    }
}

fn origin(scheme: Scheme, host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if port == scheme.default_port() {
        format!("{}://{host}", scheme.as_str())
    } else {
        format!("{}://{host}:{port}", scheme.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doh_is_preferred_and_doh3_only_is_a_management_identity() {
        let temp = tempfile::tempdir().unwrap();
        let doh = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let doh3 = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let doh_cert = temp.path().join("doh-cert.pem");
        let doh_key = temp.path().join("doh-key.pem");
        let doh3_cert = temp.path().join("doh3-cert.pem");
        let doh3_key = temp.path().join("doh3-key.pem");
        std::fs::write(&doh_cert, doh.cert.pem()).unwrap();
        std::fs::write(&doh_key, doh.signing_key.serialize_pem()).unwrap();
        std::fs::write(&doh3_cert, doh3.cert.pem()).unwrap();
        std::fs::write(&doh3_key, doh3.signing_key.serialize_pem()).unwrap();
        let example = include_str!("../../parins.example.toml");
        let listeners = format!(
            "\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n[doh3]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
            serde_json::json!(doh_cert),
            serde_json::json!(doh_key),
            serde_json::json!(doh3_cert),
            serde_json::json!(doh3_key)
        );
        let config = Config::parse_in(&format!("{example}{listeners}"), temp.path()).unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let chosen = Snapshot::prepare(&config, address).unwrap();
        assert_eq!(chosen.source, Some("doh"));
        assert_eq!(
            chosen.selected.as_ref().unwrap().1.cert[0].as_ref(),
            doh.cert.der().as_ref()
        );
        let only_doh3 = format!(
            "{example}\n[web]\npublic_host='dns.test'\n[doh3]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
            serde_json::json!(doh3_cert),
            serde_json::json!(doh3_key)
        );
        let config = Config::parse_in(&only_doh3, temp.path()).unwrap();
        let chosen = Snapshot::prepare(&config, address).unwrap();
        assert_eq!(chosen.source, Some("doh3"));
        assert_eq!(
            chosen.selected.as_ref().unwrap().1.cert[0].as_ref(),
            doh3.cert.der().as_ref()
        );
    }
}
