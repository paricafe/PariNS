//! One checker belongs to the management process, not to DNS generations.
use super::Shared;
use crate::config::UpdatesConfig;
use std::{sync::Arc, time::Duration};

pub(super) async fn run(shared: Arc<Shared>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let updates = shared.manager.lock().await.updates.clone();
    let mut revision = None;
    let mut settings = UpdatesConfig::default();
    loop {
        if *stop.borrow() {
            return;
        }
        let saved = {
            let manager = shared.manager.lock().await;
            manager.saved.as_ref().map(|s| {
                (
                    s.revision,
                    (Some(s.revision) != revision).then(|| s.toml.clone()),
                )
            })
        };
        if let Some((current, changed)) = saved {
            if let Some(toml) = changed {
                // The saved document was already validated by Config. Read only
                // management settings; do not recompile policy or touch TLS files.
                #[derive(serde::Deserialize)]
                struct SavedSettings {
                    #[serde(default)]
                    updates: UpdatesConfig,
                }
                let parsed =
                    tokio::task::spawn_blocking(move || toml::from_str::<SavedSettings>(&toml))
                        .await;
                match parsed {
                    Ok(Ok(parsed)) => {
                        settings = parsed.updates;
                        revision = Some(current);
                    }
                    _ => return, // Config owns the persisted schema.
                }
            }
            if !*stop.borrow() && updates.due(settings.auto_check) {
                // Do not abort an atomic persistence worker during shutdown.
                // Network work already has a finite timeout; serve owns the
                // overall transaction drain deadline.
                updates.perform_check(current, &settings).await;
            }
        }
        tokio::select! {
            _ = stop.changed() => {},
            _ = updates.notify.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}
