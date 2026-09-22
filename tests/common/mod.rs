use std::{net::SocketAddr, time::Duration};

pub fn resolver(upstream: SocketAddr, timeout: Duration) -> parins::resolver::Resolver {
    let mut config =
        parins::config::Config::parse(include_str!("../../parins.example.toml")).unwrap();
    config.upstreams.servers = vec![upstream.to_string()];
    config.query_timeout_ms = timeout.as_millis().try_into().unwrap();
    parins::resolver::Resolver::from_config(&config)
}
