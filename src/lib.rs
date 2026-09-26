//! DNS forwarding components for PariNS.

pub mod admin;
pub mod cache;
pub mod cache_persistence;
pub mod config;
pub mod doh;
pub mod ecs;
mod flight;
// Foundation only: no Config/Manager/Resolver entry point until index acceptance.
#[allow(dead_code)]
mod filter_subscriptions;
mod https_reader;
pub mod ingress;
pub mod limits;
pub mod manage;
pub mod metrics;
pub mod policy;
mod private_files;
pub mod protocol;
pub mod query_log;
pub mod quic;
pub mod resolver;
pub mod runtime_health;
pub mod runtime_lifecycle;
pub mod runtime_services;
pub mod server;
pub mod storage;
pub mod tls;
mod transport;
pub mod update;
mod upstream;
pub mod upstreams;
