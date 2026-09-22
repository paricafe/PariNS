//! DNS forwarding components for PariNS.

pub mod admin;
pub mod cache;
pub mod config;
pub mod doh;
pub mod ecs;
mod flight;
pub mod ingress;
pub mod limits;
pub mod manage;
pub mod metrics;
pub mod policy;
pub mod protocol;
pub mod query_log;
pub mod quic;
pub mod resolver;
pub mod scheduler;
pub mod server;
pub mod tls;
mod transport;
mod upstream;
pub mod upstreams;
