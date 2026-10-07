//! Keep immediate socket-release acceptance in its own test process.
//! Concurrent subprocess fixtures can briefly inherit CLOEXEC sockets before exec,
//! so do not add subprocess-spawning tests to this integration-test target.
use std::{sync::Arc, time::Duration};

use parins::{
    config::Config,
    server::Server,
    tls::{ListenerConfig, TlsFiles},
};
use rustls::{ClientConfig, RootCertStore};
use tokio::{net::UdpSocket, sync::oneshot, time::timeout};

const WAIT: Duration = Duration::from_secs(3);

#[tokio::test]
async fn quic_shutdown_deadline_releases_socket_with_an_incomplete_stream() {
    #[cfg(target_os = "linux")]
    fn udp_inode(address: std::net::SocketAddr) -> std::io::Result<Option<u64>> {
        let std::net::IpAddr::V4(ip) = address.ip() else {
            unreachable!("this fixture binds IPv4 loopback")
        };
        let local = format!(
            "{:08X}:{:04X}",
            u32::from_ne_bytes(ip.octets()),
            address.port()
        );
        for line in std::fs::read_to_string("/proc/net/udp")?.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(1) == Some(&local.as_str()) {
                return fields
                    .get(9)
                    .and_then(|value| value.parse().ok())
                    .map(Some)
                    .ok_or_else(|| std::io::Error::other("missing UDP inode in procfs row"));
            }
        }
        Ok(None)
    }

    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let files = TlsFiles {
        cert_file: directory.path().join("cert.pem"),
        key_file: directory.path().join("key.pem"),
    };
    std::fs::write(&files.cert_file, certificate.pem()).unwrap();
    std::fs::write(&files.key_file, key.serialize_pem()).unwrap();
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::parse(include_str!("../parins.example.toml")).unwrap();
    config.listen.set_port(0);
    config.upstreams.servers = vec![upstream.local_addr().unwrap().to_string()];
    config.query_timeout_ms = 1000;
    config.tcp_io_timeout_ms = 1000;
    config.doq = Some(ListenerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        files,
    });
    config.shutdown_grace_ms = 1;
    let server = Server::bind(config).await.unwrap();
    let address = server.encrypted_addrs().unwrap()[0].1;
    #[cfg(target_os = "linux")]
    let listener_inode = udp_inode(address);
    let metrics = server.metrics().clone();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let mut roots = RootCertStore::empty();
    roots.add(certificate.der().clone()).unwrap();
    let mut tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"doq".to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let connection = client.connect(address, "localhost").unwrap().await.unwrap();
    let (mut stream, _response) = connection.open_bi().await.unwrap();
    stream.write_all(&[0]).await.unwrap();
    timeout(WAIT, async {
        while metrics.quic.snapshot()["doq"]["stream_started"] == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    timeout(WAIT, task).await.unwrap().unwrap().unwrap();
    if let Err(error) = UdpSocket::bind(address).await {
        // Keep the immediate rebind assertion: a later successful bind would
        // hide incomplete cleanup. Inspect current owners only after it has failed
        // so CI can distinguish a retained listener from ephemeral-port reuse.
        #[cfg(target_os = "linux")]
        {
            eprintln!(
                "UDP inode for {address}: originally {listener_inode:?}, after rebind failure {:?}",
                udp_inode(address),
            );
            let owners = std::process::Command::new("ss")
                .args(["-H", "-aunp", &format!("sport = :{}", address.port())])
                .output();
            match owners {
                Ok(output) => eprintln!(
                    "UDP owners for {address} (ss status {}, bounded output):\n{}{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(8192)]),
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(2048)]),
                ),
                Err(diagnostic_error) => {
                    eprintln!("UDP owner diagnostic unavailable for {address}: {diagnostic_error}");
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let owners = std::process::Command::new("lsof")
                .args(["-nP", &format!("-iUDP:{}", address.port())])
                .output();
            match owners {
                Ok(output) => eprintln!(
                    "UDP owners for {address} (lsof status {}):\n{}{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ),
                Err(diagnostic_error) => {
                    eprintln!("UDP owner diagnostic unavailable for {address}: {diagnostic_error}");
                }
            }
        }
        panic!("immediate UDP rebind failed for {address}: {error:?}");
    }
    assert_eq!(metrics.quic.snapshot()["doq"]["stream_shutdown"], 1);
    client.close(0u32.into(), b"done");
}
