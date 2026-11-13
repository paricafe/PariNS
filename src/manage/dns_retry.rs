//! Process-owned retry of stopped or failed managed DNS generations.
use super::Shared;
use std::{sync::Arc, time::Duration};

pub(super) async fn run(shared: Arc<Shared>, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            () = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
        if *stop.borrow() {
            return;
        }
        let due = |manager: &mut super::runtime::Manager| {
            manager.reap_exited();
            manager
                .startup_retry_at()
                .is_some_and(|at| at <= std::time::Instant::now())
        };
        if !due(&mut *shared.manager.lock().await) {
            continue;
        }
        let Ok(_permit) = shared.mutation.clone().try_acquire_owned() else {
            continue;
        };
        let mut manager = shared.manager.lock().await;
        if due(&mut manager) {
            manager.retry_startup().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::{Active, Manager, Snapshot, Store, auth_budget, store};
    use tokio::{sync::Semaphore, task::JoinSet};

    #[tokio::test]
    async fn retries_a_failed_start_yields_to_mutations_ignores_freezes_and_stops_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let state = Store::open(&temp.path().join("state")).unwrap();
        state
            .save(&store::Stored {
                username: "admin".into(),
                password_hash: store::hash_password("local-test-password").unwrap(),
                toml: "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=100\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into(),
                previous: None,
                revision: 1,
            })
            .unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(std::sync::Mutex::new(Active {
            snapshot: Arc::new(Snapshot::initial()),
            sessions: Vec::new(),
        }));
        let mut manager = Manager::open(state, address, active.clone()).await.unwrap();
        manager.simulate_startup_failure().await;
        assert!(manager.resolver().is_none());
        manager
            .updates
            .frozen
            .store(true, std::sync::atomic::Ordering::Release);
        let shared = Arc::new(Shared {
            filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
            live: Default::default(),
            observation: manager.observation.clone(),
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            active,
            auth: auth_budget::Budget::new(),
            mutation: Arc::new(Semaphore::new(1)),
            address,
        });
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run(shared.clone(), stopping));
        let change = shared.mutation.clone().acquire_owned().await.unwrap();
        tokio::time::sleep(Duration::from_millis(6000)).await;
        assert!(shared.manager.lock().await.resolver().is_none());
        drop(change);
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            while shared.manager.lock().await.resolver().is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(started.is_ok(), "DNS restarted once the permit was free");
        let generation = shared.manager.lock().await.status()["generation"].clone();
        shared.manager.lock().await.simulate_exit();
        let restarted = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let manager = shared.manager.lock().await;
                if manager.resolver().is_some() && manager.status()["generation"] != generation {
                    break;
                }
            }
        })
        .await;
        assert!(restarted.is_ok(), "DNS restarted after its task ended");
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        shared.manager.lock().await.terminal_shutdown().await;
    }
}
