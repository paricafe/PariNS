//! Read-only management observation, independent of the configuration mutex.
use super::Active;
use crate::{
    cache::persistence::SnapshotReport,
    resolver::Resolver,
    runtime_health::State as HealthState,
    runtime_services::RuntimeServices,
    server::{ListenerKind, Listeners},
};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Instant,
};

#[derive(Clone)]
pub(super) struct Running {
    pub resolver: Weak<Resolver>,
    pub listen: SocketAddr,
    pub doh_listen: Option<SocketAddr>,
    pub listeners: Arc<Listeners>,
    pub started: Instant,
}

#[derive(Clone, Default)]
pub(super) struct Metadata {
    pub phase: Phase,
    pub revision: u64,
    pub generation: u64,
    pub last_error: Option<String>,
    pub rollback_error: Option<String>,
    pub cache_report: SnapshotReport,
    pub last_reload: Option<Value>,
    pub running: Option<Running>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    #[default]
    Stable,
    Starting,
    Stopping,
}

pub(super) struct Observation {
    pub services: Arc<RuntimeServices>,
    active: Arc<Mutex<Active>>,
    metadata: RwLock<Metadata>,
}

impl Observation {
    pub fn new(services: Arc<RuntimeServices>, active: Arc<Mutex<Active>>) -> Arc<Self> {
        Arc::new(Self {
            services,
            active,
            metadata: RwLock::new(Metadata::default()),
        })
    }

    pub fn publish(&self, metadata: Metadata) {
        *self.metadata.write().expect("management observation") = metadata;
    }

    pub fn status(&self) -> Value {
        let metadata = self
            .metadata
            .read()
            .expect("management observation")
            .clone();
        self.render(&metadata)
    }

    pub fn render(&self, metadata: &Metadata) -> Value {
        let mut health = self.services.health.snapshot();
        let phase = if metadata.phase == Phase::Stable && health.generation != metadata.generation {
            Phase::Starting
        } else {
            metadata.phase
        };
        let resolver = metadata.running.as_ref().and_then(|r| r.resolver.upgrade());
        let running = metadata.running.as_ref().filter(|_| {
            resolver.is_some()
                && health.generation == metadata.generation
                && (health.ready || health.state == HealthState::Degraded)
        });
        let resolver = resolver.filter(|_| running.is_some());
        let cache = resolver.as_ref().map(|r| r.cache());
        health.ready &= phase == Phase::Stable;
        let transport = self.active.lock().unwrap().snapshot.clone();
        let mut certificates = transport
            .certificates
            .as_ref()
            .map(|set| json!(set.summary()))
            .unwrap_or_else(|| json!({"certificate_generation":0,"roles":[]}));
        certificates["last_reload"] = json!(metadata.last_reload);
        json!({
            "running": running.is_some(),
            "lifecycle_phase": phase,
            "last_error": if phase != Phase::Stable { None }
            else if metadata.running.is_some() && running.is_none() {
                Some("DNS task exited; restarting automatically".to_string())
            } else { metadata.last_error.clone() },
            "revision": metadata.revision,
            "rollback_unavailable": metadata.rollback_error,
            "resolution_mode": resolver.as_ref().map(|r| r.resolution_mode()),
            "metrics": self.services.metrics.snapshot(), "metrics_scope":"process",
            "dns_health": health,
            "diagnostics": {"cache":cache.as_ref().map(|c| c.diagnostics_snapshot()),"quic":self.services.metrics.quic.snapshot(),"upstreams":resolver.as_ref().and_then(|r| r.upstream_diagnostics()),"recursion":resolver.as_ref().and_then(|r| r.recursion_diagnostics())},
            "certificates": certificates,
            "storage": self.services.storage.status(),
            "cache_persistence": metadata.cache_report,
            "listen": running.filter(|r| r.listeners.serving(ListenerKind::Dns)).map(|r| r.listen.to_string()),
            "listeners": running.map(|r| r.listeners.snapshot()),
            "doh_listen": running.filter(|r| r.listeners.serving(ListenerKind::Doh)).and_then(|r| r.doh_listen).map(|a| a.to_string()),
            "doh_http3": running.is_some_and(|r| r.listeners.serving(ListenerKind::Doh3)),
            "uptime_seconds": running.map(|r| r.started.elapsed().as_secs()),
            "generation": metadata.generation,
            "cache": cache.as_ref().map(|c| c.snapshot()),
            "refresh": resolver.as_ref().map(|r| r.refresh_snapshot()),
            "transport": transport.view(),
        })
    }
}
