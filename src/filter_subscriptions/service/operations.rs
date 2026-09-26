use super::super::{
    download::{DownloadOutcome, MAX_BYTES},
    store::{PreparedMetadata, SourceStatus as StoredStatus},
    writer::StagingWriter,
};
use super::*;
use crate::policy::canonical::{Builder, Limits};
use std::{
    io::{BufReader, Seek},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkRequest {
    Prepare {
        config_revision: u64,
        source: DraftSource,
    },
    Refresh {
        config_revision: u64,
        source_id: Option<String>,
        automatic: bool,
    },
}
impl WorkRequest {
    fn revision(&self) -> u64 {
        match self {
            Self::Prepare {
                config_revision, ..
            }
            | Self::Refresh {
                config_revision, ..
            } => *config_revision,
        }
    }
}
fn check_work(service: &Service, deadline: tokio::time::Instant) -> Result<()> {
    service.check_open()?;
    ensure!(
        tokio::time::Instant::now() < deadline,
        Failure::new("subscription_cancelled")
    );
    Ok(())
}
pub struct Begin {
    pub operation_id: String,
    pub work: Option<Work>,
}
pub struct Work {
    service: Arc<Service>,
    lease: Arc<Lease>,
    request: WorkRequest,
    id: String,
    local: Policy,
    settings: Settings,
    content_revision: u64,
    local_digest: [u8; 32],
    deadline: tokio::time::Instant,
}
pub struct WorkCandidate {
    work: Work,
    records: Vec<SourceRecord>,
    pins: Vec<WorkPin>,
    material: Option<Material>,
    failures: Vec<(String, StoredStatus)>,
}

impl Service {
    pub async fn begin(self: &Arc<Self>, request: WorkRequest) -> Result<Begin> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || owner.begin_sync(request)).await?
    }
    fn begin_sync(self: &Arc<Self>, mut request: WorkRequest) -> Result<Begin> {
        self.check_revision(request.revision())?;
        if let WorkRequest::Prepare { source, .. } = &mut request {
            super::super::settings::validate_id(&source.id)?;
            source.url = Source::new(&source.url, source.format)?.url;
        }
        let state = self.state.lock().unwrap();
        if state.active_request.as_ref() == Some(&request) {
            return Ok(Begin {
                operation_id: state
                    .operation
                    .as_ref()
                    .expect("active operation")
                    .id
                    .clone(),
                work: None,
            });
        }
        let lease = self.acquire()?;
        self.handle
            .ensure_available()
            .map_err(|_| Failure::new("busy"))?;
        let (kind, source_id, fingerprint) = match &request {
            WorkRequest::Prepare { source, .. } => {
                let identity = Source::new(&source.url, source.format)?;
                ensure!(
                    !state.settings.effective().any(|s| s
                        .identity()
                        .is_ok_and(|s| s.fingerprint == identity.fingerprint)),
                    Failure::new("subscription_active_source")
                );
                (
                    "prepare",
                    Some(source.id.clone()),
                    Some(identity.fingerprint),
                )
            }
            WorkRequest::Refresh {
                source_id,
                automatic,
                ..
            } => {
                if let Some(id) = source_id {
                    let source = state
                        .settings
                        .sources
                        .iter()
                        .find(|s| &s.id == id)
                        .context(Failure::new("subscription_material_missing"))?;
                    if *automatic {
                        ensure!(
                            state.settings.enabled && source.enabled && source.auto_update,
                            Failure::new("revision_conflict")
                        );
                    }
                }
                let selected = state.settings.sources.iter().filter(|s| {
                    source_id
                        .as_ref()
                        .map_or(state.settings.enabled && s.enabled, |id| id == &s.id)
                });
                for source in selected {
                    let fp = source.identity()?.fingerprint;
                    if !*automatic {
                        ensure!(
                            state
                                .records
                                .iter()
                                .find(|r| r.fingerprint == fp)
                                .and_then(|r| r.status.last_attempt)
                                .is_none_or(|last| now().saturating_sub(last) >= 60),
                            Failure::new("subscription_rate_limited")
                        );
                    }
                }
                ("refresh", source_id.clone(), None)
            }
        };
        let id = format!("{:016x}", rand::random::<u64>());
        drop(state);
        // begin is an admission transaction under the caller's mutation/freeze
        // gate; execute below never changes catalog authority.
        {
            let mut locked = self.store.lock().unwrap();
            if locked.is_none() {
                let quota = self.state.lock().unwrap().settings.max_disk_bytes;
                *locked = Some(Store::open(&self.path, quota, now())?);
            }
            let store = locked
                .as_mut()
                .context(Failure::new("subscription_storage_unavailable"))?;
            self.sync_roots(store)?;
            store.collect(now(), false)?;
            self.observe_store(store);
        }
        let mut state = self.state.lock().unwrap();
        self.check_open()?;
        ensure!(
            state.revision == request.revision(),
            Failure::new("revision_conflict")
        );
        state.operation = Some(Operation {
            id: id.clone(),
            kind: kind.into(),
            source_id,
            fingerprint,
            status: "running".into(),
            started_at: now(),
            finished_at: None,
            error: None,
            sha256: None,
            rules: None,
        });
        state.active_request = Some(request.clone());
        Ok(Begin {
            operation_id: id.clone(),
            work: Some(Work {
                service: self.clone(),
                lease,
                request,
                id,
                local: state.local.clone(),
                settings: state.settings.clone(),
                content_revision: state.content_revision,
                local_digest: state.local.semantic_digest(),
                deadline: tokio::time::Instant::now() + Duration::from_secs(300),
            }),
        })
    }

    pub async fn execute(self: &Arc<Self>, work: Work) -> Result<WorkCandidate> {
        ensure!(
            Arc::ptr_eq(self, &work.service),
            Failure::new("revision_conflict")
        );
        let sources: Vec<(Source, u16)> = match &work.request {
            WorkRequest::Prepare { source, .. } => {
                vec![(Source::new(&source.url, source.format)?, 24)]
            }
            WorkRequest::Refresh {
                source_id,
                automatic,
                ..
            } => work
                .settings
                .sources
                .iter()
                .filter(|s| {
                    source_id
                        .as_ref()
                        .map_or(work.settings.enabled && s.enabled, |id| id == &s.id)
                })
                .filter(|s| !*automatic || s.auto_update)
                .map(|s| Ok((s.identity()?, s.update_interval_hours)))
                .collect::<Result<_>>()?,
        };
        let mut records = Vec::new();
        let mut pins = Vec::new();
        let mut failures = Vec::new();
        for (source, interval) in sources {
            self.check_revision(work.request.revision())?;
            if tokio::time::Instant::now() >= work.deadline {
                anyhow::bail!(Failure::new("subscription_download"));
            }
            let prepared = matches!(work.request, WorkRequest::Prepare { .. });
            match self.fetch_source(&work, &source, interval, prepared).await {
                Ok((record, pin)) => {
                    records.push(record);
                    pins.push(pin);
                }
                Err(error) => {
                    let failure = Failure::from_error(&error);
                    let count = self
                        .state
                        .lock()
                        .unwrap()
                        .records
                        .iter()
                        .find(|r| r.fingerprint == source.fingerprint)
                        .map_or(1, |r| r.status.failures.saturating_add(1));
                    let retry_after = error
                        .downcast_ref::<super::super::download::DownloadError>()
                        .and_then(|e| e.retry_after_unix);
                    let delay = match count {
                        1 => 300,
                        2 => 900,
                        _ => 3600,
                    }
                    .min(u64::from(interval) * 3600);
                    failures.push((
                        source.fingerprint,
                        StoredStatus {
                            last_attempt: Some(now()),
                            next_update: Some(retry_after.unwrap_or(now() + delay).max(now() + 60)),
                            failures: count,
                            last_error: Some(failure.code),
                            error_line: failure.line,
                        },
                    ));
                    // A failed source keeps its old selection. Other successes
                    // form one complete aggregate, never a partial compile.
                }
            }
        }
        self.check_revision(work.request.revision())?;
        let material = if !records.is_empty() && matches!(work.request, WorkRequest::Refresh { .. })
        {
            let owner = self.clone();
            let local = work.local.clone();
            let settings = work.settings.clone();
            let replacements = records.clone();
            let lease = work.lease.clone();
            let deadline = work.deadline;
            let material = tokio::task::spawn_blocking(move || {
                let _lease = lease;
                let mut locked = owner.store.lock().unwrap();
                let store = locked
                    .as_mut()
                    .context(Failure::new("subscription_storage_unavailable"))?;
                material::compile(
                    store,
                    &settings,
                    &local,
                    &replacements,
                    owner.live_bytes(&local),
                    true,
                    || check_work(&owner, deadline),
                )
            })
            .await??;
            Some(material)
        } else {
            None
        };
        Ok(WorkCandidate {
            work,
            records,
            pins,
            material,
            failures,
        })
    }

    async fn fetch_source(
        self: &Arc<Self>,
        work: &Work,
        source: &Source,
        interval: u16,
        reuse: bool,
    ) -> Result<(SourceRecord, WorkPin)> {
        let owner = self.clone();
        let source = source.clone();
        let identity = source.clone();
        let lease = work.lease.clone();
        let staged = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let mut locked = owner.store.lock().unwrap();
            let store = locked
                .as_mut()
                .context(Failure::new("subscription_storage_unavailable"))?;
            if reuse && let Some((record, _)) = store.read_prepared(&identity.fingerprint, now())? {
                let pin = store.pin(&record.sha256)?;
                return Ok::<_, anyhow::Error>(Err((record, pin)));
            }
            let old = store
                .verified_content(&identity.fingerprint)
                .ok()
                .map(|(r, _)| r);
            let stage = store
                .begin_staging(MAX_BYTES, now())
                .map_err(|_| Failure::new("subscription_disk_limit"))?;
            Ok(Ok((stage, old)))
        })
        .await??;
        let (stage, old) = match staged {
            Ok(staged) => staged,
            Err(existing) => return Ok(existing),
        };
        let mut writer = match StagingWriter::new(&stage) {
            Ok(writer) => writer,
            Err(error) => {
                let owner = self.clone();
                tokio::task::spawn_blocking(move || {
                    owner
                        .store
                        .lock()
                        .unwrap()
                        .as_mut()
                        .expect("store")
                        .discard(stage)
                })
                .await??;
                return Err(error.into());
            }
        };
        let cancel = self.cancel.notified();
        tokio::pin!(cancel);
        cancel.as_mut().enable();
        self.check_open()?;
        let result = tokio::select! {
            result=tokio::time::timeout_at(work.deadline,self.download_source(&source,old.as_ref(),&mut writer))=>match result {
                Ok(value)=>value.map_err(anyhow::Error::from), Err(_)=>Err(Failure::new("subscription_download").into()),
            },
            _=cancel=>Err(Failure::new("subscription_cancelled").into()),
        };
        let file = writer.finish().await;
        let owner = self.clone();
        let lease = work.lease.clone();
        let limits = Limits {
            max_rules: work.settings.max_rules,
            max_memory_bytes: work.settings.max_memory_bytes,
            retained_bytes: self.live_bytes(&work.local) + material::COORDINATOR_BYTES,
        };
        let deadline = work.deadline;
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let mut locked = owner.store.lock().unwrap();
            let store = locked
                .as_mut()
                .context(Failure::new("subscription_storage_unavailable"))?;
            let accepted = (|| -> Result<_> {
                check_work(&owner, deadline)?;
                let mut file = file?;
                let outcome = result?;
                match outcome {
                    DownloadOutcome::NotModified => Ok(None),
                    DownloadOutcome::Downloaded(downloaded) => {
                        file.rewind()?;
                        let mut builder = Builder::new(limits)?;
                        let parsed = builder.parse_source(
                            material::CheckedReader {
                                reader: BufReader::with_capacity(16 * 1024, file),
                                check: || check_work(&owner, deadline),
                            },
                            source.format,
                            0,
                        );
                        check_work(&owner, deadline)?;
                        let stats = parsed?;
                        ensure!(
                            stats.decoded_bytes as u64 == downloaded.bytes,
                            Failure::new("subscription_parse")
                        );
                        // Full parse/limits before object installation; aggregate
                        // sorting is performed only once in the complete build.
                        Ok(Some(PreparedMetadata {
                            fingerprint: source.fingerprint,
                            sha256: downloaded.sha256,
                            bytes: downloaded.bytes,
                            rules: stats.input_rules as u64,
                            validators: downloaded.validators,
                        }))
                    }
                }
            })();
            let metadata = match accepted {
                Ok(v) => v,
                Err(e) => {
                    store.discard(stage)?;
                    return Err(e);
                }
            };
            let mut record = match metadata {
                Some(metadata) => store.install_object(stage, metadata, now())?,
                None => {
                    store.discard(stage)?;
                    let mut record = old.context(Failure::new("subscription_material_missing"))?;
                    record.prepared_at = now();
                    record
                }
            };
            record.status = StoredStatus {
                last_attempt: Some(now()),
                next_update: Some(periodic(now(), interval)),
                failures: 0,
                last_error: None,
                error_line: None,
            };
            let pin = store.pin(&record.sha256)?;
            Ok((record, pin))
        })
        .await?
    }

    pub async fn commit(self: &Arc<Self>, candidate: WorkCandidate) -> Result<()> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || {
            ensure!(
                Arc::ptr_eq(&owner, &candidate.work.service),
                Failure::new("revision_conflict")
            );
            let mut locked = owner.store.lock().unwrap();
            let store = locked
                .as_mut()
                .context(Failure::new("subscription_storage_unavailable"))?;
            // Parent already holds its mutation/freeze gate. State excludes
            // close/config-revision changes through the rename+Arc commit point.
            let mut state = owner.state.lock().unwrap();
            check_work(&owner, candidate.work.deadline)?;
            ensure!(
                state.revision == candidate.work.request.revision()
                    && state.local.semantic_digest() == candidate.work.local_digest,
                Failure::new("revision_conflict")
            );
            ensure!(
                store.revision() == candidate.work.content_revision,
                Failure::new("revision_conflict")
            );
            let outcome = store.commit_batch_with_check(
                candidate.records.clone(),
                &candidate.failures,
                candidate.work.content_revision,
                || check_work(&owner, candidate.work.deadline),
            )?;
            if let Some(material) = candidate.material {
                state.input_rules = material.policy.input_rules();
                state.ready_policy = true;
                if owner.handle.snapshot().policy.publication_digest()
                    != material.policy.publication_digest()
                {
                    owner.handle.publish(material.policy);
                }
                state.input_digest = Some(material.material_digest);
                state.unavailable = None;
                state.index_pin = material.index_pin;
            }
            state.content_revision = store.revision();
            let id = candidate.work.id.clone();
            if let Some(operation) = state.operation.as_mut().filter(|op| op.id == id) {
                operation.status = if candidate.failures.is_empty() {
                    "succeeded"
                } else {
                    "failed"
                }
                .into();
                operation.error = candidate.failures.first().map(|(_, status)| Failure {
                    code: status
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "subscription_download".into()),
                    line: status.error_line,
                });
                operation.finished_at = Some(now());
                if let Some(record) = candidate.records.first() {
                    operation.sha256 = Some(record.sha256.clone());
                    operation.rules = Some(record.rules);
                }
            }
            state.recent = state.operation.take();
            state.active_request = None;
            if outcome == super::super::store::CommitOutcome::CommittedUncertain {
                // Current policy is committed. GC is paused by Store until a
                // future worker reconciles the actual durable authority.
                if let Some(operation) = state.recent.as_mut() {
                    operation.error = Some(Failure::new("subscription_durability_uncertain"));
                }
            }
            drop(state);
            owner.observe_store(store);
            drop(candidate.pins);
            Ok(())
        })
        .await?
    }

    pub fn fail(&self, id: &str, error: &anyhow::Error) {
        let failure = Failure::from_error(error);
        let mut state = self.state.lock().unwrap();
        if state
            .operation
            .as_ref()
            .is_some_and(|operation| operation.id == id)
            && let Some(WorkRequest::Refresh { source_id, .. }) = state.active_request.clone()
        {
            let targets: Vec<_> = state
                .settings
                .sources
                .iter()
                .filter(|source| {
                    source_id
                        .as_ref()
                        .map_or(state.settings.enabled && source.enabled, |id| {
                            id == &source.id
                        })
                })
                .filter_map(|source| {
                    source
                        .identity()
                        .ok()
                        .map(|s| (s.fingerprint, source.update_interval_hours))
                })
                .collect();
            for (fp, interval) in targets {
                if let Some(record) = state.records.iter_mut().find(|r| r.fingerprint == fp) {
                    record.status.failures = record.status.failures.saturating_add(1);
                    record.status.last_attempt = Some(now());
                    let delay = match record.status.failures {
                        1 => 300,
                        2 => 900,
                        _ => 3600,
                    }
                    .min(u64::from(interval) * 3600);
                    record.status.next_update = Some(now() + delay);
                    record.status.last_error = Some(failure.code.clone());
                    record.status.error_line = failure.line;
                }
            }
        }
        if let Some(operation) = state
            .operation
            .as_mut()
            .filter(|operation| operation.id == id)
        {
            operation.status = if failure.code == "subscription_cancelled" {
                "cancelled"
            } else {
                "failed"
            }
            .into();
            operation.finished_at = Some(now());
            operation.error = Some(failure);
            state.recent = state.operation.take();
            state.active_request = None;
        }
    }

    async fn download_source(
        &self,
        source: &Source,
        old: Option<&SourceRecord>,
        writer: &mut StagingWriter,
    ) -> std::result::Result<DownloadOutcome, super::super::download::DownloadError> {
        #[cfg(test)]
        {
            let responses = self
                .responses
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|queue| queue.pop_front());
            if let Some(responses) = responses {
                return super::super::download::download_fixture(
                    &source.url,
                    old.map(|r| &r.validators),
                    old.map(|r| r.sha256.as_str()),
                    writer,
                    responses,
                )
                .await;
            }
        }
        self.reader
            .download(
                &source.url,
                old.map(|r| &r.validators),
                old.map(|r| r.sha256.as_str()),
                writer,
            )
            .await
    }

    pub fn next_due(&self, at: u64) -> Option<WorkRequest> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let state = self.state.lock().unwrap();
        if state.operation.is_some() {
            return None;
        }
        state
            .settings
            .effective()
            .filter(|s| s.auto_update)
            .find(|s| {
                let fp = s.identity().ok().map(|s| s.fingerprint);
                state
                    .records
                    .iter()
                    .find(|r| Some(&r.fingerprint) == fp.as_ref())
                    .and_then(|r| r.status.next_update)
                    .is_some_and(|due| due <= at)
            })
            .map(|source| WorkRequest::Refresh {
                config_revision: state.revision,
                source_id: Some(source.id.clone()),
                automatic: true,
            })
    }

    pub(super) fn seed_schedule(&self) -> Result<()> {
        let mut locked = self.store.lock().unwrap();
        let Some(store) = locked.as_mut() else {
            return Ok(());
        };
        let settings = self.state.lock().unwrap().settings.clone();
        for source in settings.effective().filter(|s| s.auto_update) {
            let fp = source.identity()?.fingerprint;
            if let Some(record) = store.record(&fp) {
                let mut status = record.status.clone();
                let due = if status.failures > 0 {
                    status.next_update.unwrap_or(now())
                } else {
                    periodic(record.prepared_at, source.update_interval_hours)
                };
                status.next_update = Some(if due <= now() {
                    now() + rand::random_range(60..=300)
                } else {
                    due
                });
                store.update_status(&fp, status)?;
            }
        }
        self.observe_store(store);
        Ok(())
    }

    pub async fn run_file_scheduler(self: Arc<Self>) {
        loop {
            let notified = self.cancel.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.closed.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {_=notified=>break,_=tokio::time::sleep(Duration::from_secs(30))=>{}}
            if self.closed.load(Ordering::Acquire) {
                break;
            }
            let Some(request) = self.next_due(now()) else {
                continue;
            };
            let Ok(begin) = self.begin(request).await else {
                continue;
            };
            let Some(work) = begin.work else {
                continue;
            };
            let result = match self.execute(work).await {
                Ok(candidate) => self.commit(candidate).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                self.fail(&begin.operation_id, &error);
            }
        }
    }

    pub async fn bootstrap_new(self: &Arc<Self>) -> Result<()> {
        if self
            .state
            .lock()
            .unwrap()
            .settings
            .effective()
            .next()
            .is_none()
        {
            return Ok(());
        }
        let settings = self.state.lock().unwrap().settings.clone();
        let owner = self.clone();
        let missing = tokio::task::spawn_blocking(move || {
            let locked = owner.store.lock().unwrap();
            let store = locked
                .as_ref()
                .context(Failure::new("subscription_storage_unavailable"))?;
            let mut missing = Vec::new();
            for source in settings.effective() {
                let fp = source.identity()?.fingerprint;
                if store.record(&fp).is_some() {
                    store
                        .verified_content(&fp)
                        .map_err(|_| Failure::new("subscription_material_missing"))?;
                } else {
                    missing.push(source.clone());
                }
            }
            Ok::<_, anyhow::Error>(missing)
        })
        .await??;
        if !missing.is_empty() {
            let begin = self
                .begin(WorkRequest::Refresh {
                    config_revision: self.config_revision(),
                    source_id: None,
                    automatic: true,
                })
                .await?;
            let Some(mut work) = begin.work else {
                anyhow::bail!(Failure::new("busy"));
            };
            work.deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            let result = async {
                let mut records = Vec::new();
                let mut pins = Vec::new();
                for source in missing {
                    let (record, pin) = self
                        .fetch_source(
                            &work,
                            &source.identity()?,
                            source.update_interval_hours,
                            false,
                        )
                        .await?;
                    records.push(record);
                    pins.push(pin);
                }
                let owner = self.clone();
                let local = work.local.clone();
                let settings = work.settings.clone();
                let replacements = records.clone();
                let lease = work.lease.clone();
                let deadline = work.deadline;
                let material = tokio::task::spawn_blocking(move || {
                    let _lease = lease;
                    let mut locked = owner.store.lock().unwrap();
                    let store = locked
                        .as_mut()
                        .context(Failure::new("subscription_storage_unavailable"))?;
                    material::compile(
                        store,
                        &settings,
                        &local,
                        &replacements,
                        owner.live_bytes(&local),
                        true,
                        || check_work(&owner, deadline),
                    )
                })
                .await??;
                self.commit(WorkCandidate {
                    work,
                    records,
                    pins,
                    material: Some(material),
                    failures: vec![],
                })
                .await
            }
            .await;
            if let Err(error) = result {
                self.fail(&begin.operation_id, &error);
                return Err(error);
            }
        }
        ensure!(self.ready(), Failure::new("subscription_material_missing"));
        Ok(())
    }
}
pub(super) fn periodic(at: u64, hours: u16) -> u64 {
    let period = u64::from(hours) * 3600;
    let jitter = period / 10;
    at.saturating_add(period - jitter + rand::random_range(0..=2 * jitter))
}
