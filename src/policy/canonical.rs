//! Bounded streaming rule parser for local and subscription rules. It produces
//! canonical logical rules, which `radix` builds into the lookup index.
use hickory_proto::rr::Name;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::BufRead, mem::size_of, net::IpAddr};

/// Included in source fingerprints; not an index serialization version.
pub const PARSER_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidDomain,
    InvalidSource,
    InvalidLimits,
    MemoryLimit,
    RuleLimit,
    Allocation,
    InputLimit,
    LineLimit,
    InvalidUtf8,
    UnsupportedSyntax,
    HostsRewrite,
    EmptySource,
    Io,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub line: u64,
    pub kind: ErrorKind,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "filter source line {}: {:?}", self.line, self.kind)
    }
}
impl std::error::Error for Error {}

pub(super) fn error(kind: ErrorKind) -> Error {
    Error { line: 0, kind }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Group {
    AllowSuffix,
    AllowExact,
    BlockSuffix,
    BlockExact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Entry {
    pub offset: u32,
    pub len: u16,
    pub source_slot: u16,
}
const _: () = assert!(size_of::<Entry>() == 8);

#[derive(Clone, Copy)]
pub(super) struct Record {
    pub(super) entry: Entry,
    pub(super) group: Group,
    _padding: [u8; 7],
}
const _: () = assert!(size_of::<Record>() == 16);

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_rules: usize,
    pub max_memory_bytes: usize,
    /// Active, retired, local-base and caller-owned IO capacity, not RSS.
    pub retained_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rules: 1_000_000,
            max_memory_bytes: 128 * 1024 * 1024,
            retained_bytes: 0,
        }
    }
}

pub(super) struct Budget {
    pub(super) live: usize,
    pub(super) peak: usize,
    pub(super) limit: usize,
}

impl Budget {
    pub(super) fn reserve(&mut self, bytes: usize) -> Result<(), Error> {
        let next = self
            .live
            .checked_add(bytes)
            .ok_or(error(ErrorKind::MemoryLimit))?;
        if next > self.limit {
            return Err(error(ErrorKind::MemoryLimit));
        }
        self.live = next;
        self.peak = self.peak.max(next);
        Ok(())
    }
    pub(super) fn release(&mut self, bytes: usize) {
        self.live -= bytes;
    }
}

/// Allocate a separate explicitly-sized buffer: the old and new capacities are
/// charged together *before* allocation, including growth that will later shrink.
pub(super) fn grow<T>(
    buffer: &mut Vec<T>,
    needed: usize,
    budget: &mut Budget,
) -> Result<(), Error> {
    if needed <= buffer.capacity() {
        return Ok(());
    }
    let target = needed.max(
        buffer
            .capacity()
            .checked_add(buffer.capacity() / 2)
            .ok_or(error(ErrorKind::MemoryLimit))?,
    );
    let bytes = target
        .checked_mul(size_of::<T>())
        .ok_or(error(ErrorKind::MemoryLimit))?;
    budget.reserve(bytes)?;
    let mut next = Vec::new();
    if next.try_reserve_exact(target).is_err() {
        budget.release(bytes);
        return Err(error(ErrorKind::Allocation));
    }
    let actual = next
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(error(ErrorKind::MemoryLimit))?;
    if actual > bytes && budget.reserve(actual - bytes).is_err() {
        budget.release(bytes);
        return Err(error(ErrorKind::MemoryLimit));
    }
    next.append(buffer);
    let old = buffer.capacity() * size_of::<T>();
    *buffer = next;
    budget.release(old);
    Ok(())
}

#[derive(Clone)]
pub(super) struct Key {
    pub(super) bytes: [u8; 255],
    pub(super) len: usize,
}
impl Key {
    pub(super) fn query(name: &Name) -> Self {
        let mut key = Self {
            bytes: [0; 255],
            len: 0,
        };
        for label in name.iter().rev() {
            key.push(label);
        }
        key
    }
    pub(super) fn rule(text: &str) -> Result<Self, Error> {
        let text = text.strip_suffix('.').unwrap_or(text);
        if text.is_empty() || text.len() > 253 {
            return Err(error(ErrorKind::InvalidDomain));
        }
        let mut key = Self {
            bytes: [0; 255],
            len: 0,
        };
        for label in text.rsplit('.') {
            if label.is_empty()
                || label.len() > 63
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(error(ErrorKind::InvalidDomain));
            }
            key.push(label.as_bytes());
        }
        Ok(key)
    }
    pub(super) fn push(&mut self, label: &[u8]) {
        self.bytes[self.len] = label.len() as u8;
        self.len += 1;
        for &byte in label {
            self.bytes[self.len] = byte.to_ascii_lowercase();
            self.len += 1;
        }
    }
    pub(super) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The longest kept suffix rule covering `bytes`, searched at each label boundary.
fn longest_cover(arena: &[u8], suffixes: &[Record], bytes: &[u8]) -> Option<Entry> {
    let mut boundaries = [0usize; 128];
    let (mut count, mut position) = (0, 0);
    while position < bytes.len() {
        position += 1 + usize::from(bytes[position]);
        boundaries[count] = position;
        count += 1;
    }
    boundaries[..count].iter().rev().find_map(|&end| {
        let prefix = &bytes[..end];
        suffixes
            .binary_search_by(|record| key(arena, record.entry).cmp(prefix))
            .ok()
            .map(|index| suffixes[index].entry)
    })
}

pub(super) fn key(arena: &[u8], entry: Entry) -> &[u8] {
    &arena[entry.offset as usize..entry.offset as usize + entry.len as usize]
}
pub struct Builder {
    pub(super) arena: Vec<u8>,
    pub(super) records: Vec<Record>,
    pub(super) budget: Budget,
    /// Slots below this are local rules, which outrank subscription rules.
    pub(super) local_slots: u16,
    /// Answer class of each slot: slots with one class answer alike. Empty
    /// means every slot answers alike.
    pub(super) classes: Vec<u16>,
    max_rules: usize,
    decoded_bytes: usize,
    failed: Option<Error>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    DomainList,
    HostsBlocklist,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceStats {
    pub lines: u64,
    pub input_rules: usize,
    pub decoded_bytes: usize,
    /// Exact repeated (kind, normalized domain) rules within this source.
    pub duplicates: usize,
}
impl Builder {
    /// Local text participates in the aggregate decoded-input budget too.
    pub(crate) fn account_local_text(&mut self, bytes: usize) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self
            .decoded_bytes
            .checked_add(bytes)
            .filter(|n| *n <= 32 * 1024 * 1024)
        {
            Some(total) => {
                self.decoded_bytes = total;
                Ok(())
            }
            None => {
                let error = error(ErrorKind::InputLimit);
                self.failed = Some(error);
                Err(error)
            }
        }
    }
    pub fn new(limits: Limits) -> Result<Self, Error> {
        if limits.max_rules == 0
            || limits.max_rules > 5_000_000
            || limits.max_memory_bytes == 0
            || limits.max_memory_bytes > 512 * 1024 * 1024
        {
            return Err(error(ErrorKind::InvalidLimits));
        }
        let mut budget = Budget {
            live: 0,
            peak: 0,
            limit: limits.max_memory_bytes,
        };
        budget.reserve(limits.retained_bytes)?;
        Ok(Self {
            arena: Vec::new(),
            records: Vec::new(),
            budget,
            local_slots: 1,
            classes: Vec::new(),
            max_rules: limits.max_rules,
            decoded_bytes: 0,
            failed: None,
        })
    }
    /// Answer class of each slot, from `Policy::aggregate_classes`.
    pub(crate) fn set_classes(&mut self, classes: Vec<u16>) {
        self.classes = classes;
    }
    /// Slots are assigned by the caller: local first, then configured source order.
    pub fn add(&mut self, domain: &str, group: Group, source_slot: u16) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.add_inner(domain, group, source_slot);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn add_inner(&mut self, domain: &str, group: Group, source_slot: u16) -> Result<(), Error> {
        if source_slot == u16::MAX {
            return Err(error(ErrorKind::InvalidSource));
        }
        let encoded = Key::rule(domain)?;
        if self.records.len() >= self.max_rules {
            return Err(error(ErrorKind::RuleLimit));
        }
        let end = self
            .arena
            .len()
            .checked_add(encoded.len)
            .filter(|n| *n <= u32::MAX as usize)
            .ok_or(error(ErrorKind::MemoryLimit))?;
        grow(&mut self.arena, end, &mut self.budget)?;
        let records_len = self.records.len() + 1;
        grow(&mut self.records, records_len, &mut self.budget)?;
        let entry = Entry {
            offset: self.arena.len() as u32,
            len: encoded.len as u16,
            source_slot,
        };
        self.arena.extend_from_slice(encoded.as_slice());
        self.records.push(Record {
            entry,
            group,
            _padding: [0; 7],
        });
        Ok(())
    }
    #[cfg(test)]
    pub fn prepare(self) -> Result<Canonical, Error> {
        self.prepare_with_check(|| Ok(()))
    }
    pub fn prepare_with_check(
        mut self,
        mut check: impl FnMut() -> Result<(), Error>,
    ) -> Result<Canonical, Error> {
        check()?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let input_rules = self.records.len();
        let arena = &self.arena;
        self.records.sort_unstable_by(|a, b| {
            (a.group as u8)
                .cmp(&(b.group as u8))
                .then_with(|| key(arena, a.entry).cmp(key(arena, b.entry)))
                .then(a.entry.source_slot.cmp(&b.entry.source_slot))
        });
        // Sorting is non-preemptible; its owner retains the memory lease until
        // this worker really exits, including when its caller is cancelled.
        check()?;
        let mut ranges = [0..0, 0..0, 0..0, 0..0];
        let mut read = 0;
        let mut write = 0;
        let local_slots = self.local_slots;
        let classes = &self.classes;
        let uniform = classes.windows(2).all(|pair| pair[0] == pair[1]);
        let class = |slot: u16| classes.get(usize::from(slot)).copied().unwrap_or(0);
        // A covered block rule is dropped only when it cannot change the answer:
        // every slot answers alike, a local rule covers a subscription rule, or
        // the covering rule (the longest such suffix) has its class and layer.
        let covers = |group: usize, cover: Entry, entry: Entry| {
            let local = |entry: Entry| entry.source_slot < local_slots;
            group < 2
                || uniform
                || (local(cover) && !local(entry))
                || (class(cover.source_slot) == class(entry.source_slot)
                    && local(cover) == local(entry))
        };
        for group in 0..4 {
            // Kept suffixes that cover the current key, longest last. Keys are
            // sorted, so an entry stops covering once a later key leaves it.
            let mut ancestors = [None::<Entry>; 128];
            let mut depth = 0;
            // Only the lowest slot of an identical key survives, kept or not.
            let mut last: Option<Entry> = None;
            let start = write;
            while read < self.records.len() && self.records[read].group as usize == group {
                if read.is_multiple_of(4096) {
                    check()?;
                }
                let record = self.records[read];
                read += 1;
                let entry = record.entry;
                let bytes = key(&self.arena, entry);
                let duplicate = last.is_some_and(|last| bytes == key(&self.arena, last));
                last = Some(entry);
                let cover = if group % 2 == 0 {
                    while depth > 0
                        && !bytes.starts_with(key(
                            &self.arena,
                            ancestors[depth - 1].expect("pushed ancestor"),
                        ))
                    {
                        depth -= 1;
                    }
                    depth.checked_sub(1).and_then(|top| ancestors[top])
                } else {
                    longest_cover(&self.arena, &self.records[ranges[group - 1].clone()], bytes)
                };
                let redundant = duplicate || cover.is_some_and(|cover| covers(group, cover, entry));
                if !redundant && group % 2 == 0 {
                    // A label-aligned key nests at most 127 suffixes deep.
                    ancestors[depth] = Some(entry);
                    depth += 1;
                }
                if !redundant {
                    self.records[write] = record;
                    write += 1;
                }
            }
            ranges[group] = start..write;
        }
        self.records.truncate(write);
        let mut hash = Sha256::new();
        hash.update(b"parins-policy-semantics-v1\0");
        for (group, range) in ranges.iter().enumerate() {
            hash.update([group as u8]);
            hash.update((range.len() as u64).to_be_bytes());
            for record in &self.records[range.clone()] {
                hash.update(record.entry.len.to_be_bytes());
                hash.update(key(&self.arena, record.entry));
            }
        }
        check()?;
        Ok(Canonical {
            arena: self.arena,
            records: self.records,
            budget: self.budget,
            semantic_digest: hash.finalize().into(),
            input_rules,
        })
    }

    /// Reads one bounded line at a time; caller accounts for its BufRead capacity
    /// in retained_bytes. Errors poison this candidate; it cannot be published.
    pub fn parse_source(
        &mut self,
        mut reader: impl BufRead,
        format: Format,
        source_slot: u16,
    ) -> Result<SourceStats, Error> {
        const MAX_SOURCE: usize = 16 * 1024 * 1024;
        const MAX_AGGREGATE: usize = 32 * 1024 * 1024;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Err(error) = self.budget.reserve(4096) {
            self.failed = Some(error);
            return Err(error);
        }
        let result = (|| {
            let mut stats = SourceStats::default();
            let initial_rules = self.records.len();
            let mut line = [0u8; 4096];
            let mut used = 0;
            loop {
                let chunk = reader.fill_buf().map_err(|_| Error {
                    line: stats.lines + 1,
                    kind: ErrorKind::Io,
                })?;
                if chunk.is_empty() {
                    if used > 0 {
                        stats.lines += 1;
                        self.parse_line(&line[..used], stats.lines, format, source_slot)?;
                    }
                    break;
                }
                let newline = chunk.iter().position(|&byte| byte == b'\n');
                let length = newline.unwrap_or(chunk.len());
                let consumed = length + usize::from(newline.is_some());
                stats.decoded_bytes = stats
                    .decoded_bytes
                    .checked_add(consumed)
                    .filter(|n| *n <= MAX_SOURCE)
                    .ok_or(Error {
                        line: stats.lines + 1,
                        kind: ErrorKind::InputLimit,
                    })?;
                self.decoded_bytes = self
                    .decoded_bytes
                    .checked_add(consumed)
                    .filter(|n| *n <= MAX_AGGREGATE)
                    .ok_or(Error {
                        line: stats.lines + 1,
                        kind: ErrorKind::InputLimit,
                    })?;
                let end = used
                    .checked_add(length)
                    .filter(|n| *n <= line.len())
                    .ok_or(Error {
                        line: stats.lines + 1,
                        kind: ErrorKind::LineLimit,
                    })?;
                line[used..end].copy_from_slice(&chunk[..length]);
                used = end;
                reader.consume(consumed);
                if newline.is_some() {
                    stats.lines += 1;
                    self.parse_line(&line[..used], stats.lines, format, source_slot)?;
                    used = 0;
                }
            }
            stats.input_rules = self.records.len() - initial_rules;
            if stats.input_rules == 0 {
                return Err(error(ErrorKind::EmptySource));
            }
            let records = &mut self.records[initial_rules..];
            records.sort_unstable_by(|a, b| {
                (a.group as u8)
                    .cmp(&(b.group as u8))
                    .then_with(|| key(&self.arena, a.entry).cmp(key(&self.arena, b.entry)))
            });
            stats.duplicates = duplicate_count(records, &self.arena);
            Ok(stats)
        })();
        self.budget.release(4096);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn parse_line(
        &mut self,
        bytes: &[u8],
        line: u64,
        format: Format,
        slot: u16,
    ) -> Result<(), Error> {
        let bytes = if line == 1 {
            bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
        } else {
            bytes
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error {
                line,
                kind: ErrorKind::InvalidUtf8,
            })?
            .trim_ascii();
        if text.is_empty() || text.starts_with(['#', '!']) {
            return Ok(());
        }
        let comment = text
            .as_bytes()
            .windows(2)
            .position(|pair| pair[0].is_ascii_whitespace() && pair[1] == b'#');
        let text = comment.map_or(text, |i| &text[..i]);
        let result = match format {
            Format::DomainList => {
                let mut tokens = text.split_ascii_whitespace();
                let token = tokens.next().expect("nonempty line");
                if tokens.next().is_some() {
                    Err(error(ErrorKind::UnsupportedSyntax))
                } else {
                    let (domain, group) = token
                        .strip_prefix('.')
                        .map_or((token, Group::BlockExact), |name| {
                            (name, Group::BlockSuffix)
                        });
                    self.add(domain, group, slot)
                }
            }
            Format::HostsBlocklist => {
                let mut tokens = text.split_ascii_whitespace();
                let address = tokens
                    .next()
                    .expect("nonempty line")
                    .parse::<IpAddr>()
                    .map_err(|_| Error {
                        line,
                        kind: ErrorKind::UnsupportedSyntax,
                    })?;
                let sinkhole = match address {
                    IpAddr::V4(ip) => ip.is_unspecified() || ip.octets() == [127, 0, 0, 1],
                    IpAddr::V6(ip) => ip.is_unspecified() || ip.is_loopback(),
                };
                if !sinkhole {
                    return Err(Error {
                        line,
                        kind: ErrorKind::HostsRewrite,
                    });
                }
                let mut count = 0;
                for token in tokens {
                    self.add(token, Group::BlockExact, slot)
                        .map_err(|error| Error { line, ..error })?;
                    count += 1;
                }
                if count == 0 {
                    Err(error(ErrorKind::UnsupportedSyntax))
                } else {
                    Ok(())
                }
            }
        };
        result.map_err(|error| Error { line, ..error })
    }
}

fn duplicate_count(records: &[Record], arena: &[u8]) -> usize {
    records
        .windows(2)
        .filter(|pair| {
            pair[0].group == pair[1].group && key(arena, pair[0].entry) == key(arena, pair[1].entry)
        })
        .count()
}

/// Canonical logical rules, the input of `radix::Index::build`.
pub struct Canonical {
    pub(super) arena: Vec<u8>,
    pub(super) records: Vec<Record>,
    pub(super) budget: Budget,
    pub semantic_digest: [u8; 32],
    pub input_rules: usize,
}
impl Canonical {
    #[cfg(test)]
    pub fn finish_radix(self) -> Result<super::radix::Index, Error> {
        super::radix::Index::build(self)
    }
    pub fn finish_radix_with_check(
        self,
        check: impl FnMut() -> Result<(), Error>,
    ) -> Result<super::radix::Index, Error> {
        super::radix::Index::build_with_check(self, check)
    }
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
