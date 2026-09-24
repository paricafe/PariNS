//! Process-owned metrics and runtime persistence shared across DNS generations.
use crate::{
    metrics::Metrics,
    query_log::{ListOptions, Page, QueryLog},
    storage::{self, RuntimeSettings},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub struct RuntimeServices {
    pub metrics: Arc<Metrics>,
    pub query_log: Arc<QueryLog>,
    pub storage: storage::Handle,
    pub health: crate::runtime_health::Health,
    dns_transition: Mutex<()>,
}
impl RuntimeServices {
    pub fn open(dir: &Path, settings: RuntimeSettings, revision: u64) -> anyhow::Result<Arc<Self>> {
        let metrics = Arc::new(Metrics::default());
        let storage = storage::Handle::open(dir, settings, revision, metrics.clone())?;
        Ok(Arc::new(Self {
            metrics,
            query_log: Arc::new(QueryLog::with_storage(storage.clone())),
            storage,
            health: Default::default(),
            dns_transition: Mutex::new(()),
        }))
    }
    pub fn ephemeral(settings: RuntimeSettings) -> Arc<Self> {
        let metrics = Arc::new(Metrics::default());
        let storage = storage::Handle::ephemeral(settings, metrics.clone())
            .expect("validated ephemeral runtime settings");
        Arc::new(Self {
            metrics,
            query_log: Arc::new(QueryLog::with_storage(storage.clone())),
            storage,
            health: Default::default(),
            dns_transition: Mutex::new(()),
        })
    }
    pub fn publish_settings(&self, settings: RuntimeSettings, revision: u64) {
        self.storage.publish_settings(settings, revision);
    }
    pub fn set_dns_state(&self, running: bool, generation: u64) {
        use crate::runtime_health::State;
        self.set_dns_health(
            if running {
                State::Running
            } else {
                State::Stopped
            },
            generation,
            None,
        );
    }
    pub fn set_dns_health(
        &self,
        state: crate::runtime_health::State,
        generation: u64,
        code: Option<&'static str>,
    ) {
        // Health and the history sampler describe one transition. Holding this
        // short, IO-free lock prevents an accepted older publisher from updating
        // the sampler after a newer generation has already published its health.
        let _transition = self.dns_transition.lock().expect("DNS health transition");
        let running = matches!(state, crate::runtime_health::State::Running);
        if self.health.publish(state, generation, code) {
            self.storage.set_dns_state(running, generation);
        }
    }
    pub fn status(&self) -> storage::Status {
        self.storage.status()
    }
    pub async fn list_logs(&self, options: ListOptions) -> storage::Result<Page> {
        self.storage.list_logs(options).await
    }
    pub async fn clear_logs(&self, epoch: u64) -> storage::Result<storage::ClearResult> {
        self.storage.clear_logs(epoch).await
    }
    pub async fn statistics(
        &self,
        options: storage::HistoryOptions,
    ) -> storage::Result<storage::StatisticsView> {
        self.storage.statistics(options).await
    }
    pub async fn reset_totals(&self, epoch: u64) -> storage::Result<storage::ClearResult> {
        self.storage.reset_totals(epoch).await
    }
    pub async fn clear_history(&self, epoch: u64) -> storage::Result<storage::ClearResult> {
        self.storage.clear_history(epoch).await
    }
    pub async fn flush(&self) -> storage::Result<()> {
        self.storage.flush().await
    }
    pub async fn shutdown(&self) -> storage::Result<()> {
        self.storage.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_health::State;

    #[tokio::test]
    async fn concurrent_health_publications_keep_sampler_on_highest_generation() {
        let services = RuntimeServices::ephemeral(RuntimeSettings::default());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let services = services.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for step in 0..100 {
                        let generation = step * 8 + thread + 1;
                        let state = if generation % 2 == 0 {
                            State::Running
                        } else {
                            State::Stopped
                        };
                        services.set_dns_health(state, generation, None);
                        std::thread::yield_now();
                    }
                });
            }
        });
        let health = services.health.snapshot();
        assert_eq!(health.generation, 800);
        assert!(health.ready);
        // Remove the deliberate transition history so aggregation cannot mix
        // generations; the next forced tail proves the sampler's current state.
        services
            .clear_history(services.status().history_epoch)
            .await
            .unwrap();
        services.flush().await.unwrap();
        let history = services
            .statistics(storage::HistoryOptions::default())
            .await
            .unwrap();
        let point = history.samples.last().unwrap();
        assert_eq!(point.generation, health.generation);
        assert_eq!(point.running, health.ready);
        services.shutdown().await.unwrap();
    }
}
