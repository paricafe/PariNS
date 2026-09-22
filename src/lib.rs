//! DNS forwarding components for PariNS.

pub mod cache;
pub mod config;
pub mod ecs;
mod flight;
pub mod metrics;
pub mod policy;
pub mod protocol;
pub mod resolver;
pub mod server;
mod transport;
mod upstream;
