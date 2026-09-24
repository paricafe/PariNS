//! Management transport is an immutable per-connection view. Manager prepares a
//! candidate before persistence and publishes it only after the store commit.
use std::{fmt, net::SocketAddr, sync::Arc};

use anyhow::{Result, ensure};
use rustls::ServerConfig;
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
    pub certificates: Option<Arc<tls::CertificateSet>>,
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
            certificates: None,
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
            "web.public_host is required when DoH is enabled in managed mode"
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
            certificates: None,
            realm: 0,
        })
    }

    pub fn prepare(config: &Config, address: SocketAddr) -> Result<Self> {
        let mut candidate = Self::describe(config, address)?;
        let mut sources = config.certificate_sources();
        sources.doh_public_host = candidate.public_host.clone();
        let certificates = tls::CertificateSet::prepare(sources).map_err(|error| {
            if error.is::<tls::ManagementNameMismatch>() {
                error
            } else {
                CertificateInvalid(error).into()
            }
        })?;
        if candidate.source.is_some() {
            candidate.tls =
                Some(certificates.server_config(tls::CertificateRole::Doh, &[b"http/1.1"])?);
        }
        candidate.certificates = Some(certificates);
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
    fn http3_toggle_keeps_the_doh_management_identity_and_realm() {
        let temp = tempfile::tempdir().unwrap();
        let doh = rcgen::generate_simple_self_signed(vec!["dns.test".into()]).unwrap();
        let doh_cert = temp.path().join("doh-cert.pem");
        let doh_key = temp.path().join("doh-key.pem");
        std::fs::write(&doh_cert, doh.cert.pem()).unwrap();
        std::fs::write(&doh_key, doh.signing_key.serialize_pem()).unwrap();
        let example = include_str!("../../parins.example.toml");
        let listeners = format!(
            "\n[web]\npublic_host='dns.test'\n[doh]\nlisten='127.0.0.1:0'\ncert_file={}\nkey_file={}\n",
            serde_json::json!(doh_cert),
            serde_json::json!(doh_key)
        );
        let config = Config::parse_in(&format!("{example}{listeners}"), temp.path()).unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let chosen = Snapshot::prepare(&config, address).unwrap();
        assert_eq!(chosen.source, Some("doh"));
        assert_eq!(
            chosen
                .certificates
                .as_ref()
                .unwrap()
                .key(tls::CertificateRole::Doh)
                .unwrap()
                .cert[0]
                .as_ref(),
            doh.cert.der().as_ref()
        );
        let mut config = config;
        config.doh.as_mut().unwrap().http3 = true;
        let enabled = Snapshot::prepare(&config, address).unwrap();
        assert_eq!(enabled.source, Some("doh"));
        assert!(!chosen.changes_realm(&enabled));
        assert_eq!(
            chosen
                .certificates
                .unwrap()
                .key(tls::CertificateRole::Doh)
                .unwrap()
                .cert,
            enabled
                .certificates
                .unwrap()
                .key(tls::CertificateRole::Doh)
                .unwrap()
                .cert
        );
    }
}
