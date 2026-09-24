//! Owned, bounded refresh work. No queue, no task survives its cache generation.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use hickory_proto::op::Message;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::config::PrefetchConfig;

#[derive(Default)]
pub(super) struct Refresh {
    state: Mutex<State>,
    scheduled: AtomicU64,
    success: AtomicU64,
    failure: AtomicU64,
    rejected: AtomicU64,
}

#[derive(Default)]
struct State {
    closed: bool,
    tasks: JoinSet<()>,
    active: HashSet<Vec<u8>>,
    cooldown: HashMap<Vec<u8>, Instant>,
    starts: VecDeque<Instant>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub active: usize,
    pub scheduled: u64,
    pub success: u64,
    pub failure: u64,
    pub rejected: u64,
}

impl Refresh {
    pub(super) fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().expect("refresh state poisoned");
        Snapshot {
            active: state.active.len(),
            scheduled: self.scheduled.load(Relaxed),
            success: self.success.load(Relaxed),
            failure: self.failure.load(Relaxed),
            rejected: self.rejected.load(Relaxed),
        }
    }

    pub(super) fn schedule<F>(
        self: &Arc<Self>,
        query: &Message,
        epoch: u64,
        config: &PrefetchConfig,
        work: impl FnOnce() -> F,
    ) where
        F: Future<Output = bool> + Send + 'static,
    {
        let Some(mut key) = crate::protocol::canonical_work_key(query) else {
            return;
        };
        key.extend_from_slice(&epoch.to_be_bytes());
        let now = Instant::now();
        let mut state = self.state.lock().expect("refresh state poisoned");
        while state.tasks.try_join_next().is_some() {}
        state.cooldown.retain(|_, until| *until > now);
        while state
            .starts
            .front()
            .is_some_and(|start| now.duration_since(*start) >= Duration::from_secs(1))
        {
            state.starts.pop_front();
        }
        if state.closed
            || state.active.len() >= config.max_inflight
            || state.starts.len() >= config.rate_per_sec as usize
            || state.active.contains(&key)
            || state.cooldown.contains_key(&key)
        {
            self.rejected.fetch_add(1, Relaxed);
            return;
        }
        state.active.insert(key.clone());
        state.starts.push_back(now);
        self.scheduled.fetch_add(1, Relaxed);
        let done = Completion {
            owner: Arc::downgrade(self),
            key,
            backoff: Duration::from_secs(config.backoff_secs),
            // A bounded failure table cannot grow with random names or ECS prefixes.
            capacity: config.max_inflight.saturating_mul(8).clamp(64, 4096),
            result: None,
        };
        // Register the shared upstream work synchronously after admission. Delaying
        // registration until this task is polled permits an expiry-boundary race.
        let work = work();
        state.tasks.spawn(async move {
            let mut done = done;
            done.result = Some(work.await);
        });
    }

    pub(super) fn cancel(&self) {
        let mut state = self.state.lock().expect("refresh state poisoned");
        state.closed = true;
        state.tasks.abort_all();
    }

    pub(super) async fn shutdown(&self) {
        let mut tasks = {
            let mut state = self.state.lock().expect("refresh state poisoned");
            state.closed = true;
            state.tasks.abort_all();
            std::mem::take(&mut state.tasks)
        };
        while tasks.join_next().await.is_some() {}
    }
}

struct Completion {
    owner: Weak<Refresh>,
    key: Vec<u8>,
    backoff: Duration,
    capacity: usize,
    result: Option<bool>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let mut state = owner.state.lock().expect("refresh state poisoned");
        state.active.remove(&self.key);
        match self.result {
            Some(true) => {
                owner.success.fetch_add(1, Relaxed);
            }
            Some(false) => {
                owner.failure.fetch_add(1, Relaxed);
                if !state.closed {
                    if state.cooldown.len() >= self.capacity
                        && let Some(oldest) = state
                            .cooldown
                            .iter()
                            .min_by_key(|(_, until)| **until)
                            .map(|(key, _)| key.clone())
                    {
                        state.cooldown.remove(&oldest);
                    }
                    state
                        .cooldown
                        .insert(self.key.clone(), Instant::now() + self.backoff);
                }
            }
            None => {} // Cancellation is not an upstream failure.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::{
        op::{MessageType, OpCode, Query},
        rr::{Name, RecordType},
    };
    use std::sync::atomic::AtomicBool;

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Relaxed);
        }
    }

    #[tokio::test]
    async fn cancel_and_owner_drop_destroy_pending_network_work() {
        for drop_owner in [false, true] {
            let refresh = Arc::new(Refresh::default());
            let mut query = Message::new(0, MessageType::Query, OpCode::Query);
            query.add_query(Query::query(
                Name::from_ascii("cancel.test.").unwrap(),
                RecordType::A,
            ));
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = Dropped(dropped.clone());
            let (started, ready) = tokio::sync::oneshot::channel();
            refresh.schedule(&query, 0, &PrefetchConfig::default(), || async move {
                let _guard = guard;
                let _ = started.send(());
                std::future::pending::<bool>().await
            });
            ready.await.unwrap();
            if drop_owner {
                drop(refresh);
            } else {
                refresh.cancel();
                refresh.shutdown().await;
                assert_eq!(refresh.snapshot().active, 0);
                assert_eq!(refresh.snapshot().failure, 0);
                refresh.schedule(&query, 0, &PrefetchConfig::default(), || async {
                    panic!("a closed owner must not start work");
                });
                assert_eq!(refresh.snapshot().scheduled, 1);
                assert_eq!(refresh.snapshot().rejected, 1);
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                while !dropped.load(Relaxed) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancel drops pending network future");
        }
    }
}
