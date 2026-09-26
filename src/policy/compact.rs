//! FS compact-index prototype. Physical layouts stay test/benchmark-only.
pub use super::canonical::*;
use super::canonical::{Key, error, grow, key};
use hickory_proto::rr::Name;
use std::mem::size_of;
#[path = "radix.rs"]
pub mod radix;

impl Key {
    fn query(name: &Name) -> Self {
        let mut key = Self {
            bytes: [0; 255],
            len: 0,
        };
        for label in name.iter().rev() {
            key.push(label);
        }
        key
    }
}
fn suffix_match(arena: &[u8], table: &[Entry], query: &[u8]) -> Option<Entry> {
    let index = table.partition_point(|entry| key(arena, *entry) <= query);
    index
        .checked_sub(1)
        .map(|index| table[index])
        .filter(|entry| query.starts_with(key(arena, *entry)))
}

impl Builder {
    pub fn finish(self) -> Result<Index, Error> {
        self.prepare()?.finish()
    }
}
impl Canonical {
    pub fn finish(mut self) -> Result<Index, Error> {
        let mut arena = Vec::new();
        let mut tables: [Vec<Entry>; 4] = std::array::from_fn(|_| Vec::new());
        let bytes = self
            .records
            .iter()
            .map(|record| usize::from(record.entry.len))
            .try_fold(0usize, usize::checked_add)
            .ok_or(error(ErrorKind::MemoryLimit))?;
        grow(&mut arena, bytes, &mut self.budget)?;
        for (group, table) in tables.iter_mut().enumerate() {
            let start = self
                .records
                .partition_point(|record| (record.group as usize) < group);
            let end = self
                .records
                .partition_point(|record| (record.group as usize) <= group);
            grow(table, end - start, &mut self.budget)?;
            for record in &self.records[start..end] {
                table.push(Entry {
                    offset: arena.len() as u32,
                    ..record.entry
                });
                arena.extend_from_slice(key(&self.arena, record.entry));
            }
        }
        Ok(Index {
            arena,
            tables,
            semantic_digest: self.semantic_digest,
            input_rules: self.input_rules,
            peak_bytes: self.budget.peak,
        })
    }
    pub fn finish_radix(self) -> Result<radix::Index, Error> {
        radix::Index::build(self)
    }
}

pub struct Index {
    arena: Vec<u8>,
    tables: [Vec<Entry>; 4],
    pub semantic_digest: [u8; 32],
    pub input_rules: usize,
    pub peak_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub group: Group,
    pub entry: Entry,
}
impl Index {
    pub fn lookup(&self, name: &Name) -> Option<Match> {
        let encoded = Key::query(name);
        for group in [
            Group::AllowSuffix,
            Group::AllowExact,
            Group::BlockSuffix,
            Group::BlockExact,
        ] {
            let table = &self.tables[group as usize];
            let found = if (group as usize).is_multiple_of(2) {
                suffix_match(&self.arena, table, encoded.as_slice())
            } else {
                table
                    .binary_search_by(|entry| key(&self.arena, *entry).cmp(encoded.as_slice()))
                    .ok()
                    .map(|i| table[i])
            };
            if let Some(entry) = found {
                return Some(Match { group, entry });
            }
        }
        None
    }
    pub fn index_rules(&self) -> usize {
        self.tables.iter().map(Vec::len).sum()
    }
    pub fn index_bytes(&self) -> usize {
        self.arena.capacity()
            + self
                .tables
                .iter()
                .map(|table| table.capacity() * size_of::<Entry>())
                .sum::<usize>()
    }
    pub fn rule(&self, entry: Entry) -> String {
        let mut labels = Vec::new();
        let mut bytes = key(&self.arena, entry);
        while let Some((&len, rest)) = bytes.split_first() {
            labels.push(&rest[..len as usize]);
            bytes = &rest[len as usize..];
        }
        labels.reverse();
        labels
            .iter()
            .map(|label| std::str::from_utf8(label).expect("ASCII rule"))
            .collect::<Vec<_>>()
            .join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prune_and_match() {
        let mut build = Builder::new(Limits::default()).unwrap();
        for (domain, group, source) in [
            ("EXAMPLE.com.", Group::BlockSuffix, 2),
            ("a.example.com", Group::BlockSuffix, 0),
            ("example.com", Group::BlockSuffix, 1),
            ("b.example.com", Group::BlockExact, 0),
            ("safe.example.com", Group::AllowExact, 0),
        ] {
            build.add(domain, group, source).unwrap();
        }
        let index = build.finish().unwrap();
        assert_eq!(index.input_rules, 5);
        assert_eq!(index.index_rules(), 2);
        assert_eq!(
            index
                .lookup(&Name::from_ascii("x.example.com").unwrap())
                .unwrap()
                .entry
                .source_slot,
            1
        );
        assert_eq!(
            index
                .lookup(&Name::from_ascii("safe.example.com").unwrap())
                .unwrap()
                .group,
            Group::AllowExact
        );
        assert!(
            index
                .lookup(&Name::from_ascii("badexample.com").unwrap())
                .is_none()
        );
        assert_eq!(
            index.rule(
                index
                    .lookup(&Name::from_ascii("example.com").unwrap())
                    .unwrap()
                    .entry
            ),
            "example.com"
        );
        assert!(index.peak_bytes > index.index_bytes());
    }
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod regression_tests;
