//! Global bounds on queries the recursor sends to authoritative servers.
//!
//! Every attempt to query an authoritative server, over UDP or TCP, is admitted
//! before its own timeout starts: it takes one concurrency permit, held until the
//! attempt ends, and one token from a shared rate bucket.
//! Closing the bounds at shutdown fails queries waiting for a permit at once and
//! queries waiting for a token when they wake.

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::{FutureExt, future::BoxFuture};
use hickory_resolver::{
    Admission, ConnectionProvider, PoolContext,
    config::ConnectionConfig,
    net::{NetError, runtime::TokioRuntimeProvider},
};
use tokio::{
    sync::Semaphore,
    time::{Instant, sleep_until},
};

pub struct Bounds {
    inflight: Arc<Semaphore>,
    rate: f64,
    burst: f64,
    bucket: Mutex<(f64, Instant)>,
    pub(super) sent: AtomicU64,
    pub(super) rate_waits: AtomicU64,
    pub(super) rate_wait_us: AtomicU64,
    pub(super) refused: AtomicU64,
}

impl Bounds {
    pub fn new(per_second: u32, inflight: usize) -> Arc<Self> {
        let rate = f64::from(per_second);
        Arc::new(Self {
            inflight: Arc::new(Semaphore::new(inflight)),
            rate,
            // One second of burst keeps the sustained rate a hard cap.
            burst: rate,
            bucket: Mutex::new((rate, Instant::now())),
            sent: AtomicU64::new(0),
            rate_waits: AtomicU64::new(0),
            rate_wait_us: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        })
    }

    pub fn close(&self) {
        self.inflight.close();
    }

    pub fn inflight(&self, limit: usize) -> usize {
        limit.saturating_sub(self.inflight.available_permits())
    }

    /// Take one token, waiting while the bucket is empty. A token is taken
    /// only when it is used, so a cancelled wait leaves no debt behind.
    /// Returns false when the bounds closed during the wait.
    async fn take(&self) -> bool {
        let started = Instant::now();
        let mut waited = false;
        loop {
            if self.inflight.is_closed() {
                return false;
            }
            let wait = {
                let now = Instant::now();
                let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
                let elapsed = now.saturating_duration_since(bucket.1).as_secs_f64();
                bucket.0 = (bucket.0 + elapsed * self.rate).min(self.burst);
                bucket.1 = now;
                if bucket.0 >= 1.0 {
                    bucket.0 -= 1.0;
                    None
                } else {
                    Some(Duration::from_secs_f64((1.0 - bucket.0) / self.rate))
                }
            };
            let Some(wait) = wait else {
                if waited {
                    self.rate_waits.fetch_add(1, Ordering::Relaxed);
                    self.rate_wait_us.fetch_add(
                        started.elapsed().as_micros().try_into().unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                }
                return true;
            };
            waited = true;
            sleep_until(Instant::now() + wait).await;
        }
    }
}

#[derive(Clone)]
pub struct Provider {
    runtime: TokioRuntimeProvider,
    bounds: Arc<Bounds>,
}

impl Provider {
    pub fn new(bounds: Arc<Bounds>) -> Self {
        Self {
            runtime: TokioRuntimeProvider::default(),
            bounds,
        }
    }
}

impl ConnectionProvider for Provider {
    type Conn = <TokioRuntimeProvider as ConnectionProvider>::Conn;
    type FutureConn = <TokioRuntimeProvider as ConnectionProvider>::FutureConn;
    type RuntimeProvider = TokioRuntimeProvider;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        self.runtime.new_connection(ip, config, cx)
    }

    fn runtime_provider(&self) -> &TokioRuntimeProvider {
        &self.runtime
    }

    /// The pool awaits this before a server's attempt timeout starts, so waiting
    /// for a permit or a token never counts against or penalizes the server.
    fn admit(&self) -> BoxFuture<'static, Result<Admission, NetError>> {
        let bounds = self.bounds.clone();
        async move {
            let Ok(permit) = bounds.inflight.clone().acquire_owned().await else {
                bounds.refused.fetch_add(1, Ordering::Relaxed);
                return Err(NetError::from("recursion is shut down"));
            };
            if !bounds.take().await {
                bounds.refused.fetch_add(1, Ordering::Relaxed);
                return Err(NetError::from("recursion is shut down"));
            }
            bounds.sent.fetch_add(1, Ordering::Relaxed);
            // The permit is held until the attempt ends.
            Ok(Admission::new(permit))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn cancelled_waits_leave_no_debt() {
        let bounds = Bounds::new(10, 4);
        for _ in 0..10 {
            assert!(bounds.take().await);
        }
        // Many callers give up within one refill interval.
        for _ in 0..50 {
            assert!(
                tokio::time::timeout(Duration::from_millis(1), bounds.take())
                    .await
                    .is_err()
            );
        }
        // One refill interval later a token is available again.
        let started = Instant::now();
        assert!(bounds.take().await);
        assert!(started.elapsed() <= Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn closing_fails_token_waiters() {
        let bounds = Bounds::new(1, 4);
        assert!(bounds.take().await);
        let waiter = tokio::spawn({
            let bounds = bounds.clone();
            async move { bounds.take().await }
        });
        tokio::task::yield_now().await;
        bounds.close();
        assert!(!waiter.await.unwrap());
    }
}
