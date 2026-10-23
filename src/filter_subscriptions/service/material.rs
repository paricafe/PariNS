use super::*;
use crate::filter_subscriptions::settings::SourceSettings;
use crate::policy::{
    canonical::{Builder, Error as ParseError, ErrorKind, Limits},
    radix::Index,
};
use sha2::{Digest, Sha256};
use std::{fs::File, io::BufReader};

const IO_BYTES: usize = 16 * 1024;
// Conservative allocation reserve, not RSS: at most eight simultaneously held
// catalog/record views (Store, cached status, worker captures, candidate, source
// inputs and publication snapshots), each bounded to 2 * the 256 KiB catalog
// limit including Vec/String capacity and descriptors; two encoded catalog
// buffers; four <=64 KiB source-settings/identity sets; 256 KiB fixed IO/scratch.
// This deliberately reserves more than a normal operation holds and is checked
// before reading source descriptors or allocating parser/index buffers.
pub(super) const COORDINATOR_BYTES: usize =
    8 * (2 * 256 * 1024) + 2 * (256 * 1024) + 4 * (64 * 1024) + 256 * 1024;
const TOTAL_TEXT: u64 = 32 * 1024 * 1024;

pub struct Material {
    pub policy: Policy,
    pub material_digest: [u8; 32],
    pub content_revision: u64,
    pub(super) index_pin: Option<WorkPin>,
}
struct Input {
    id: String,
    source: Source,
    response: Option<crate::policy::BlockResponse>,
    record: SourceRecord,
    file: File,
}

/// Names the derived index: the rules, their slots with sources in their
/// configured order, and which slots answer alike (the index keeps covered
/// rules that answer differently). Method changes that keep those classes
/// reuse the index.
fn index_digest(local: &Policy, inputs: &[Input]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"parins-filter-input-v2\0");
    hash.update(local.semantic_digest());
    hash.update(local.local_digest());
    for class in local.aggregate_classes(inputs.iter().map(|i| i.response)) {
        hash.update(class.to_be_bytes());
    }
    hash.update((local.input_rules() as u64).to_be_bytes());
    hash.update((local.input_bytes() as u64).to_be_bytes());
    for input in inputs {
        hash.update([input.id.len() as u8]);
        hash.update(input.id.as_bytes());
        hash.update(input.source.fingerprint.as_bytes());
        hash.update(input.record.sha256.as_bytes());
    }
    hash.finalize().into()
}
/// Identifies the published policy: the derived index plus every block method,
/// so a method change publishes a new generation from the same index.
fn digest(local: &Policy, inputs: &[Input]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"parins-filter-material-v1\0");
    hash.update(index_digest(local, inputs));
    hash.update(local.aggregate_responses_digest(inputs.iter().map(|i| i.response)));
    hash.finalize().into()
}
fn source_input(settings: &SourceSettings, record: SourceRecord, file: File) -> Result<Input> {
    Ok(Input {
        id: settings.id.clone(),
        source: settings.identity()?,
        response: settings.response()?,
        record,
        file,
    })
}
fn limits(settings: &Settings, retained: usize) -> Result<Limits> {
    Ok(Limits {
        max_rules: settings.max_rules,
        max_memory_bytes: settings.max_memory_bytes,
        retained_bytes: retained
            .checked_add(COORDINATOR_BYTES)
            .context(Failure::new("subscription_memory_limit"))?,
    })
}
fn input_limits(inputs: &[Input], local: &Policy, settings: &Settings) -> Result<()> {
    let bytes = inputs
        .iter()
        .try_fold(local.input_bytes() as u64, |n, i| {
            n.checked_add(i.record.bytes)
        })
        .context(Failure::new("subscription_memory_limit"))?;
    let rules = inputs
        .iter()
        .try_fold(local.input_rules() as u64, |n, i| {
            n.checked_add(i.record.rules)
        })
        .context(Failure::new("subscription_memory_limit"))?;
    ensure!(
        bytes <= TOTAL_TEXT && rules <= settings.max_rules as u64,
        Failure::new("subscription_memory_limit")
    );
    Ok(())
}
fn compile_inputs(
    inputs: Vec<Input>,
    local: &Policy,
    limits: Limits,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Policy> {
    let mut builder = Builder::new(limits)?;
    local.append_to(&mut builder)?;
    let responses: Vec<_> = inputs.iter().map(|i| i.response).collect();
    builder.set_classes(local.aggregate_classes(responses.iter().copied()));
    let mut ids = local.aggregate_ids([]);
    for input in inputs {
        check()?;
        let parsed = builder.parse_source(
            CheckedReader {
                reader: BufReader::with_capacity(IO_BYTES, input.file),
                check: &mut check,
            },
            input.source.format,
            ids.len() as u16,
        );
        check()?;
        let stats = parsed?;
        ensure!(
            stats.input_rules as u64 == input.record.rules
                && stats.decoded_bytes as u64 == input.record.bytes,
            Failure::new("subscription_material_missing")
        );
        ids.push(input.id);
    }
    let mut cancelled = || {
        check().map_err(|_| ParseError {
            line: 0,
            kind: ErrorKind::Cancelled,
        })
    };
    let index = builder
        .prepare_with_check(&mut cancelled)?
        .finish_radix_with_check(cancelled)?;
    Ok(local.aggregate(index, ids, responses))
}

pub(super) fn local_material(local: &Policy, settings: &Settings) -> Result<Material> {
    input_limits(&[], local, settings)?;
    ensure!(
        local.owned_bytes() <= settings.max_memory_bytes,
        Failure::new("subscription_memory_limit")
    );
    Ok(Material {
        policy: local.clone(),
        material_digest: digest(local, &[]),
        index_pin: None,
        content_revision: 0,
    })
}

pub(super) fn identity(store: &Store, settings: &Settings, local: &Policy) -> Result<[u8; 32]> {
    let mut inputs = Vec::new();
    for source in settings.effective() {
        let (record, file) = store
            .verified_content(&source.identity()?.fingerprint)
            .map_err(|_| Failure::new("subscription_material_missing"))?;
        inputs.push(source_input(source, record, file)?);
    }
    input_limits(&inputs, local, settings)?;
    Ok(digest(local, &inputs))
}

pub(super) fn compile(
    store: &mut Store,
    settings: &Settings,
    local: &Policy,
    replacements: &[SourceRecord],
    retained: usize,
    persist: bool,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Material> {
    check()?;
    if settings.effective().next().is_none() {
        ensure!(
            retained <= settings.max_memory_bytes,
            Failure::new("subscription_memory_limit")
        );
        return local_material(local, settings);
    }
    ensure!(
        retained
            .checked_add(COORDINATOR_BYTES)
            .is_some_and(|n| n <= settings.max_memory_bytes),
        Failure::new("subscription_memory_limit")
    );
    let sources: Vec<_> = settings.effective().collect();
    let mut inputs = Vec::with_capacity(sources.len());
    let mut source_pins = Vec::with_capacity(sources.len());
    for settings in sources {
        check()?;
        let source = settings.identity()?;
        let record = replacements
            .iter()
            .find(|r| r.fingerprint == source.fingerprint)
            .or_else(|| store.record(&source.fingerprint))
            .cloned()
            .context(Failure::new("subscription_material_missing"))?;
        let file = store
            .verified_record(&record)
            .map_err(|_| Failure::new("subscription_material_missing"))?;
        source_pins.push(store.pin(&record.sha256)?);
        inputs.push(source_input(settings, record, file)?);
    }
    input_limits(&inputs, local, settings)?;
    let input_digest = digest(local, &inputs);
    let index_digest = index_digest(local, &inputs);
    let name = hex(index_digest);
    let ids = local.aggregate_ids(inputs.iter().map(|i| i.id.clone()));
    let responses: Vec<_> = inputs.iter().map(|i| i.response).collect();
    let budget = limits(settings, retained)?;
    if let Ok(Some(file)) = store.read_index(&name, settings.max_memory_bytes as u64)
        && let Ok(index) = Index::read_from_with_check(file, index_digest, &ids, budget, &mut check)
    {
        return Ok(Material {
            policy: local.aggregate(index, ids, responses),
            material_digest: input_digest,
            index_pin: Some(store.pin_index(&name)?),
            content_revision: store.revision(),
        });
    }
    let policy = compile_inputs(inputs, local, budget, &mut check)?;
    check()?;
    if !persist {
        return Ok(Material {
            policy,
            material_digest: input_digest,
            index_pin: None,
            content_revision: store.revision(),
        });
    }
    let max_bytes = (policy.index_bytes() as u64)
        .checked_add(4096)
        .context(Failure::new("subscription_disk_limit"))?;
    store.write_index(&name, max_bytes, |out| {
        policy.write_index(out, index_digest)
    })?;
    let index_pin = Some(store.pin_index(&name)?);
    Ok(Material {
        policy,
        material_digest: input_digest,
        index_pin,
        content_revision: store.revision(),
    })
}

/// What the selected sources read, as persisted: each enabled source's ID and
/// stored content SHA-256, and the catalog content revision. Unlike material
/// digests, nothing here depends on how this version encodes policy.
pub fn selected_content(path: &Path, settings: &Settings) -> Result<(Vec<(String, String)>, u64)> {
    if settings.effective().next().is_none() {
        return Ok((Vec::new(), 0));
    }
    let view = Store::read_only(path)?;
    let mut sources = Vec::new();
    for source in settings.effective() {
        let record = view
            .record(&source.identity()?.fingerprint)
            .context(Failure::new("subscription_material_missing"))?;
        sources.push((source.id.clone(), record.sha256.clone()));
    }
    Ok((sources, view.revision()))
}

/// Full material preflight, without acquiring a writer or writing any derived data.
pub fn read_only(path: &Path, settings: &Settings, local: &Policy) -> Result<Material> {
    if settings.effective().next().is_none() {
        return local_material(local, settings);
    }
    ensure!(
        local
            .owned_bytes()
            .checked_add(COORDINATOR_BYTES)
            .is_some_and(|n| n <= settings.max_memory_bytes),
        Failure::new("subscription_memory_limit")
    );
    let view = Store::read_only(path).map_err(|_| {
        Failure::new(if !path.exists() {
            "subscription_material_missing"
        } else {
            "subscription_storage_unavailable"
        })
    })?;
    let mut inputs = Vec::new();
    for source in settings.effective() {
        let (record, file) = view
            .verified_content(&source.identity()?.fingerprint)
            .map_err(|_| Failure::new("subscription_material_missing"))?;
        inputs.push(source_input(source, record, file)?);
    }
    input_limits(&inputs, local, settings)?;
    let input_digest = digest(local, &inputs);
    let index_digest = index_digest(local, &inputs);
    let ids = local.aggregate_ids(inputs.iter().map(|i| i.id.clone()));
    let responses: Vec<_> = inputs.iter().map(|i| i.response).collect();
    let budget = limits(settings, local.owned_bytes())?;
    if let Ok(Some(file)) = view.read_index(&hex(index_digest), settings.max_memory_bytes as u64)
        && let Ok(index) = Index::read_from(file, index_digest, &ids, budget)
    {
        return Ok(Material {
            policy: local.aggregate(index, ids, responses),
            material_digest: input_digest,
            index_pin: None,
            content_revision: view.revision(),
        });
    }
    Ok(Material {
        policy: compile_inputs(inputs, local, budget, || Ok(()))?,
        material_digest: input_digest,
        index_pin: None,
        content_revision: view.revision(),
    })
}
fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) struct CheckedReader<R, F> {
    pub reader: R,
    pub check: F,
}
impl<R: std::io::BufRead, F: FnMut() -> Result<()>> std::io::Read for CheckedReader<R, F> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        (self.check)().map_err(|_| std::io::Error::other("cancelled"))?;
        self.reader.read(out)
    }
}
impl<R: std::io::BufRead, F: FnMut() -> Result<()>> std::io::BufRead for CheckedReader<R, F> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        (self.check)().map_err(|_| std::io::Error::other("cancelled"))?;
        self.reader.fill_buf()
    }
    fn consume(&mut self, amount: usize) {
        self.reader.consume(amount)
    }
}
