//! Authentication attempts are charged to socket peers, independently of the
//! global CPU budget for password hashing. No forwarding headers are identities.
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use tokio::sync::Semaphore;

use super::{ApiError, error, internal};

const WINDOW: Duration = Duration::from_secs(60);
const ATTEMPTS: usize = 5;
const MAX_SOURCES: usize = 1024;
const RECLAIM_SCAN: usize = 16;
const MAX_HASHES: usize = 2;

#[derive(Default)]
struct Sources {
    entries: HashMap<IpAddr, VecDeque<Instant>>,
    sweep: VecDeque<IpAddr>,
}

pub(super) struct Budget {
    sources: Mutex<Sources>,
    hashing: Arc<Semaphore>,
}

impl Budget {
    pub fn new() -> Self {
        Self {
            sources: Mutex::new(Sources::default()),
            hashing: Arc::new(Semaphore::new(MAX_HASHES)),
        }
    }

    pub fn attempt(&self, peer: IpAddr) -> Result<(), ApiError> {
        self.attempt_at(peer, Instant::now())
    }

    fn attempt_at(&self, peer: IpAddr, now: Instant) -> Result<(), ApiError> {
        let peer = match peer {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            _ => peer,
        };
        let mut sources = self.sources.lock().unwrap();
        // Sweep a bounded batch even below capacity so unused identities are
        // reclaimed during ordinary traffic. Never evict an unexpired debt.
        for _ in 0..RECLAIM_SCAN.min(sources.sweep.len()) {
            let source = sources.sweep.pop_front().unwrap();
            let attempts = sources.entries.get_mut(&source).unwrap();
            expire(attempts, now);
            if attempts.is_empty() {
                sources.entries.remove(&source);
            } else {
                sources.sweep.push_back(source);
            }
        }
        if !sources.entries.contains_key(&peer) {
            if sources.entries.len() == MAX_SOURCES {
                return Err(limited());
            }
            sources.entries.insert(peer, VecDeque::new());
            sources.sweep.push_back(peer);
        }
        let attempts = sources.entries.get_mut(&peer).unwrap();
        expire(attempts, now);
        if attempts.len() == ATTEMPTS {
            return Err(limited());
        }
        // Timestamp capture precedes locking: keep the queue ordered when
        // contending callers acquire the lock in a different order.
        attempts.push_back(attempts.back().copied().map_or(now, |last| last.max(now)));
        Ok(())
    }

    pub async fn password_work<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let permit = self.hashing.clone().try_acquire_owned().map_err(|_| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "AUTH_BUSY",
                "Authentication is busy; try again shortly",
            )
        })?;
        tokio::task::spawn_blocking(move || {
            // Dropping the HTTP future cannot cancel running blocking work.
            // Its CPU permit must live here, until hashing actually finishes.
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| internal())
    }
}

fn expire(attempts: &mut VecDeque<Instant>, now: Instant) {
    while attempts
        .front()
        .is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW)
    {
        attempts.pop_front();
    }
}

fn limited() -> ApiError {
    error(
        StatusCode::TOO_MANY_REQUESTS,
        "LOGIN_LIMIT",
        "Try again in one minute",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_source_sliding_window_and_mapped_ipv4_share_debt() {
        let budget = Budget::new();
        let now = Instant::now();
        let peer = "192.0.2.1".parse().unwrap();
        for i in 0..ATTEMPTS {
            budget
                .attempt_at(peer, now + Duration::from_secs(i as u64))
                .unwrap();
        }
        assert!(
            budget
                .attempt_at(
                    "::ffff:192.0.2.1".parse().unwrap(),
                    now + Duration::from_secs(5)
                )
                .is_err()
        );
        budget
            .attempt_at("192.0.2.2".parse().unwrap(), now + Duration::from_secs(5))
            .unwrap();
        budget.attempt_at(peer, now + WINDOW).unwrap();
        assert!(budget.attempt_at(peer, now + WINDOW).is_err());
        budget
            .attempt_at(peer, now + WINDOW + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn source_table_and_reclamation_are_bounded_without_resetting_active_debt() {
        let budget = Budget::new();
        let now = Instant::now();
        for i in 0..MAX_SOURCES {
            budget
                .attempt_at(IpAddr::V4((0xc000_0000 + i as u32).into()), now)
                .unwrap();
        }
        let new_peer = "203.0.113.1".parse().unwrap();
        assert!(budget.attempt_at(new_peer, now).is_err());
        assert_eq!(budget.sources.lock().unwrap().entries.len(), MAX_SOURCES);
        budget.attempt_at(new_peer, now + WINDOW).unwrap();
        let sources = budget.sources.lock().unwrap();
        assert_eq!(sources.entries.len(), MAX_SOURCES - RECLAIM_SCAN + 1);
        assert_eq!(sources.sweep.len(), sources.entries.len());
        drop(sources);
        for _ in 0..MAX_SOURCES / RECLAIM_SCAN {
            let _ = budget.attempt_at(new_peer, now + WINDOW);
        }
        assert_eq!(budget.sources.lock().unwrap().entries.len(), 1);
    }

    #[tokio::test]
    async fn hash_budget_survives_caller_cancellation_until_blocking_work_exits() {
        let budget = Arc::new(Budget::new());
        let mut workers = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..MAX_HASHES {
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let owner = budget.clone();
            workers.push(tokio::spawn(async move {
                owner
                    .password_work(move || {
                        started.send(()).unwrap();
                        let _ = wait.recv();
                    })
                    .await
            }));
            running.await.unwrap();
            releases.push(release);
        }
        assert_eq!(
            budget.password_work(|| ()).await.unwrap_err().1,
            "AUTH_BUSY"
        );
        // The global CPU budget does not consume other sources' attempt quota.
        budget.attempt("192.0.2.1".parse().unwrap()).unwrap();
        for worker in workers {
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
        }
        assert_eq!(
            budget.password_work(|| ()).await.unwrap_err().1,
            "AUTH_BUSY"
        );
        drop(releases);
        tokio::time::timeout(Duration::from_secs(5), async {
            while budget.hashing.available_permits() != MAX_HASHES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        budget.password_work(|| ()).await.unwrap();
    }
}
