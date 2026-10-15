//! Restarts a saved DNS configuration whose generation failed to start, such as
//! a listener port held by another program after a reboot. One task belongs to
//! the management process; Manager decides when a retry is due (bounded backoff)
//! and the task stops with management shutdown.
use super::Shared;
use std::{sync::Arc, time::Duration};

pub(super) async fn run(shared: Arc<Shared>, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        // Poll once a second: retries are due at most every 5 to 60 seconds, and
        // a skipped round must not spin while it stays due.
        tokio::select! {
            _ = stop.changed() => return,
            () = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
        if *stop.borrow() {
            return;
        }
        let due = |manager: &super::runtime::Manager| {
            manager
                .startup_retry_at()
                .is_some_and(|at| at <= std::time::Instant::now())
        };
        // Take the mutation permit only when a retry is due, so idle polling
        // never makes an administrator's change see BUSY.
        if !due(&*shared.manager.lock().await) {
            continue;
        }
        // A configuration change in progress owns DNS startup itself; try again
        // on the next round rather than queueing behind it. An update freeze does
        // not block this: restarting the saved revision changes no configuration,
        // and a freshly updated process needs DNS to pass launch readiness.
        let Ok(_permit) = shared.mutation.clone().try_acquire_owned() else {
            continue;
        };
        let mut manager = shared.manager.lock().await;
        if due(&manager) {
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
    async fn retries_after_the_conflict_ends_yields_to_mutations_and_stops_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let state = Store::open(&temp.path().join("state")).unwrap();
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        state
            .save(&store::Stored {
                username: "admin".into(),
                password_hash: store::hash_password("local-test-password").unwrap(),
                toml: format!("listen='127.0.0.1:{port}'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=100\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n"),
                previous: None,
                revision: 1,
            })
            .unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(std::sync::Mutex::new(Active {
            snapshot: Arc::new(Snapshot::initial()),
            sessions: Vec::new(),
        }));
        let manager = Manager::open(state, address, active.clone()).await.unwrap();
        assert!(manager.resolver().is_none());
        let shared = Arc::new(Shared {
            filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            active,
            auth: auth_budget::Budget::new(),
            mutation: Arc::new(Semaphore::new(1)),
            address,
        });
        // An update freeze (e.g. a freshly updated process awaiting launch
        // readiness) must not block restarting the saved revision.
        shared
            .manager
            .lock()
            .await
            .updates
            .frozen
            .store(true, std::sync::atomic::Ordering::Release);
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run(shared.clone(), stopping));
        // A configuration change holds the permit past the first due time:
        // the retry waits for it instead of starting DNS underneath it.
        let change = shared.mutation.clone().acquire_owned().await.unwrap();
        drop(holder);
        tokio::time::sleep(Duration::from_millis(6000)).await;
        assert!(shared.manager.lock().await.resolver().is_none());
        drop(change);
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            while shared.manager.lock().await.resolver().is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(started.is_ok(), "DNS restarted once the port was free");
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        shared.manager.lock().await.terminal_shutdown().await;
    }
}
